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
pub fn ingest_file(sink: &Sink, path: &Path) -> Result<Ingested> {
    let f = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    ingest_reader(sink, std::io::BufReader::new(f))
}

pub fn ingest_bytes(sink: &Sink, bytes: &[u8]) -> Result<Ingested> {
    ingest_reader(sink, bytes)
}

pub fn ingest_reader<R: std::io::Read>(sink: &Sink, reader: R) -> Result<Ingested> {
    let mut stats = WriteStats::default();

    // Pass 1: cut into content-defined chunks and store each leaf.
    let mut level_refs: Vec<(Hash, u64)> = Vec::new();
    for chunk in Chunker::new(reader) {
        let chunk = chunk?;
        let len = chunk.len() as u64;
        let (h, st) = sink.put(&Object::Chunk(chunk))?;
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
            return Ok(Ingested { hash: h, size: total, stats });
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
    debug_assert!(
        entries.windows(2).all(|w| w[0].name <= w[1].name),
        "entries must be sorted before hashing"
    );

    let total_size: u64 = entries.iter().map(|e| e.size).sum();
    let total_entries = entries.len() as u32;

    let mut level_refs: Vec<TreeChild> = Vec::new();
    for run in cut_runs(&entries) {
        let size = run.iter().map(|e| e.size).sum();
        let (h, _) = sink.put(&Object::Entries(run.to_vec()))?;
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

fn enumerate(root: &Path, ignore: &Ignore) -> Result<Skeleton> {
    let mut sk = Skeleton { jobs: Vec::new(), dirs: Vec::new() };
    walk(root, "", ignore, &mut sk)?;
    // Largest first: with a few very large files among many small ones, feeding
    // the big ones out first stops one thread finishing last on a 1 GB image
    // while the rest idle.
    sk.jobs.sort_by(|a, b| {
        let size = |j: &Job| fs::symlink_metadata(&j.abs).map(|m| m.len()).unwrap_or(0);
        size(b).cmp(&size(a))
    });
    Ok(sk)
}

fn walk(dir: &Path, prefix: &str, ignore: &Ignore, out: &mut Skeleton) -> Result<()> {
    let read = fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in read {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let ft = entry.file_type()?;
        if ignore.matches(&name, ft.is_dir()) {
            continue;
        }
        let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let abs = entry.path();

        if ft.is_symlink() {
            out.jobs.push(Job { rel, abs, kind: JobKind::Symlink });
        } else if ft.is_dir() {
            out.dirs.push(rel.clone());
            walk(&abs, &rel, ignore, out)?;
        } else {
            let exec = is_executable(&entry.metadata()?);
            out.jobs.push(Job { rel, abs, kind: JobKind::File { exec } });
        }
    }
    Ok(())
}

/// Ingest one job's content. Runs on a worker thread.
fn run_job(sink: &Sink, job: &Job) -> Result<(String, TreeEntry, WriteStats)> {
    let (hash, size, stats, kind) = match job.kind {
        JobKind::Symlink => {
            // Store the link *target* as content. A symlink then costs one small
            // chunk rather than a copy of whatever it points at.
            let target = fs::read_link(&job.abs)?;
            let ing = ingest_bytes(sink, target.to_string_lossy().as_bytes())?;
            (ing.hash, ing.size, ing.stats, EntryKind::Symlink)
        }
        JobKind::File { exec } => {
            let ing = ingest_file(sink, &job.abs)?;
            (ing.hash, ing.size, ing.stats, EntryKind::File { exec })
        }
    };
    let name = job.rel.rsplit('/').next().unwrap_or(&job.rel).to_string();
    Ok((job.rel.clone(), TreeEntry { name, kind, hash, size }, stats))
}

/// Snapshot a directory into a `Tree`, reading files on every available core.
pub fn ingest_dir(sink: &Sink, dir: &Path, ignore: &Ignore) -> Result<Ingested> {
    let sk = enumerate(dir, ignore)?;

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(sk.jobs.len().max(1));

    let next = std::sync::atomic::AtomicUsize::new(0);
    let jobs = &sk.jobs;

    // Scoped threads: no 'static bound, so `sink` and `jobs` are borrowed
    // directly and there is nothing to clone or Arc.
    let collected: Vec<Result<Vec<(String, TreeEntry, WriteStats)>>> =
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let next = &next;
                    scope.spawn(move || {
                        let mut mine = Vec::new();
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(job) = jobs.get(i) else { break };
                            mine.push(run_job(sink, job)?);
                        }
                        Ok(mine)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("ingest worker panicked")).collect()
        });

    let mut files: BTreeMap<String, TreeEntry> = BTreeMap::new();
    let mut stats = WriteStats::default();
    for batch in collected {
        for (rel, entry, st) in batch? {
            stats.merge(st);
            files.insert(rel, entry);
        }
    }

    // Tree building is serial and deterministic: identical contents must give an
    // identical hash no matter how the work was scheduled.
    let (hash, size) = build_nested(sink, &files, &sk.dirs, "")?;
    Ok(Ingested { hash, size, stats })
}

/// Assemble nested trees from flat paths, creating empty directories where the
/// skeleton recorded one but no file lives under it.
fn build_nested(
    sink: &Sink,
    files: &BTreeMap<String, TreeEntry>,
    dirs: &[String],
    prefix: &str,
) -> Result<(Hash, u64)> {
    let strip = |path: &str| -> Option<String> {
        if prefix.is_empty() {
            Some(path.to_string())
        } else {
            path.strip_prefix(prefix)?.strip_prefix('/').map(str::to_string)
        }
    };

    let mut entries: Vec<TreeEntry> = Vec::new();
    let mut subdirs: std::collections::BTreeSet<String> = Default::default();

    for path in files.keys() {
        let Some(rest) = strip(path) else { continue };
        match rest.split_once('/') {
            None => entries.push(files[path].clone()),
            Some((dir, _)) => {
                subdirs.insert(dir.to_string());
            }
        }
    }
    // Directories with no files under them would otherwise vanish.
    for d in dirs {
        let Some(rest) = strip(d) else { continue };
        if !rest.contains('/') {
            subdirs.insert(rest);
        }
    }

    for name in subdirs {
        let child_prefix =
            if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let (hash, size) = build_nested(sink, files, dirs, &child_prefix)?;
        entries.push(TreeEntry { name, kind: EntryKind::Dir, hash, size });
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let (hash, size, _) = build_tree(sink, entries)?;
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

/// Dead-simple ignore rules, read from `.fkitignore` or `.fkthat`.
///
/// Supported patterns (deliberately minimal — no full glob engine):
///   * `target`    — matches any component with that exact name
///   * `*.log`     — suffix match
///   * `build/`    — matches directories only
///   * `# comment` and blank lines are skipped
pub struct Ignore {
    names: Vec<String>,
    suffixes: Vec<String>,
    dirs: Vec<String>,
}

/// Both names for the same file, in the order they are read.
///
/// If a repository has both, both apply. The alternative — first one wins —
/// means adding the second file silently does nothing, and a rule that is
/// present in the tree and quietly not in effect is the worst outcome an
/// ignore file can produce.
pub const IGNORE_FILES: [&str; 2] = [".fkitignore", ".fkthat"];

impl Ignore {
    pub fn empty() -> Ignore {
        Ignore { names: vec![], suffixes: vec![], dirs: vec![] }
    }

    pub fn load(repo_root: &Path) -> Ignore {
        let mut ig = Ignore::empty();
        // The repo's own metadata is never part of a snapshot.
        ig.names.push(".fkit".into());

        for file in IGNORE_FILES {
            if let Ok(text) = fs::read_to_string(repo_root.join(file)) {
                ig.extend_from(&text);
            }
        }
        ig
    }

    /// Parse rules out of one ignore file's contents.
    fn extend_from(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_suffix('/') {
                self.dirs.push(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("*.") {
                self.suffixes.push(format!(".{rest}"));
            } else {
                self.names.push(line.to_string());
            }
        }
    }

    pub fn matches(&self, name: &str, is_dir: bool) -> bool {
        self.names.iter().any(|n| n == name)
            || (is_dir && self.dirs.iter().any(|d| d == name))
            || self.suffixes.iter().any(|s| name.ends_with(s.as_str()))
    }
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

        let first = ingest_dir(&Sink::writing(&s), &a, &Ignore::empty()).unwrap();
        let second = ingest_dir(&Sink::writing(&s), &b, &Ignore::empty()).unwrap();
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
        let ing = ingest_dir(&Sink::writing(&s), &d, &Ignore::empty()).unwrap();

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
        let ing = ingest_dir(&Sink::writing(&s), &d, &Ignore::empty()).unwrap();
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

        let ha = ingest_dir(&Sink::writing(&s), &a, &Ignore::empty()).unwrap().hash;
        let hb = ingest_dir(&Sink::writing(&s), &b, &Ignore::empty()).unwrap().hash;
        assert_eq!(ha, hb, "identical trees must hash identically");
        let _ = fs::remove_dir_all(dir);
    }
}
