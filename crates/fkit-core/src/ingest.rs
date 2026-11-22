//! Turning a working directory into a Merkle DAG.
//!
//! Two jobs:
//!   * [`ingest_file`] — bytes -> chunks -> a balanced Merkle tree over them
//!   * [`ingest_dir`]  — a directory -> a `Tree` object referencing its children
//!
//! Both are *pure* in the sense that matters: the same input always produces
//! the same root hash, on any machine, in any order.

use crate::hash::Hash;
use crate::chunker::Chunker;
use crate::object::{EntryKind, Object, TreeChild, TreeEntry};
use crate::store::{Sink, Store, WriteStats};
use anyhow::{Context, Result};
use std::fs;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How many children a `FileNode` may hold before we add a level above it.
///
/// With ~8 KiB chunks: one level-0 node covers ~2 MB, level-1 ~512 MB,
/// level-2 ~128 GB. So three levels handle any file anyone will ever commit,
/// and no single object exceeds ~10 KB on disk.
pub const FANOUT: usize = 256;

/// Directory-entry run sizes, the tree-level counterpart of the byte chunker.
///
/// Most directories hold fewer than `MIN_RUN` entries and become a single run,
/// so nothing is paid for the common case. The bounds only matter for the
/// directories that actually hurt: `node_modules`, a flat asset folder, a
/// dataset of 100 000 files.
pub const MIN_RUN: usize = 32;
pub const MAX_RUN: usize = 1024;
/// Boundary probability per entry: 1 in 2^8, i.e. runs average ~256 entries.
const RUN_MASK: u64 = 0xFF;

pub struct Ingested {
    pub hash: Hash,
    pub size: u64,
    pub stats: WriteStats,
    /// Paths the working tree changed underneath the ingest. Always empty for
    /// a single file; see [`Raced`].
    pub raced: Vec<Raced>,
    /// Lines in ignore files found below the root that could not be turned
    /// into rules. The root's own are in [`Ignore::problems`], which the
    /// caller had before the walk began.
    pub unusable: Vec<Problem>,
}

/// A path that did not hold still while the tree was being read.
///
/// A working tree is not a quiet place: builds, editors and test runs create
/// and delete files the whole time a snapshot is being taken. Listing the
/// tree and reading it are two moments, and a file can be gone by the second.
/// That is not an error -- there is nothing wrong with the repository, the
/// store, or the file that is no longer there -- so it does not fail the
/// snapshot. It is reported instead, because a commit that quietly left
/// something out is its own kind of lie.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Raced {
    /// Relative to the snapshot root.
    pub path: String,
    pub what: Race,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Race {
    /// Listed, then gone before it could be read. Left out of the snapshot,
    /// which is the truth: it is not there.
    Vanished,
    /// Still being written when it was read, twice. What the second read saw
    /// is stored, and may be a file caught halfway through a write.
    Changed,
}

/// Whether an error means "this path is not there any more".
///
/// `NotADirectory` is the same event seen from further down: a parent
/// directory was removed and something else took its name.
fn gone(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Chunk a file and build its Merkle tree bottom-up.
///
/// ```text
///   chunks:   c0 c1 c2 c3 ... c999          (content-defined, ~8 KiB each)
///                |
///   level 0:  [c0..c255] [c256..c511] ...   (FileNode, children = chunks)
///                |
///   level 1:  [n0 n1 n2 n3]                 (FileNode, children = FileNodes)
///                |
///   root:     hash of the level-1 node  <- this is the file's identity
/// ```
///
/// The root hash commits to every byte in the file. Two files with identical
/// contents get the same root hash even if they have different names, live in
/// different repos, or were committed by different people a decade apart.
/// What a path held in the commit this one is built on.
///
/// This is the whole reason fkit can patch without guessing. Git has to *hunt*
/// for something to delta against -- it sorts objects by type, size and a hash
/// of the path, then slides a window looking for a candidate, because a
/// packfile is a bag of blobs with no record of which are versions of which.
/// A tree already knows. When `src/foo.c` is ingested, the parent commit's tree
/// says exactly which chunks were there last time, in order, and the chunk at
/// position *i* is overwhelmingly the predecessor of the new chunk at position
/// *i*. No search, no heuristic, no window.
///
/// Lookups happen once per file and only for files being written, so there is
/// nothing to cache and nothing to invalidate.
pub struct Prior<'a> {
    store: Option<&'a Store>,
    root: Option<Hash>,
}

impl<'a> Prior<'a> {
    /// No previous version: every object is stored whole. What a first commit
    /// gets, and what every caller that has not been taught about this gets.
    pub fn none() -> Prior<'a> {
        Prior { store: None, root: None }
    }

    /// The tree of the commit being built on.
    pub fn at(store: &'a Store, root: Hash) -> Prior<'a> {
        Prior { store: Some(store), root: Some(root) }
    }

    /// The chunks that `rel` was made of last time, in order.
    ///
    /// Empty for a new file, a path that was a directory, or anything this
    /// cannot resolve -- all of which simply mean "store it whole", so no error
    /// is worth raising. A wrong answer here costs a few bytes, never
    /// correctness: the pack verifies that a patch is smaller before keeping
    /// it, and every reader verifies the hash after materializing.
    pub fn chunks_for(&self, rel: &str) -> Vec<Hash> {
        let (Some(store), Some(root)) = (self.store, self.root) else {
            return Vec::new();
        };
        let mut at = root;
        let mut parts = rel.split('/').peekable();
        while let Some(name) = parts.next() {
            let Ok(entries) = read_entries(store, at) else {
                return Vec::new();
            };
            let Some(e) = entries.into_iter().find(|e| e.name == name) else {
                return Vec::new();
            };
            if parts.peek().is_some() {
                if e.kind != EntryKind::Dir {
                    return Vec::new();
                }
                at = e.hash;
            } else {
                return match e.kind {
                    EntryKind::File { .. } | EntryKind::Symlink => leaves(store, e.hash),
                    _ => Vec::new(),
                };
            }
        }
        Vec::new()
    }
}

impl Prior<'_> {
    /// The `Entries` runs that directory held last time, in order.
    ///
    /// A directory listing is the thing that actually churns: change one file
    /// and every directory from it to the root is rewritten, even though a
    /// single 32-byte hash moved. Runs are cut content-defined, exactly like a
    /// file's chunks, so run *i* is the predecessor of run *i* for the same
    /// reason.
    ///
    /// `""` is the root.
    pub fn entries_for(&self, dir: &str) -> Vec<Hash> {
        let (Some(store), Some(root)) = (self.store, self.root) else {
            return Vec::new();
        };
        let mut at = root;
        if !dir.is_empty() {
            for name in dir.split('/') {
                let Ok(entries) = read_entries(store, at) else {
                    return Vec::new();
                };
                match entries.into_iter().find(|e| e.name == name) {
                    Some(e) if e.kind == EntryKind::Dir => at = e.hash,
                    _ => return Vec::new(),
                }
            }
        }
        runs_of(store, at)
    }
}

/// A tree's `Entries` objects, left to right.
fn runs_of(store: &Store, root: Hash) -> Vec<Hash> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(h) = stack.pop() {
        match store.get(h) {
            Ok(Object::Tree { level, children }) => {
                if level == 0 {
                    out.extend(children.into_iter().map(|c| c.hash));
                } else {
                    stack.extend(children.into_iter().rev().map(|c| c.hash));
                }
            }
            _ => return out,
        }
    }
    out
}

/// A file's chunk hashes, left to right.
///
/// Bounded by the same limit the rest of the walk uses: a file node cannot name
/// more children than fit in an object, so this cannot run away.
fn leaves(store: &Store, root: Hash) -> Vec<Hash> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(h) = stack.pop() {
        match store.get(h) {
            Ok(Object::File { level, children }) => {
                if level == 0 {
                    out.extend(children.into_iter().map(|(c, _)| c));
                } else {
                    // Reversed, because the stack pops last-in-first-out and
                    // the order of chunks is the whole point.
                    stack.extend(children.into_iter().rev().map(|(c, _)| c));
                }
            }
            _ => return out,
        }
    }
    out
}

pub fn ingest_file(sink: &Sink, path: &Path) -> Result<Ingested> {
    let f = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    ingest_reader(sink, std::io::BufReader::new(f))
}

pub fn ingest_bytes(sink: &Sink, bytes: &[u8]) -> Result<Ingested> {
    ingest_reader(sink, bytes)
}

pub fn ingest_reader<R: std::io::Read>(sink: &Sink, reader: R) -> Result<Ingested> {
    ingest_reader_after(sink, reader, &[])
}

/// Ingest, knowing what this file was made of last time.
///
/// `was` is the previous version's chunks in order. The chunk arriving at
/// position *i* is offered `was[i]` as something it may be a small edit away
/// from -- which is right far more often than not, because content-defined
/// boundaries only move where the content moved. Where it is wrong the pack
/// notices the patch is not smaller and stores the object whole, so a bad
/// guess costs a little work and never correctness.
pub fn ingest_reader_after<R: std::io::Read>(
    sink: &Sink,
    reader: R,
    was: &[Hash],
) -> Result<Ingested> {
    let mut stats = WriteStats::default();

    // Pass 1: cut into content-defined chunks and store each leaf.
    let mut level_refs: Vec<(Hash, u64)> = Vec::new();
    for (i, chunk) in Chunker::new(reader).enumerate() {
        let chunk = chunk?;
        let len = chunk.len() as u64;
        let (h, st) = sink.put_based(&Object::Chunk(chunk), was.get(i).copied())?;
        stats.merge(st);
        level_refs.push((h, len));
    }

    let total: u64 = level_refs.iter().map(|(_, n)| n).sum();

    // Pass 2: fold upward until a single root remains.
    let mut level: u8 = 0;
    loop {
        if level_refs.len() <= FANOUT {
            let (h, st) = sink.put(&Object::File {
                level,
                children: level_refs,
            })?;
            stats.merge(st);
            return Ok(Ingested { hash: h, size: total, stats, raced: Vec::new(), unusable: Vec::new() });
        }

        let mut next = Vec::with_capacity(level_refs.len() / FANOUT + 1);
        for group in level_refs.chunks(FANOUT) {
            let span: u64 = group.iter().map(|(_, n)| n).sum();
            let (h, st) = sink.put(&Object::File {
                level,
                children: group.to_vec(),
            })?;
            stats.merge(st);
            next.push((h, span));
        }
        level_refs = next;
        level += 1;
    }
}

/// Reassemble a file's bytes by walking its Merkle tree depth-first.
pub fn read_file(store: &Store, root: Hash, out: &mut impl std::io::Write) -> Result<()> {
    match store.get(root)? {
        Object::Chunk(data) => out.write_all(&data)?,
        Object::File { children, .. } => {
            for (child, _) in children {
                read_file(store, child, out)?;
            }
        }
        other => anyhow::bail!(
            "expected a file node, found a {}",
            other.kind().name()
        ),
    }
    Ok(())
}

/// Cut a sorted entry list into content-defined runs.
///
/// The boundary test hashes the *entry itself*, not its position, for exactly
/// the reason the byte chunker does: inserting a file into a large directory
/// must perturb one run, not renumber every boundary after it. A positional
/// split (every N entries) would rewrite the whole directory on any insertion
/// near the front, which is precisely git's behaviour.
fn cut_runs(entries: &[TreeEntry]) -> Vec<&[TreeEntry]> {
    if entries.len() <= MIN_RUN {
        return vec![entries];
    }
    let mut runs = Vec::new();
    let mut start = 0usize;

    for i in 0..entries.len() {
        let len = i - start + 1;
        if len < MIN_RUN {
            continue;
        }
        let e = &entries[i];
        let mut h = blake3::Hasher::new();
        h.update(e.name.as_bytes());
        h.update(&e.hash.0);
        let fp = u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap());

        if fp & RUN_MASK == 0 || len >= MAX_RUN {
            runs.push(&entries[start..=i]);
            start = i + 1;
        }
    }
    if start < entries.len() {
        runs.push(&entries[start..]);
    }
    runs
}

/// Build a directory's Merkle tree from its sorted entries.
///
/// Mirrors [`ingest_reader`]: cut into runs, store each run, then fold upward
/// with the same `FANOUT` until one root remains.
pub fn build_tree(sink: &Sink, entries: Vec<TreeEntry>) -> Result<(Hash, u64, u32)> {
    build_tree_after(sink, entries, &[])
}

/// Build a tree, knowing the runs the same directory was made of last time.
pub fn build_tree_after(
    sink: &Sink,
    entries: Vec<TreeEntry>,
    was: &[Hash],
) -> Result<(Hash, u64, u32)> {
    debug_assert!(
        entries.windows(2).all(|w| w[0].name <= w[1].name),
        "entries must be sorted before hashing"
    );

    let total_size: u64 = entries.iter().map(|e| e.size).sum();
    let total_entries = entries.len() as u32;

    let mut level_refs: Vec<TreeChild> = Vec::new();
    for (i, run) in cut_runs(&entries).into_iter().enumerate() {
        let size = run.iter().map(|e| e.size).sum();
        let (h, _) =
            sink.put_based(&Object::Entries(run.to_vec()), was.get(i).copied())?;
        level_refs.push(TreeChild { hash: h, entries: run.len() as u32, size });
    }

    let mut level: u8 = 0;
    loop {
        if level_refs.len() <= FANOUT {
            let (h, _) = sink.put(&Object::Tree { level, children: level_refs })?;
            return Ok((h, total_size, total_entries));
        }
        let mut next = Vec::with_capacity(level_refs.len() / FANOUT + 1);
        for group in level_refs.chunks(FANOUT) {
            let size = group.iter().map(|c| c.size).sum();
            let count = group.iter().map(|c| c.entries).sum();
            let (h, _) = sink.put(&Object::Tree { level, children: group.to_vec() })?;
            next.push(TreeChild { hash: h, entries: count, size });
        }
        level_refs = next;
        level += 1;
    }
}

/// Every entry of a directory, in name order, flattened across its runs.
pub fn read_entries(store: &Store, tree: Hash) -> Result<Vec<TreeEntry>> {
    let mut out = Vec::new();
    collect_entries(store, tree, &mut out)?;
    Ok(out)
}

fn collect_entries(store: &Store, node: Hash, out: &mut Vec<TreeEntry>) -> Result<()> {
    match store.get(node)? {
        Object::Entries(e) => out.extend(e),
        Object::Tree { children, .. } => {
            for c in children {
                collect_entries(store, c.hash, out)?;
            }
        }
        other => anyhow::bail!("expected a tree node, found a {}", other.kind().name()),
    }
    Ok(())
}

/// A directory tree as enumerated from disk, before any content is read.
///
/// Building this first is what makes parallelism possible: walking the
/// filesystem is cheap and inherently serial, while *reading and hashing* the
/// files is the expensive part and is embarrassingly parallel. Separating them
/// also keeps tree construction deterministic — entries are assembled in name
/// order regardless of which thread finished first.
struct Skeleton {
    /// Files and symlinks to ingest, in a stable order.
    jobs: Vec<Job>,
    /// Every directory, so empty ones survive the round trip.
    dirs: Vec<String>,
    /// Mounted submodule paths found during the walk. Recorded rather than
    /// descended into: their content is already named by the pin.
    subs: Vec<String>,
    /// Paths that were gone again before the walk could look at them.
    raced: Vec<Raced>,
    /// Ignore-file lines met on the way down that are not in effect.
    unusable: Vec<Problem>,
}

struct Job {
    /// Path relative to the snapshot root, used as the tree key.
    rel: String,
    abs: PathBuf,
    kind: JobKind,
}

enum JobKind {
    File { exec: bool },
    Symlink,
}

fn enumerate(root: &Path, ignore: &Ignore, mounts: &Mounts) -> Result<Skeleton> {
    let mut sk = Skeleton {
        jobs: Vec::new(),
        dirs: Vec::new(),
        subs: Vec::new(),
        raced: Vec::new(),
        unusable: Vec::new(),
    };
    walk(root, "", ignore, mounts, &mut sk)?;
    // Largest first: with a few very large files among many small ones, feeding
    // the big ones out first stops one thread finishing last on a 1 GB image
    // while the rest idle.
    sk.jobs.sort_by(|a, b| {
        let size = |j: &Job| fs::symlink_metadata(&j.abs).map(|m| m.len()).unwrap_or(0);
        size(b).cmp(&size(a))
    });
    Ok(sk)
}

fn walk(
    dir: &Path,
    prefix: &str,
    ignore: &Ignore,
    mounts: &Mounts,
    out: &mut Skeleton,
) -> Result<()> {
    let vanished = |path: &str| Raced { path: path.to_string(), what: Race::Vanished };

    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        // A directory its parent listed a moment ago. The root is not excused:
        // with no root there is nothing to snapshot, and that is an error.
        Err(e) if !prefix.is_empty() && gone(&e) => {
            out.raced.push(vanished(prefix));
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    // Recorded only once it has been opened, so a directory that vanished is
    // not written into the snapshot as an empty one.
    if !prefix.is_empty() {
        out.dirs.push(prefix.to_string());
    }

    // The whole listing first, because whether this directory has an ignore
    // file of its own has to be known before the first entry is judged.
    let entries = read
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("reading {}", dir.display()))?;

    // The root's files were read by `Ignore::load`. Anywhere else, a
    // directory with an ignore file gets its own rules on top of its
    // parent's, for itself and everything under it.
    let has_own = ignore.nested
        && !prefix.is_empty()
        && entries.iter().any(|e| IGNORE_FILES.iter().any(|f| e.file_name() == *f));
    let own;
    let ignore = if has_own {
        let (deeper, problems) = ignore.descend(dir, prefix);
        out.unusable.extend(problems);
        own = deeper;
        &own
    } else {
        ignore
    };

    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let abs = entry.path();

        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) if gone(&e) => {
                // Not worth a warning if it was never going to be included.
                if !ignore.matches(&rel, false) {
                    out.raced.push(vanished(&rel));
                }
                continue;
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", abs.display())),
        };
        if ignore.matches(&rel, ft.is_dir()) {
            continue;
        }

        if ft.is_symlink() {
            out.jobs.push(Job { rel, abs, kind: JobKind::Symlink });
        } else if ft.is_dir() {
            // A submodule is a boundary. Its files are already content in this
            // store, named by the pin, so walking into them would store a
            // second copy under this repository's own paths and quietly turn a
            // reference into a fork.
            if mounts.contains_key(&rel) {
                out.subs.push(rel);
                continue;
            }
            walk(&abs, &rel, ignore, mounts, out)?;
        } else {
            let exec = match entry.metadata() {
                Ok(m) => is_executable(&m),
                Err(e) if gone(&e) => {
                    out.raced.push(vanished(&rel));
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("reading {}", abs.display())),
            };
            out.jobs.push(Job { rel, abs, kind: JobKind::File { exec } });
        }
    }
    Ok(())
}

/// What one job came to.
struct Ran {
    /// `None` when the path was gone by the time it was this job's turn.
    entry: Option<TreeEntry>,
    stats: WriteStats,
    raced: Option<Race>,
}

/// Ingest one job's content. Runs on a worker thread.
///
/// The path was listed by [`walk`] some time ago -- seconds, on a large tree
/// -- and may not be there now. That is reported in [`Ran`], not as an error:
/// one file deleted by a build must not cost the other hundred thousand.
/// Errors are kept for what is actually wrong, like a file that exists and
/// cannot be read.
fn run_job(sink: &Sink, job: &Job, prior: &Prior) -> Result<Ran> {
    let name = job.rel.rsplit('/').next().unwrap_or(&job.rel).to_string();
    let vanished =
        || Ran { entry: None, stats: WriteStats::default(), raced: Some(Race::Vanished) };

    match job.kind {
        JobKind::Symlink => {
            // Store the link *target* as content. A symlink then costs one small
            // chunk rather than a copy of whatever it points at.
            let target = match fs::read_link(&job.abs) {
                Ok(t) => t,
                // InvalidInput: something is there, and it is not a link any
                // more. The link this job was for is gone all the same.
                Err(e) if gone(&e) || e.kind() == std::io::ErrorKind::InvalidInput => {
                    return Ok(vanished());
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("reading {}", job.abs.display()));
                }
            };
            let ing = ingest_bytes(sink, target.to_string_lossy().as_bytes())?;
            let entry =
                TreeEntry { name, kind: EntryKind::Symlink, hash: ing.hash, size: ing.size };
            Ok(Ran { entry: Some(entry), stats: ing.stats, raced: None })
        }
        JobKind::File { exec } => {
            // What this path was made of in the commit being built on, so
            // each chunk can be offered its own predecessor.
            let was = prior.chunks_for(&job.rel);
            let Some((ing, settled)) = read_settled(sink, &job.abs, &was)? else {
                return Ok(vanished());
            };
            let entry = TreeEntry {
                name,
                kind: EntryKind::File { exec },
                hash: ing.hash,
                size: ing.size,
            };
            Ok(Ran {
                entry: Some(entry),
                stats: ing.stats,
                raced: (!settled).then_some(Race::Changed),
            })
        }
    }
}

/// Read a file that something else may be writing.
///
/// Returns `None` if it is gone, and otherwise what was read along with
/// whether the file held still for it. Size and modification time are compared
/// across the read; if either moved the file is read once more, on the theory
/// that most writes are over in less time than that takes. A second miss is
/// kept and flagged rather than chased: a log being appended to never settles,
/// and a snapshot that waits for it never finishes.
///
/// The comparison can be fooled by a write that keeps the size and lands
/// inside the clock's resolution. Nothing here depends on it being right --
/// the stored bytes are exactly the bytes that were read, either way.
fn read_settled(sink: &Sink, path: &Path, was: &[Hash]) -> Result<Option<(Ingested, bool)>> {
    let reading = || format!("reading {}", path.display());
    let stamp = |m: &fs::Metadata| (m.len(), m.modified().ok());

    let mut stats = WriteStats::default();
    let mut retried = false;
    loop {
        let f = match fs::File::open(path) {
            Ok(f) => f,
            Err(e) if gone(&e) => return Ok(None),
            Err(e) => return Err(e).with_context(reading),
        };
        let before = f.metadata().with_context(reading)?;
        // Replaced by a directory since the walk. The file is gone; whatever
        // the directory holds belongs to the next snapshot.
        if before.is_dir() {
            return Ok(None);
        }

        let mut ing =
            ingest_reader_after(sink, std::io::BufReader::new(&f), was).with_context(reading)?;
        let after = f.metadata().with_context(reading)?;
        let settled = stamp(&before) == stamp(&after) && ing.size == after.len();

        // Both reads wrote to the store, so both are counted.
        stats.merge(ing.stats);
        if settled || retried {
            ing.stats = stats;
            return Ok(Some((ing, settled)));
        }
        retried = true;
    }
}

/// Snapshot a directory into a `Tree`, reading files on every available core.
/// Submodule pins, by path relative to the repository root.
pub type Mounts = std::collections::BTreeMap<String, Hash>;

pub fn ingest_dir(
    sink: &Sink,
    dir: &Path,
    ignore: &Ignore,
    mounts: &Mounts,
) -> Result<Ingested> {
    ingest_dir_after(sink, dir, ignore, mounts, &Prior::none())
}

/// What an ingest is doing, for a caller that wants to say so.
///
/// Hashing a large tree takes minutes, and a command that prints nothing for
/// minutes is indistinguishable from one that has hung. The counts are read
/// from the same atomic the workers already use to claim work, so reporting
/// costs nothing when nobody is listening.
#[derive(Debug, Clone, Copy)]
pub enum Progress<'a> {
    /// Walking the directory tree, before any hashing starts.
    Scanning { files: usize },
    /// Hashing file contents. `done` of `total`, with the bytes so far.
    Hashing { done: usize, total: usize, bytes: u64, path: &'a str },
    /// Building the directory objects — serial, and usually brief.
    Building,
}

/// A callback invoked as an ingest proceeds. Called from the coordinating
/// thread only, so an implementation does not have to be thread-safe.
pub type Observer<'o> = &'o mut dyn FnMut(Progress<'_>);

/// Decrements a count of running workers when one leaves, by any route.
struct Leaving<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for Leaving<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Release);
    }
}

/// Ingest a directory, knowing the tree it is a revision of.
///
/// Everything the previous tree offers is a hint about which chunk each new
/// chunk is a small edit away from. Pass [`Prior::none`] and this behaves
/// exactly as it always did.
pub fn ingest_dir_after(
    sink: &Sink,
    dir: &Path,
    ignore: &Ignore,
    mounts: &Mounts,
    prior: &Prior,
) -> Result<Ingested> {
    ingest_dir_watched(sink, dir, ignore, mounts, prior, None)
}

/// [`ingest_dir_after`], reporting progress as it goes.
pub fn ingest_dir_watched(
    sink: &Sink,
    dir: &Path,
    ignore: &Ignore,
    mounts: &Mounts,
    prior: &Prior,
    mut watch: Option<Observer<'_>>,
) -> Result<Ingested> {
    if let Some(w) = watch.as_deref_mut() {
        w(Progress::Scanning { files: 0 });
    }
    let sk = enumerate(dir, ignore, mounts)?;
    ingest_enumerated(sink, dir, sk, mounts, prior, watch)
}

/// The second half of an ingest: read what the walk listed.
///
/// Separate from the walk because the gap between the two is where a working
/// tree moves, and a test has to be able to stand in that gap.
fn ingest_enumerated(
    sink: &Sink,
    dir: &Path,
    mut sk: Skeleton,
    mounts: &Mounts,
    prior: &Prior,
    mut watch: Option<Observer<'_>>,
) -> Result<Ingested> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(sk.jobs.len().max(1));

    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let bytes = std::sync::atomic::AtomicU64::new(0);
    // Workers still going. The reporting loop waits on this as well as on the
    // count, because a worker that stops early -- an unreadable file, a panic
    // -- never reaches the count, and waiting for one that is never coming is
    // a hang. It hung only when somebody had asked for progress, which is to
    // say interactively, which is to say whenever a person runs `commit`.
    let live = std::sync::atomic::AtomicUsize::new(threads);
    let jobs = &sk.jobs;
    let total = jobs.len();

    // Scoped threads: no 'static bound, so `sink` and `jobs` are borrowed
    // directly and there is nothing to clone or Arc.
    let collected: Vec<Result<Vec<(&str, Ran)>>> =
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let next = &next;
                    let done = &done;
                    let bytes = &bytes;
                    let live = &live;
                    scope.spawn(move || {
                        // Counts this worker out however it leaves, including
                        // by `?` and by panicking.
                        let _leaving = Leaving(live);
                        let mut mine = Vec::new();
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(job) = jobs.get(i) else { break };
                            let ran = run_job(sink, job, prior)?;
                            // A job that found nothing to read is still a job
                            // finished, or the count would stop short of the
                            // total and the progress line with it.
                            done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let size = ran.entry.as_ref().map_or(0, |e| e.size);
                            bytes.fetch_add(size, std::sync::atomic::Ordering::Relaxed);
                            mine.push((job.rel.as_str(), ran));
                        }
                        Ok(mine)
                    })
                })
                .collect();

            // The coordinating thread reports while the workers run. Polling
            // beats a channel here: the counters are already there, and a
            // progress line nobody reads should not slow the work down.
            if let Some(w) = watch.as_deref_mut() {
                use std::sync::atomic::Ordering::{Acquire, Relaxed};
                // Acquire against the workers' release on the way out, so once
                // this sees none of them running it also sees everything they
                // counted.
                while live.load(Acquire) > 0 && done.load(Relaxed) < total {
                    w(Progress::Hashing {
                        done: done.load(Relaxed),
                        total,
                        bytes: bytes.load(Relaxed),
                        path: "",
                    });
                    std::thread::sleep(std::time::Duration::from_millis(80));
                }
                // What was actually finished, not what was hoped for: on the
                // way out of a failed ingest this is short of the total, and
                // saying otherwise would be the one thing worse than silence.
                w(Progress::Hashing {
                    done: done.load(Relaxed),
                    total,
                    bytes: bytes.load(Relaxed),
                    path: "",
                });
            }

            handles.into_iter().map(|h| h.join().expect("ingest worker panicked")).collect()
        });

    let mut files: BTreeMap<String, TreeEntry> = BTreeMap::new();
    let mut stats = WriteStats::default();
    let mut raced = std::mem::take(&mut sk.raced);
    for batch in collected {
        for (rel, ran) in batch? {
            stats.merge(ran.stats);
            if let Some(what) = ran.raced {
                raced.push(Raced { path: rel.to_string(), what });
            }
            if let Some(entry) = ran.entry {
                files.insert(rel.to_string(), entry);
            }
        }
    }
    // Sorted, so the same race reads the same way whichever worker met it.
    raced.sort();

    // A file that vanished usually went with its directory, which the walk
    // recorded while it was still there. Left alone it would be written into
    // the snapshot as an empty directory that does not exist. Only checked
    // when something did vanish, so a quiet tree pays nothing for this.
    if raced.iter().any(|r| r.what == Race::Vanished) {
        sk.dirs.retain(|d| fs::symlink_metadata(dir.join(d)).is_ok_and(|m| m.is_dir()));
    }

    // Submodules join the flat path map as ordinary entries, so tree building
    // below needs no notion of them at all.
    for rel in &sk.subs {
        let pin = mounts[rel];
        let size = crate::submodule::pinned_size(sink.store(), pin).with_context(|| {
            format!(
                "submodule {rel} is pinned at {pin}, which is not in this store — \
                 committing it would record a revision nobody else could check out"
            )
        })?;
        let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
        files.insert(
            rel.clone(),
            TreeEntry { name, kind: EntryKind::Submodule, hash: pin, size },
        );
    }

    // Tree building is serial and deterministic: identical contents must give an
    // identical hash no matter how the work was scheduled.
    if let Some(w) = watch {
        w(Progress::Building);
    }
    let dirs: std::collections::BTreeSet<String> = std::mem::take(&mut sk.dirs).into_iter().collect();
    let (hash, size) = build_nested(sink, &files, &dirs, "", prior)?;
    let unusable = std::mem::take(&mut sk.unusable);
    Ok(Ingested { hash, size, stats, raced, unusable })
}

/// Build nested trees from a flat map of full paths.
///
/// What `ingest_dir` does once it has hashed everything, exposed because
/// building history from a stream has the same map and none of the directory
/// walking -- it knows what changed, so there is nothing to discover.
pub fn build_paths(
    sink: &Sink,
    files: &BTreeMap<String, TreeEntry>,
    prior: &Prior,
) -> Result<(Hash, u64)> {
    build_nested(sink, files, &Default::default(), "", prior)
}

/// Assemble nested trees from flat paths, creating empty directories where the
/// skeleton recorded one but no file lives under it.
fn build_nested(
    sink: &Sink,
    files: &BTreeMap<String, TreeEntry>,
    dirs: &std::collections::BTreeSet<String>,
    prefix: &str,
    prior: &Prior,
) -> Result<(Hash, u64)> {
    use std::ops::Bound;

    // Where this directory's run begins. The root's is everything.
    let lead = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
    let from = (Bound::Included(lead.as_str()), Bound::Unbounded);

    let mut entries: Vec<TreeEntry> = Vec::new();
    let mut subdirs: std::collections::BTreeSet<String> = Default::default();

    for (path, entry) in files.range::<str, _>(from) {
        // The first key that does not start with `dir/` is past the run.
        let Some(rest) = path.strip_prefix(lead.as_str()) else { break };
        match rest.split_once('/') {
            None => entries.push(entry.clone()),
            Some((dir, _)) => {
                if !subdirs.contains(dir) {
                    subdirs.insert(dir.to_string());
                }
            }
        }
    }
    // Directories with no files under them would otherwise vanish.
    for d in dirs.range::<str, _>(from) {
        let Some(rest) = d.strip_prefix(lead.as_str()) else { break };
        if !rest.is_empty() && !rest.contains('/') && !subdirs.contains(rest) {
            subdirs.insert(rest.to_string());
        }
    }

    for name in subdirs {
        let child_prefix =
            if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let (hash, size) = build_nested(sink, files, dirs, &child_prefix, prior)?;
        entries.push(TreeEntry { name, kind: EntryKind::Dir, hash, size });
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    // What this directory's listing was made of before, so a run that differs
    // by one entry is stored as the few bytes that differ.
    let was = prior.entries_for(prefix);
    let (hash, size, _) = build_tree_after(sink, entries, &was)?;
    Ok((hash, size))
}

#[cfg(unix)]
fn is_executable(m: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_m: &fs::Metadata) -> bool {
    false
}

/// Ignore rules, read from `.fkitignore` or `.fkthat`.
///
/// The pattern language is `.gitignore`'s, all of it, matched by hand:
///
///   * `target`         — anything with that name, at any depth
///   * `*.log`, `vgcore.*`, `a?.txt` — `*` and `?` within one name
///   * `*.[oa]`, `[!a-c]*`, `[[:digit:]]*` — character classes
///   * `build/`         — a trailing `/` means directories only
///   * `/patch/`        — a leading `/` pins the rule to the ignore file's
///     own directory
///   * `server/target/` — a rule with a `/` inside it is a path from there too
///   * `**/target/`, `a/**/b`, `patch/**` — `**` stands for any depth
///   * `!keep.tmp`      — takes back what an earlier rule excluded
///   * `\#`, `\!`, `\*`, `name\ ` — a backslash makes the next character
///     ordinary
///   * `# comment` and blank lines are skipped
///
/// An ignore file may sit in any directory, and its rules are read relative
/// to the directory it is in. The last rule to match a path decides, and a
/// deeper file is read after a shallower one, so it has the last word. A
/// directory that is ignored is never walked, which means nothing inside it
/// can be taken back — the same bargain git makes, and the reason a 20 GB
/// build folder costs nothing to exclude.
///
/// The one thing here git does not do: `./patch/` is read as `/patch/`. In
/// git that line can never match anything, and a rule that is present in the
/// file and quietly not in effect is the worst outcome an ignore file can
/// produce. For the same reason, the few lines that cannot mean anything — an
/// unclosed `[`, a trailing `\`, a bare `/` — are not dropped in silence:
/// they are listed in [`Ignore::problems`].
#[derive(Clone)]
pub struct Ignore {
    rules: Vec<Rule>,
    problems: Vec<Problem>,
    /// Whether the walk should look for ignore files below the root. Off for
    /// [`Ignore::empty`], which has to go on meaning "ignore nothing".
    nested: bool,
}

/// One line of an ignore file, parsed.
#[derive(Clone)]
struct Rule {
    /// The directory the ignore file is in, relative to the root; `""` for
    /// the root itself. The rule says nothing about paths outside it.
    base: String,
    /// How many components `base` has, to skip them without re-counting.
    base_depth: usize,
    /// `!rule`: a match puts the path back rather than taking it out.
    negated: bool,
    /// Only directories. Set by a trailing `/`.
    dir_only: bool,
    /// Matched against the whole path below `base`. When false there is
    /// exactly one segment, a name, and it is matched against the last
    /// component alone.
    rooted: bool,
    segments: Vec<Segment>,
}

#[derive(Clone)]
enum Segment {
    /// `**`: any number of directories. In front of or between names that
    /// number may be none. At the end of a rule it means "everything inside",
    /// which is not the directory itself, so there it is at least one.
    AnyDepth { at_least_one: bool },
    /// One path component.
    Name(Glob),
}

/// A line that could not be turned into a rule, and so is not in effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The ignore file, relative to the root: `.fkthat`, `server/.fkitignore`.
    pub file: String,
    /// 1-based. 0 when it is the file as a whole that could not be read.
///
/// Both maps are sorted, and that is what this leans on: everything under a
/// directory is one contiguous run of keys, starting at `dir/`. So a directory
/// looks only at its own run. It used to look at every path in the repository
/// and discard the ones that were not its own, which made a build cost files
/// times directories -- a tree of sixty thousand files in three thousand
/// directories examined a hundred and eighty million paths to commit one
/// change, and every commit, status and import paid it.
    pub line: usize,
    pub text: String,
    pub why: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            write!(f, "{} is not in effect — {}", self.file, self.why)
        } else {
            write!(
                f,
                "{}:{}: '{}' is not in effect — {}",
                self.file, self.line, self.text, self.why
            )
        }
    }
}

/// Both names for the same file, in the order they are read.
///
/// If a directory has both, both apply. The alternative — first one wins —
/// means adding the second file silently does nothing, and a rule that is
/// present in the tree and quietly not in effect is the worst outcome an
/// ignore file can produce.
pub const IGNORE_FILES: [&str; 2] = [".fkitignore", ".fkthat"];

impl Ignore {
    /// Ignore nothing, and read no ignore files.
    pub fn empty() -> Ignore {
        Ignore { rules: vec![], problems: vec![], nested: false }
    }

    /// The rules at the root of a repository. Ignore files further down are
    /// picked up by the walk as it reaches them.
    pub fn load(repo_root: &Path) -> Ignore {
        let mut ig = Ignore { rules: vec![], problems: vec![], nested: true };
        ig.read_files(repo_root, "");
        ig
    }

    /// Lines that were read and could not be honoured. Empty is the normal
    /// case; anything here is something the person who wrote the file believes
    /// is being ignored and is not.
    ///
    /// Only what this value itself read: after [`Ignore::load`], the root's
    /// files. Problems in files further down surface in
    /// [`Ingested::unusable`], because nobody has opened those yet.
    pub fn problems(&self) -> &[Problem] {
        &self.problems
    }

    /// These rules plus the ones in `dir`'s own ignore files, for walking
    /// `dir` and everything under it. `prefix` is `dir` relative to the root.
    ///
    /// Returns the new problems separately so the parent's are not reported
    /// once per directory.
    fn descend(&self, dir: &Path, prefix: &str) -> (Ignore, Vec<Problem>) {
        let mut ig = Ignore { rules: self.rules.clone(), problems: vec![], nested: true };
        ig.read_files(dir, prefix);
        let problems = std::mem::take(&mut ig.problems);
        (ig, problems)
    }

    /// Read whichever of [`IGNORE_FILES`] `dir` has.
    fn read_files(&mut self, dir: &Path, base: &str) {
        for file in IGNORE_FILES {
            let shown = if base.is_empty() { file.to_string() } else { format!("{base}/{file}") };
            match fs::read_to_string(dir.join(file)) {
                Ok(text) => self.extend_from(base, &shown, &text),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                // There, and unreadable: every rule in it is one somebody
                // believes is in effect.
                Err(e) => self.problems.push(Problem {
                    file: shown,
                    line: 0,
                    text: String::new(),
                    why: format!("it cannot be read: {e}"),
                }),
            }
        }
    }

    /// Parse rules out of one ignore file's contents.
    fn extend_from(&mut self, base: &str, file: &str, text: &str) {
        for (i, line) in text.lines().enumerate() {
            match Rule::parse(base, line) {
                Ok(Some(rule)) => self.rules.push(rule),
                Ok(None) => {}
                Err(why) => self.problems.push(Problem {
                    file: file.to_string(),
                    line: i + 1,
                    text: line.to_string(),
                    why,
                }),
            }
        }
    }

    /// Whether `path` — relative to the repository root, `/`-separated — is
    /// left out of a snapshot.
    ///
    /// This answers for the path it is given and nothing above it. The walk
    /// asks about every directory on the way down and does not enter one that
    /// is ignored, so `server/target/` never has to be recognised from
    /// `server/target/o`.
    pub fn matches(&self, path: &str, is_dir: bool) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path);
        // The repository's own metadata is never part of a snapshot, and no
        // rule gets to say otherwise.
        if name == ".fkit" {
            return true;
        }
        // Split only if a rule needs the whole path; most rules are a bare
        // name, and most repositories have no other kind.
        let mut parts: Option<Vec<&str>> = None;
        for rule in self.rules.iter().rev() {
            if rule.dir_only && !is_dir {
                continue;
            }
            if !rule.base.is_empty()
                && !path.strip_prefix(rule.base.as_str()).is_some_and(|r| r.starts_with('/'))
            {
                continue;
            }
            let hit = if rule.rooted {
                let parts = parts.get_or_insert_with(|| path.split('/').collect());
                segments_match(&rule.segments, &parts[rule.base_depth..])
            } else {
                match &rule.segments[0] {
                    Segment::Name(glob) => glob.matches(name),
                    // Not constructed: a lone `**` is parsed as `*`.
                    Segment::AnyDepth { .. } => true,
                }
            };
            if hit {
                return !rule.negated;
            }
        }
        false
    }
}

impl Rule {
    /// `Ok(None)` for a blank line or a comment.
    fn parse(base: &str, line: &str) -> std::result::Result<Option<Rule>, String> {
        // Trailing spaces do not count, unless a backslash says one does.
        // Leading ones do: ` x` is a file whose name starts with a space.
        let mut text = line;
        while let Some(shorter) = text.strip_suffix(' ') {
            let slashes = shorter.len() - shorter.trim_end_matches('\\').len();
            if slashes % 2 == 1 {
                break;
            }
            text = shorter;
        }
        if text.is_empty() || text.starts_with('#') {
            return Ok(None);
        }
        let (negated, mut rest) = match text.strip_prefix('!') {
            Some(r) => (true, r),
            None => (false, text),
        };

        // `/x` means "x, right here". So does `./x`, which git would never
        // match; see the note on `Ignore`.
        let mut rooted = false;
        while let Some(r) = rest.strip_prefix("./") {
            rest = r;
            rooted = true;
        }
        if let Some(r) = rest.strip_prefix('/') {
            rest = r;
            rooted = true;
        }
        let dir_only = rest.ends_with('/');
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.is_empty() {
            return Err("it names nothing".into());
        }
        // A `/` anywhere but the end makes the rule a path from the ignore
        // file's directory. One name on its own floats: it matches wherever
        // that name turns up below. Any `/` counts, as it does for git, even
        // one inside a `[...]` where it separates nothing.
        let rooted = rooted || rest.contains('/');

        let components = tokenize(rest)?;
        let last = components.len() - 1;
        let mut segments: Vec<Segment> = Vec::new();
        for (i, part) in components.into_iter().enumerate() {
            // One leading and one trailing `/` mean something. Any other
            // doubled `/` asks for a path component with no name, and no path
            // has one. Git keeps such a line and never matches it.
            if part.tokens.is_empty() {
                return Err("a doubled / can never match; no path has an empty name in it".into());
            }
            // Two stars or more, alone between slashes. Git reads `***/` the
            // same as `**/`, so this does too.
            let stars = part.tokens.iter().all(|t| matches!(t, Token::Star));
            if stars && part.tokens.len() >= 2 {
                segments.push(Segment::AnyDepth { at_least_one: i == last || part.escaped_slash });
            } else {
                segments.push(Segment::Name(Glob::new(part.tokens)));
            }
        }

        // A bare `**` is every name, which is what `*` already says.
        if !rooted {
            segments = vec![match segments.remove(0) {
                Segment::AnyDepth { .. } => Segment::Name(Glob::new(vec![Token::Star])),
                name => name,
            }];
        }

        let base_depth = if base.is_empty() { 0 } else { base.split('/').count() };
        Ok(Some(Rule {
            base: base.to_string(),
            base_depth,
            negated,
            dir_only,
            rooted,
            segments,
        }))
    }
}

/// One `/`-separated piece of a pattern, as tokens.
struct Component {
    tokens: Vec<Token>,
    /// The `/` that ended this piece was written `\/`. Still a separator --
    /// there is no other kind of `/` in a path -- but git does not let a `**`
    /// in front of one stand for no directories at all, so it is remembered.
    escaped_slash: bool,
}

/// Read a whole pattern: split it at its `/`s and each piece into tokens.
///
/// One pass rather than a split and then a parse, because where the pieces
/// end depends on what is in them: the `/` in `[a/b]` is a member of a set and
/// the one in `a\\/b` follows an escaped backslash, and only reading the
/// pattern left to right tells either from a separator.
fn tokenize(pattern: &str) -> std::result::Result<Vec<Component>, String> {
    let fresh = || Component { tokens: Vec::new(), escaped_slash: false };
    let mut parts = vec![fresh()];
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        let here = parts.last_mut().expect("starts with one part");
        match c {
            '/' => parts.push(fresh()),
            '\\' => match chars.next() {
                Some('/') => {
                    here.escaped_slash = true;
                    parts.push(fresh());
                }
                Some(next) => here.tokens.push(Token::Char(next)),
                None => return Err("it ends in a backslash with nothing to escape".into()),
            },
            '?' => here.tokens.push(Token::One),
            '*' => here.tokens.push(Token::Star),
            '[' => here.tokens.push(parse_class(&mut chars)?),
            c => here.tokens.push(Token::Char(c)),
        }
    }
    Ok(parts)
}

/// Match a rule's segments against a path's components, all of both.
fn segments_match(segments: &[Segment], parts: &[&str]) -> bool {
    match segments.split_first() {
        None => parts.is_empty(),
        Some((Segment::AnyDepth { at_least_one }, rest)) => {
            // Try swallowing nothing (where that is allowed), then one
            // directory, then two...
            (usize::from(*at_least_one)..=parts.len())
                .any(|skip| segments_match(rest, &parts[skip..]))
        }
        Some((Segment::Name(glob), rest)) => match parts.split_first() {
            Some((first, more)) => glob.matches(first) && segments_match(rest, more),
            None => false,
        },
    }
}

/// A pattern for one path component: `*.log`, `f?.txt`, `*.[oa]`.
///
/// Parsed once, when the ignore file is read, so matching a hundred thousand
/// names does not mean reading the pattern a hundred thousand times.
#[derive(Clone)]
struct Glob {
    tokens: Vec<Token>,
    /// Set when the pattern has no wildcards at all, which is most of them:
    /// then matching is one string comparison.
    literal: Option<String>,
}

#[derive(Clone)]
enum Token {
    Char(char),
    /// `?`: exactly one character.
    One,
    /// `*`: any run of characters, including none.
    Star,
    /// `[...]`: one character from a set, or with `negated` one not in it.
    Class { negated: bool, items: Vec<ClassItem> },
}

#[derive(Clone)]
enum ClassItem {
    /// `a-z`. A single character is the range from itself to itself.
    Range(char, char),
    /// `[:alpha:]` and friends.
    Named(fn(char) -> bool),
}

impl Token {
    /// Whether this token matches the single character `c`. Never asked of
    /// `Star`, which is not about one character.
    fn takes(&self, c: char) -> bool {
        match self {
            Token::Char(x) => *x == c,
            Token::One => true,
            Token::Star => false,
            Token::Class { negated, items } => {
                let inside = items.iter().any(|item| match item {
                    ClassItem::Range(lo, hi) => *lo <= c && c <= *hi,
                    ClassItem::Named(is) => is(c),
                });
                inside != *negated
            }
        }
    }
}

impl Glob {
    fn new(mut tokens: Vec<Token>) -> Glob {
        // `**` inside a name is just `*`.
        tokens.dedup_by(|a, b| matches!((a, b), (Token::Star, Token::Star)));
        let literal = tokens
            .iter()
            .map(|t| match t {
                Token::Char(c) => Some(*c),
                _ => None,
            })
            .collect::<Option<String>>();
        Glob { tokens, literal }
    }

    /// The classic two-pointer walk: on a mismatch, go back to the last `*`
    /// and let it swallow one more character. No recursion to blow up on
    /// `a*a*a*a*b`. Positions in the name are byte offsets, stepped a whole
    /// character at a time, so nothing is copied out to match.
    fn matches(&self, name: &str) -> bool {
        if let Some(literal) = &self.literal {
            return literal == name;
        }
        let t = &self.tokens;
        let (mut ti, mut ni) = (0, 0);
        // Where the last `*` was, and where in the name it had reached.
        let mut star: Option<(usize, usize)> = None;

        while let Some(c) = name[ni..].chars().next() {
            if ti < t.len() && matches!(t[ti], Token::Star) {
                star = Some((ti, ni));
                ti += 1;
            } else if ti < t.len() && t[ti].takes(c) {
                ti += 1;
                ni += c.len_utf8();
            } else if let Some((st, sn)) = star {
                let swallowed = name[sn..].chars().next().map_or(1, char::len_utf8);
                ti = st + 1;
                ni = sn + swallowed;
                star = Some((st, ni));
            } else {
                return false;
            }
        }
        t[ti..].iter().all(|tok| matches!(tok, Token::Star))
    }
}

/// Parse a character class, the opening `[` already consumed.
///
/// The awkward corners are the ones every glob has: `!` or `^` first negates,
/// a `]` first is a member rather than the end, a `-` last is a member rather
/// than a range, and a backslash makes anything ordinary.
fn parse_class(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> std::result::Result<Token, String> {
    let unclosed = || "a [ is never closed".to_string();

    let negated = matches!(chars.peek(), Some('!' | '^'));
    if negated {
        chars.next();
    }
    let mut items = Vec::new();
    let mut first = true;
    loop {
        let c = chars.next().ok_or_else(unclosed)?;
        let lo = match c {
            ']' if !first => break,
            '[' if chars.peek() == Some(&':') => {
                chars.next();
                let mut name = String::new();
                loop {
                    match chars.next().ok_or_else(unclosed)? {
                        ':' if chars.peek() == Some(&']') => {
                            chars.next();
                            break;
                        }
                        ch => name.push(ch),
                    }
                }
                let is = named_class(&name)
                    .ok_or_else(|| format!("[:{name}:] is not a character class"))?;
                items.push(ClassItem::Named(is));
                first = false;
                continue;
            }
            '\\' => chars.next().ok_or_else(unclosed)?,
            c => c,
        };
        first = false;

        // `a-z` is a range; `a-]` is an `a` and then a `-`.
        let mut ahead = chars.clone();
        if ahead.next() == Some('-') && !matches!(ahead.peek(), Some(']') | None) {
            chars.next();
            let mut hi = chars.next().ok_or_else(unclosed)?;
            if hi == '\\' {
                hi = chars.next().ok_or_else(unclosed)?;
            }
            items.push(ClassItem::Range(lo, hi));
        } else {
            items.push(ClassItem::Range(lo, lo));
        }
    }
    Ok(Token::Class { negated, items })
}

/// The POSIX classes, by the name between `[:` and `:]`.
fn named_class(name: &str) -> Option<fn(char) -> bool> {
    Some(match name {
        "alnum" => |c| c.is_alphanumeric(),
        "alpha" => |c| c.is_alphabetic(),
        "blank" => |c| c == ' ' || c == '\t',
        "cntrl" => |c| c.is_control(),
        "digit" => |c| c.is_ascii_digit(),
        "graph" => |c| !c.is_whitespace() && !c.is_control(),
        "lower" => |c| c.is_lowercase(),
        "print" => |c| !c.is_control(),
        "punct" => |c| c.is_ascii_punctuation(),
        "space" => |c| c.is_whitespace(),
        "upper" => |c| c.is_uppercase(),
        "xdigit" => |c| c.is_ascii_hexdigit(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(tag: &str) -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("fkit-ingest-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (Store::open(&dir).unwrap(), dir)
    }

    #[test]
    fn either_ignore_filename_is_read() {
        for name in super::IGNORE_FILES {
            let dir = std::env::temp_dir()
                .join(format!("fkit-ign-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(name), "target/\n*.log\nsecret.txt\n").unwrap();

            let ig = Ignore::load(&dir);
            assert!(ig.matches("target", true), "{name}: a directory rule");
            assert!(ig.matches("run.log", false), "{name}: a suffix rule");
            assert!(ig.matches("secret.txt", false), "{name}: an exact name");
            assert!(!ig.matches("keep.rs", false), "{name}: everything else stays");
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn both_ignore_files_apply_together() {
        // Whichever name someone reaches for, a rule that is present in the
        // tree must be in effect — the second file cannot be silently dead.
        let dir = std::env::temp_dir().join(format!("fkit-ign-both-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".fkitignore"), "target/\n").unwrap();
        fs::write(dir.join(".fkthat"), "*.log\n").unwrap();

        let ig = Ignore::load(&dir);
        assert!(ig.matches("target", true), "the rule from .fkitignore");
        assert!(ig.matches("run.log", false), "the rule from .fkthat");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Rules straight from text, as if they had been read from `.fkthat`.
    fn rules(text: &str) -> Ignore {
        let mut ig = Ignore::empty();
        ig.extend_from("", ".fkthat", text);
        assert_eq!(ig.problems(), &[], "every line here should parse");
        ig
    }

    #[test]
    fn a_bare_name_matches_at_any_depth() {
        let ig = rules("target\nprefix/\n*.key\n");
        assert!(ig.matches("target", true));
        assert!(ig.matches("server/target", true));
        assert!(ig.matches("a/b/target", false), "a bare name is files too");
        assert!(ig.matches("prefix", true));
        assert!(ig.matches("sub/prefix", true));
        assert!(!ig.matches("sub/prefix", false), "a trailing / is directories only");
        assert!(ig.matches("deep/down/id.key", false));
        assert!(!ig.matches("deep/down/id.keys", false));
    }

    /// The case the rules were rewritten for: a root `patch/` full of build
    /// output, and source folders elsewhere that happen to share the name.
    #[test]
    fn a_leading_slash_pins_a_rule_to_the_root() {
        for line in ["/patch/", "./patch/"] {
            let ig = rules(line);
            assert!(ig.matches("patch", true), "{line}: the root folder goes");
            assert!(!ig.matches("server/patch", true), "{line}: the source folder stays");
            assert!(!ig.matches("data/patch", true), "{line}: and the other one");
            assert!(!ig.matches("patch", false), "{line}: a file called patch is not a folder");
        }
        let ig = rules("/notes.txt");
        assert!(ig.matches("notes.txt", false));
        assert!(!ig.matches("docs/notes.txt", false));
    }

    /// `patch/**` is everything inside the folder and not the folder, which
    /// is the difference between it and `patch/`: something directly inside
    /// can still be taken back.
    #[test]
    fn a_trailing_double_star_is_everything_inside() {
        let ig = rules("patch/**\n!patch/README\n");
        assert!(!ig.matches("patch", true), "the folder itself is not inside itself");
        assert!(ig.matches("patch/p1", false));
        assert!(ig.matches("patch/2024", true));
        assert!(ig.matches("patch/2024/a/b", false));
        assert!(!ig.matches("patch/README", false), "taken back");
        assert!(!ig.matches("server/patch/src", true), "still only the root one");
    }

    #[test]
    fn a_rule_with_a_slash_inside_is_a_path_from_the_root() {
        let ig = rules("server/target/\ndocs/*.pdf\n");
        assert!(ig.matches("server/target", true));
        assert!(!ig.matches("target", true), "only the one that was named");
        assert!(!ig.matches("client/server/target", true), "and only from the root");
        assert!(ig.matches("docs/a.pdf", false));
        assert!(!ig.matches("docs/old/a.pdf", false), "* does not cross a /");
        assert!(!ig.matches("a.pdf", false));
    }

    #[test]
    fn double_star_stands_for_any_depth() {
        let ig = rules("**/target/\nsrc/**/gen\n");
        assert!(ig.matches("target", true), "any depth includes none");
        assert!(ig.matches("server/target", true));
        assert!(ig.matches("a/b/c/target", true));
        assert!(!ig.matches("a/target", false), "still directories only");
        assert!(ig.matches("src/gen", false));
        assert!(ig.matches("src/a/b/gen", false));
        assert!(!ig.matches("lib/a/gen", false));
        assert!(rules("***/gen").matches("a/b/gen", false), "more stars are no different");
    }

    #[test]
    fn stars_and_question_marks_match_within_a_name() {
        let ig = rules("vgcore.*\nf?.txt\n*~\ncore*dump*\n");
        assert!(ig.matches("vgcore.123", false));
        assert!(ig.matches("deep/vgcore.9", false));
        assert!(!ig.matches("vgcore", false), "the dot is part of the rule");
        assert!(ig.matches("f1.txt", false));
        assert!(!ig.matches("f10.txt", false), "? is exactly one character");
        assert!(!ig.matches("f.txt", false));
        assert!(ig.matches("notes.md~", false));
        assert!(ig.matches("core.dump.1", false));
        assert!(ig.matches("coredump", false), "* may match nothing");
        assert!(!ig.matches("score.dump", false), "the whole name, not part of it");

        // Backtracking: the first `a` a star could stop at is not always the
        // right one.
        let glob = |pattern: &str, name: &str| {
            let mut parts = tokenize(pattern).unwrap();
            assert_eq!(parts.len(), 1, "one name, no slashes");
            Glob::new(parts.remove(0).tokens).matches(name)
        };
        assert!(glob("*a*b", "xaxaxb"));
        assert!(!glob("*a*b", "xaxaxbx"));
        assert!(glob("***", ""));
        assert!(glob("é?", "éü"), "characters, not bytes");
        assert!(glob("*ü", "éüü"), "and a star steps over whole characters");
    }

    #[test]
    fn a_bang_takes_back_what_an_earlier_rule_excluded() {
        let ig = rules("*.tmp\n!keep.tmp\n");
        assert!(ig.matches("a.tmp", false));
        assert!(!ig.matches("keep.tmp", false));
        assert!(!ig.matches("sub/keep.tmp", false));

        // The last rule to match decides, so the other order means the other
        // thing.
        let ig = rules("!keep.tmp\n*.tmp\n");
        assert!(ig.matches("keep.tmp", false));
    }

    #[test]
    fn the_repository_itself_is_never_snapshotted() {
        assert!(Ignore::empty().matches(".fkit", true));
        assert!(Ignore::empty().matches("vendored/.fkit", true));
        assert!(rules("!.fkit\n").matches(".fkit", true), "and no rule can say otherwise");
    }

    #[test]
    fn character_classes_match_one_character_from_a_set() {
        let ig = rules("*.[oa]\nlog[0-9]\n[!a-c]x\n[]-]y\n[[:digit:][:upper:]]z\n[a\\]]w\n");
        assert!(ig.matches("main.o", false));
        assert!(ig.matches("lib/libz.a", false));
        assert!(!ig.matches("main.c", false));
        assert!(!ig.matches("main.oa", false), "one character, not one or more");
        assert!(ig.matches("log7", false));
        assert!(!ig.matches("logx", false));
        assert!(ig.matches("dx", false));
        assert!(!ig.matches("bx", false), "! at the front turns the set inside out");
        assert!(ig.matches("]y", false), "a ] first is a member, not the end");
        assert!(ig.matches("-y", false), "a - last is a member, not a range");
        assert!(ig.matches("7z", false));
        assert!(ig.matches("Qz", false));
        assert!(!ig.matches("qz", false));
        assert!(ig.matches("]w", false), "an escaped ] is a member too");
        assert!(rules("[^a-c]x").matches("dx", false), "^ negates as well as !");
    }

    #[test]
    fn a_backslash_makes_the_next_character_ordinary() {
        let ig = rules("\\#notes\n\\!important\nstar\\*\nwhat\\?\ntrailing\\ \nplain   \n");
        assert!(ig.matches("#notes", false), "not a comment");
        assert!(ig.matches("!important", false), "not a negation");
        assert!(ig.matches("star*", false));
        assert!(!ig.matches("starry", false), "the * is a character now");
        assert!(ig.matches("what?", false));
        assert!(!ig.matches("whatx", false));
        assert!(ig.matches("trailing ", false), "an escaped trailing space is kept");
        assert!(!ig.matches("trailing", false));
        assert!(ig.matches("plain", false), "unescaped trailing spaces are not");
        assert!(rules(" lead").matches(" lead", false), "leading spaces are part of the name");

        // An escaped / is still a /, and an escaped \ before one is not an
        // escape of it.
        let ig = rules("docs\\/*.pdf\n");
        assert!(ig.matches("docs/a.pdf", false));
        assert!(!ig.matches("a.pdf", false));
        assert!(rules("a\\\\/b").matches("a\\/b", false));

        // Corners found by running the same rules through git itself. A / in
        // a class separates nothing, but still pins the rule to its folder.
        let ig = rules("[x/]y\n");
        assert!(ig.matches("xy", false));
        assert!(!ig.matches("sub/xy", false));
        // And `**` before an escaped / cannot stand for no directories.
        let ig = rules("**\\/gen\n");
        assert!(ig.matches("a/gen", false));
        assert!(!ig.matches("gen", false));
        assert!(rules("**/gen").matches("gen", false), "where a plain / lets it");
    }

    /// The few lines that cannot mean anything are said out loud. The
    /// alternative is what this used to do: keep them as a name no file has.
    #[test]
    fn a_line_that_cannot_be_a_rule_is_reported_not_kept() {
        let mut ig = Ignore::empty();
        ig.extend_from(
            "",
            ".fkthat",
            "# objects\n*.[oa\n\ntarget/\nback\\\n/\n!\n[[:nope:]]\na//b\n",
        );

        let seen: Vec<(usize, &str)> =
            ig.problems().iter().map(|p| (p.line, p.text.as_str())).collect();
        assert_eq!(
            seen,
            [(2, "*.[oa"), (5, "back\\"), (6, "/"), (7, "!"), (8, "[[:nope:]]"), (9, "a//b")],
        );
        assert_eq!(
            ig.problems()[0].to_string(),
            ".fkthat:2: '*.[oa' is not in effect — a [ is never closed",
        );

        // The good line beside them still works, and the bad ones match
        // nothing rather than something surprising.
        assert!(ig.matches("target", true));
        assert!(!ig.matches("main.o", false));
        assert!(!ig.matches("*.[oa", false));
    }

    /// Every file path in a tree, for comparing against what was on disk.
    fn paths_in(store: &Store, tree: Hash, prefix: &str, out: &mut Vec<String>) {
        for e in read_entries(store, tree).unwrap() {
            let path =
                if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
            if e.kind == EntryKind::Dir {
                out.push(format!("{path}/"));
                paths_in(store, e.hash, &path, out);
            } else {
                out.push(path);
            }
        }
    }

    fn put(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    /// The rules as the walk applies them: an ignored directory is not
    /// entered, so a path rule only has to match the directory itself.
    #[test]
    fn the_walk_applies_rules_to_paths_not_just_names() {
        let (store, dir) = tmp_store("ignwalk");
        let work = dir.join("work");
        put(&work, "patch/p1", "a");
        put(&work, "server/patch/src/lib.rs", "b");
        put(&work, "server/target/o", "c");
        put(&work, "sub/prefix/y", "e");
        put(&work, "vgcore.123", "f");

        let snapshot = |text: &str| {
            let ig = rules(text);
            let ing = ingest_dir(&Sink::writing(&store), &work, &ig, &Default::default()).unwrap();
            let mut out = Vec::new();
            paths_in(&store, ing.hash, "", &mut out);
            out
        };

        let all = snapshot("");
        assert!(all.contains(&"patch/p1".to_string()));

        // Everything inside goes, the folder stays -- and since a snapshot
        // records empty folders, it is there, empty.
        let inside = snapshot("patch/**\n");
        assert!(inside.contains(&"patch/".to_string()));
        assert!(!inside.contains(&"patch/p1".to_string()));

        let got = snapshot("/patch/\nserver/target/\nvgcore.*\n");
        assert_eq!(
            got,
            ["server/", "server/patch/", "server/patch/src/", "server/patch/src/lib.rs",
             "sub/", "sub/prefix/", "sub/prefix/y"],
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// An ignore file further down speaks for its own directory: its rules are
    /// read from where it sits, it cannot reach its siblings, and where it
    /// disagrees with the root it has the last word.
    #[test]
    fn an_ignore_file_in_a_subdirectory_applies_from_there_down() {
        let dir = std::env::temp_dir().join(format!("fkit-ign-nested-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = Store::open(dir.join("store")).unwrap();
        let work = dir.join("work");
        put(&work, ".fkitignore", "*.log\n");
        put(&work, "a.log", "");
        put(&work, "out/x", "");
        put(&work, "server/.fkthat", "/out/\n!keep.log\n*.gen\nbad[\n");
        put(&work, "server/out/x", "");
        put(&work, "server/keep.log", "");
        put(&work, "server/other.log", "");
        put(&work, "server/deep/out/x", "");
        put(&work, "server/deep/keep.log", "");
        put(&work, "server/deep/a.gen", "");
        put(&work, "client/a.gen", "");

        let ig = Ignore::load(&work);
        assert_eq!(ig.problems(), &[], "the root file is fine");
        let ing = ingest_dir(&Sink::writing(&store), &work, &ig, &Mounts::new()).unwrap();
        let mut got = Vec::new();
        paths_in(&store, ing.hash, "", &mut got);
        got.retain(|p| !p.ends_with('/'));
        assert_eq!(
            got,
            [
                ".fkitignore",
                "client/a.gen",           // server's *.gen is not client's business
                "out/x",                  // nor is its /out/
                "server/.fkthat",
                "server/deep/keep.log",   // !keep.log overrides the root's *.log
                "server/deep/out/x",      // /out/ is pinned to server/, not below
                "server/keep.log",
            ],
        );

        // And the line in it that could not be a rule is named, with the
        // path of the file it is in.
        let said: Vec<String> = ing.unusable.iter().map(|p| p.to_string()).collect();
        assert_eq!(said, ["server/.fkthat:4: 'bad[' is not in effect — a [ is never closed"]);

        // `Ignore::empty` goes on meaning what it says.
        let all = ingest_dir(&Sink::writing(&store), &work, &Ignore::empty(), &Mounts::new())
            .unwrap();
        let mut got = Vec::new();
        paths_in(&store, all.hash, "", &mut got);
        assert!(got.contains(&"server/out/x".to_string()));
        assert!(got.contains(&"a.log".to_string()));
        let _ = fs::remove_dir_all(dir);
    }

    /// A file listed by the walk and gone by the time it is read is left out
    /// and reported. It used to fail the whole snapshot, which in a working
    /// tree with a build running meant commits failing at random.
    #[test]
    fn a_file_that_vanishes_after_the_walk_is_left_out_not_fatal() {
        let (store, dir) = tmp_store("vanish");
        let work = dir.join("work");
        for i in 0..40 {
            put(&work, &format!("f{i}.txt"), &format!("file {i}"));
        }
        put(&work, "build/tmp/a.o", "a");
        put(&work, "build/tmp/b.o", "b");
        put(&work, "build/keep.txt", "k");
        #[cfg(unix)]
        std::os::unix::fs::symlink("f1.txt", work.join("link")).unwrap();

        let sink = Sink::writing(&store);
        let (ig, mounts) = (Ignore::empty(), Mounts::new());
        let sk = enumerate(&work, &ig, &mounts).unwrap();

        // The gap between listing and reading, where a build does its work.
        fs::remove_file(work.join("f3.txt")).unwrap();
        fs::remove_file(work.join("f17.txt")).unwrap();
        fs::remove_dir_all(work.join("build/tmp")).unwrap();
        #[cfg(unix)]
        fs::remove_file(work.join("link")).unwrap();

        let ing = ingest_enumerated(&sink, &work, sk, &mounts, &Prior::none(), None)
            .expect("vanished files must not fail the snapshot");

        let mut want = vec!["build/tmp/a.o", "build/tmp/b.o", "f17.txt", "f3.txt"];
        if cfg!(unix) {
            want.push("link");
        }
        let got: Vec<&str> = ing.raced.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(got, want);
        assert!(ing.raced.iter().all(|r| r.what == Race::Vanished));

        // And what was stored is exactly the tree as it now stands: no entry
        // for the files, and no empty `build/tmp` for a directory that is gone.
        let fresh = ingest_dir(&sink, &work, &ig, &mounts).unwrap();
        assert!(fresh.raced.is_empty(), "a quiet tree reports nothing");
        assert_eq!(ing.hash, fresh.hash);
        let _ = fs::remove_dir_all(dir);
    }

    /// Forgiving a missing path stops at the root: there the directory being
    /// gone is not a race, it is the wrong directory.
    #[test]
    fn a_missing_root_is_still_an_error() {
        let (store, dir) = tmp_store("noroot");
        let out = ingest_dir(
            &Sink::dry(&store),
            &dir.join("never-created"),
            &Ignore::empty(),
            &Mounts::new(),
        );
        assert!(out.is_err(), "no root is nothing to snapshot, not an empty snapshot");
        let _ = fs::remove_dir_all(dir);
    }

    /// A file replaced by a directory of the same name is gone as a file.
    #[test]
    fn a_file_that_became_a_directory_is_skipped() {
        let (store, dir) = tmp_store("becamedir");
        let work = dir.join("work");
        put(&work, "a.txt", "a");
        put(&work, "thing", "was a file");

        let sink = Sink::writing(&store);
        let (ig, mounts) = (Ignore::empty(), Mounts::new());
        let sk = enumerate(&work, &ig, &mounts).unwrap();
        fs::remove_file(work.join("thing")).unwrap();
        fs::create_dir(work.join("thing")).unwrap();

        let ing = ingest_enumerated(&sink, &work, sk, &mounts, &Prior::none(), None).unwrap();
        assert_eq!(ing.raced, [Raced { path: "thing".into(), what: Race::Vanished }]);
        let mut paths = Vec::new();
        paths_in(&store, ing.hash, "", &mut paths);
        assert_eq!(paths, ["a.txt"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn file_roundtrips_through_the_merkle_tree() {
        let (s, dir) = tmp_store("roundtrip");
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();

        let ing = ingest_bytes(&Sink::writing(&s), &data).unwrap();
        assert_eq!(ing.size, data.len() as u64);

        let mut out = Vec::new();
        read_file(&s, ing.hash, &mut out).unwrap();
        assert_eq!(out, data, "reassembled bytes must equal the original");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_file_is_representable() {
        let (s, dir) = tmp_store("empty");
        let ing = ingest_bytes(&Sink::writing(&s), b"").unwrap();
        assert_eq!(ing.size, 0);
        let mut out = Vec::new();
        read_file(&s, ing.hash, &mut out).unwrap();
        assert!(out.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn big_files_grow_extra_levels() {
        let (s, dir) = tmp_store("levels");
        // ~24 MB / ~8 KiB avg = ~3000 chunks, comfortably past FANOUT=256.
        let data: Vec<u8> = (0..24_000_000u32).map(|i| (i.wrapping_mul(2246822519) >> 11) as u8).collect();
        let ing = ingest_bytes(&Sink::writing(&s), &data).unwrap();

        match s.get(ing.hash).unwrap() {
            Object::File { level, children } => {
                assert!(level >= 1, "expected a multi-level tree, got level {level}");
                assert!(children.len() <= FANOUT);
            }
            other => panic!("expected a file node, got {:?}", other.kind()),
        }
        let _ = fs::remove_dir_all(dir);
    }

    /// The payoff property: editing one byte of a large file should re-store
    /// only a handful of objects, not the whole thing.
    #[test]
    fn small_edit_stores_almost_nothing_new() {
        let (s, dir) = tmp_store("edit");
        let data: Vec<u8> = (0..8_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 15) as u8).collect();

        let first = ingest_bytes(&Sink::writing(&s), &data).unwrap();

        let mut edited = data.clone();
        edited[4_000_000] ^= 0xFF;
        let second = ingest_bytes(&Sink::writing(&s), &edited).unwrap();

        assert_ne!(first.hash, second.hash, "changed content must change the root hash");

        let rewritten = second.stats.bytes_written;
        let total = data.len() as u64;
        assert!(
            rewritten < total / 50,
            "a 1-byte edit rewrote {rewritten} bytes of {total} (expected <2%)"
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// The property the whole change exists for: inserting one file into a
    /// large directory must not rewrite the whole directory.
    #[test]
    fn adding_one_file_to_a_large_directory_rewrites_almost_nothing() {
        let (s, dir) = tmp_store("bigdir");
        let a = dir.join("a");
        let b = dir.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();

        // 4000 files, then the same 4000 plus one inserted near the front.
        for i in 0..4000 {
            let name = format!("file-{i:05}.txt");
            fs::write(a.join(&name), format!("contents {i}\n")).unwrap();
            fs::write(b.join(&name), format!("contents {i}\n")).unwrap();
        }
        fs::write(b.join("file-00007-inserted.txt"), "new\n").unwrap();

        let first = ingest_dir(&Sink::writing(&s), &a, &Ignore::empty(), &Default::default()).unwrap();
        let second = ingest_dir(&Sink::writing(&s), &b, &Ignore::empty(), &Default::default()).unwrap();
        assert_ne!(first.hash, second.hash);

        // Only the run containing the insertion (plus the spine above it) should
        // be new. A flat git-style tree would rewrite every byte of the listing.
        let rewritten = second.stats.bytes_written;
        assert!(
            rewritten < 40_000,
            "inserting one file rewrote {rewritten} bytes of directory listing"
        );
    }

    #[test]
    fn a_large_directory_becomes_several_runs() {
        let (s, dir) = tmp_store("runs");
        let d = dir.join("many");
        fs::create_dir_all(&d).unwrap();
        for i in 0..3000 {
            fs::write(d.join(format!("f-{i:05}")), "x").unwrap();
        }
        let ing = ingest_dir(&Sink::writing(&s), &d, &Ignore::empty(), &Default::default()).unwrap();

        match s.get(ing.hash).unwrap() {
            Object::Tree { children, .. } => assert!(
                children.len() > 1,
                "3000 entries should span multiple runs, got {}",
                children.len()
            ),
            other => panic!("expected a tree, got {:?}", other.kind()),
        }
        assert_eq!(read_entries(&s, ing.hash).unwrap().len(), 3000);
    }

    #[test]
    fn a_small_directory_is_a_single_run() {
        let (s, dir) = tmp_store("smalldir");
        let d = dir.join("few");
        fs::create_dir_all(&d).unwrap();
        for i in 0..5 {
            fs::write(d.join(format!("f{i}")), "x").unwrap();
        }
        let ing = ingest_dir(&Sink::writing(&s), &d, &Ignore::empty(), &Default::default()).unwrap();
        match s.get(ing.hash).unwrap() {
            Object::Tree { level, children } => {
                assert_eq!(level, 0);
                assert_eq!(children.len(), 1, "a small directory needs no fan-out");
            }
            other => panic!("expected a tree, got {:?}", other.kind()),
        }
    }

    #[test]
    fn directory_hash_is_independent_of_read_order() {
        let (s, dir) = tmp_store("order");
        let a = dir.join("wt-a");
        let b = dir.join("wt-b");

        // Same contents, created in opposite order.
        fs::create_dir_all(a.join("sub")).unwrap();
        fs::write(a.join("alpha.txt"), b"one").unwrap();
        fs::write(a.join("zeta.txt"), b"two").unwrap();
        fs::write(a.join("sub/nested"), b"three").unwrap();

        fs::create_dir_all(b.join("sub")).unwrap();
        fs::write(b.join("zeta.txt"), b"two").unwrap();
        fs::write(b.join("sub/nested"), b"three").unwrap();
        fs::write(b.join("alpha.txt"), b"one").unwrap();

        let ha = ingest_dir(&Sink::writing(&s), &a, &Ignore::empty(), &Default::default()).unwrap().hash;
        let hb = ingest_dir(&Sink::writing(&s), &b, &Ignore::empty(), &Default::default()).unwrap().hash;
        assert_eq!(ha, hb, "identical trees must hash identically");
        let _ = fs::remove_dir_all(dir);
    }
}
