//! Build history from a stream instead of from a working directory.
//!
//! # Why this exists
//!
//! Replaying a repository one commit at a time through the ordinary path costs
//! a full working-tree scan per commit. Importing git's own history that way
//! means around four hundred million `stat` calls to discover an average of two
//! changed files each time, plus a process for every commit. The chain itself
//! is unavoidably serial -- a commit names its parent, so it cannot be built
//! before its parent exists -- but almost none of that cost is.
//!
//! Here the previous commit's tree is already in memory, the stream says which
//! paths changed, and nothing touches a filesystem. Only what changed is
//! hashed, and only the trees above it are rebuilt.
//!
//! # The format
//!
//! Deliberately git's own `fast-import` stream, so
//!
//! ```text
//! git fast-export --all | fkit fast-import
//! ```
//!
//! works with no adapter in between. Inventing a format here would have been
//! less work and worth much less: the point of speaking this one is that every
//! tool that can produce history already produces this.
//!
//! Supported: `blob`, `commit`, `mark`, `data` (counted and delimited), `from`,
//! `merge`, `M`, `D`, `C`, `R`, `deleteall`, `reset`, `tag`, `checkpoint`,
//! `progress`, `feature`, `option`, `done`. Unknown directives are refused
//! rather than skipped, because quietly dropping part of a history is the one
//! failure mode an importer must not have.

use crate::hash::Hash;
use crate::object::{Commit, EntryKind, Object, TreeEntry};
use crate::store::{Sink, Store};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;

/// What a `:N` mark refers to.
#[derive(Debug, Clone)]
enum Mark {
    /// Content that has arrived but has not been given a path yet.
    ///
    /// A stream announces a blob before the commit that places it, so at the
    /// moment it arrives there is nothing to say which file it is a new
    /// version *of*. Storing it there and then means storing it whole, every
    /// time, for every version of every file -- which is most of what a
    /// repository is. Held instead until an `M` names its path, at which point
    /// the previous version of that path is known and the content can be
    /// stored as the difference from it.
    Pending(Vec<u8>),
    /// A file's content: its Merkle root and its length.
    Blob(Hash, u64),
    Commit(Hash),
}

/// How much unplaced blob content to hold before storing some of it whole.
///
/// A stream normally names a blob's path within a commit or two, so this stays
/// far below the cap. It exists so that a producer which front-loads every
/// blob cannot make the importer hold an entire repository in memory.
const PENDING_LIMIT: usize = 256 * 1024 * 1024;

/// One branch being built: every path it holds, and where its tip is.
///
/// The paths are kept flat and complete rather than as nested trees. A commit
/// then applies its changes to a map and the trees are derived from it, which
/// is far cheaper than walking and rewriting a tree structure per change --
/// and it is the same shape `ingest_dir` already builds from.
#[derive(Default, Clone)]
struct Branch {
    paths: BTreeMap<String, TreeEntry>,
    tip: Option<Hash>,
    tree: Option<Hash>,
}

#[derive(Debug)]
pub struct Report {
    pub commits: u64,
    pub blobs: u64,
    /// Submodule pins naming a repository that was not in the stream.
    pub skipped_submodules: u64,
    pub refs: Vec<(String, Hash)>,
}

pub struct FastImport<'a> {
    sink: &'a Sink<'a>,
    store: &'a Store,
    marks: HashMap<u32, Mark>,
    branches: HashMap<String, Branch>,
    commits: u64,
    blobs: u64,
    skipped_submodules: u64,
    pending_bytes: usize,
    every: u64,
    on_progress: Option<Box<dyn Fn(u64, u64) + 'a>>,
}

impl<'a> FastImport<'a> {
    pub fn new(sink: &'a Sink<'a>, store: &'a Store) -> Self {
        FastImport {
            sink,
            store,
            marks: HashMap::new(),
            branches: HashMap::new(),
            commits: 0,
            blobs: 0,
            skipped_submodules: 0,
            pending_bytes: 0,
            every: 0,
            on_progress: None,
        }
    }

    /// Report every `every` commits. An import of a real history runs for
    /// minutes, and a process that says nothing for minutes looks stuck.
    pub fn with_progress(mut self, every: u64, f: impl Fn(u64, u64) + 'a) -> Self {
        self.every = every;
        self.on_progress = Some(Box::new(f));
        self
    }

    /// Consume a whole stream. Returns what was built and where each ref ended.
    pub fn run<R: BufRead>(mut self, input: R) -> Result<Report> {
        let mut r = Reader::new(input);
        while let Some(line) = r.line()? {
            if line.is_empty() {
                continue;
            }
            let (word, rest) = split_once(&line);
            match word {
                "blob" => self.do_blob(&mut r)?,
                "commit" => self.do_commit(&mut r, rest)?,
                "reset" => self.do_reset(&mut r, rest)?,
                "tag" => self.do_tag(&mut r)?,
                // Nothing here needs a filesystem, so a checkpoint is a no-op
                // rather than a flush point.
                "checkpoint" | "done" => {}
                "progress" => eprintln!("{rest}"),
                // Capability negotiation. We answer to whatever we were given,
                // and the ones that would change the meaning of the stream are
                // refused below rather than ignored.
                "feature" | "option" => self.check_feature(rest)?,
                other => bail!("unsupported fast-import directive: {other}"),
            }
        }

        let refs = self
            .branches
            .iter()
            .filter_map(|(name, b)| b.tip.map(|t| (name.clone(), t)))
            .collect();
        Ok(Report {
            commits: self.commits,
            blobs: self.blobs,
            skipped_submodules: self.skipped_submodules,
            refs,
        })
    }

    /// Refuse the features that would silently change what we build.
    fn check_feature(&self, rest: &str) -> Result<()> {
        let name = rest.split('=').next().unwrap_or("").trim();
        match name {
            // Harmless: they describe the producer, not the content.
            "" | "date-format" | "export-marks" | "import-marks"
            | "import-marks-if-exists" | "force" | "quiet" | "stats"
            | "alias" | "get-mark" | "cat-blob" | "ls" | "notes" | "done" => Ok(()),
            other => bail!(
                "this stream asks for the `{other}` feature, which this importer \
                 does not implement; refusing rather than importing something \
                 different from what was exported"
            ),
        }
    }

    fn do_blob<R: BufRead>(&mut self, r: &mut Reader<R>) -> Result<()> {
        let mut mark = None;
        loop {
            let Some(line) = r.peek()? else { break };
            let (word, rest) = split_once(&line);
            match word {
                "mark" => {
                    mark = Some(parse_mark(rest)?);
                    r.take();
                }
                "data" => {
                    let rest = rest.to_string();
                    r.take();
                    let bytes = r.data(&rest)?;
                    self.blobs += 1;
                    match mark {
                        // Held until a path is known. See `Mark::Pending`.
                        Some(m) => {
                            self.pending_bytes += bytes.len();
                            self.marks.insert(m, Mark::Pending(bytes));
                            self.relieve_pressure()?;
                        }
                        // Nothing can refer to it later, so it is only useful
                        // now, and there is no path coming for it.
                        None => {
                            crate::ingest::ingest_bytes(self.sink, &bytes)?;
                        }
                    }
                    return Ok(());
                }
                _ => break,
            }
        }
        bail!("a blob arrived with no data")
    }

    fn do_commit<R: BufRead>(&mut self, r: &mut Reader<R>, refname: &str) -> Result<()> {
        let refname = refname.trim().to_string();
        if refname.is_empty() {
            bail!("a commit named no ref");
        }

        let mut mark = None;
        let mut author: Option<String> = None;
        let mut committer: Option<String> = None;
        let mut when: i64 = 0;
        let mut message = String::new();
        let mut from: Option<Hash> = None;
        let mut merges: Vec<Hash> = Vec::new();
        let mut changes: Vec<Change> = Vec::new();
        let mut saw_from = false;

        // The store, copied out of `self` so that holding a `Prior` built from
        // it does not borrow the importer for the rest of the loop.
        let store = self.store;
        let mut prior: Option<crate::ingest::Prior> = None;

        while let Some(line) = r.peek()? {
            let (word, rest) = split_once(&line);
            let word = word.to_string();
            let rest = rest.to_string();
            match word.as_str() {
                "mark" => {
                    mark = Some(parse_mark(&rest)?);
                    r.take();
                }
                "author" => {
                    let (who, ts) = parse_ident(&rest)?;
                    author = Some(who);
                    when = ts;
                    r.take();
                }
                "committer" => {
                    let (who, ts) = parse_ident(&rest)?;
                    committer = Some(who);
                    // The committer's time is the one git orders history by,
                    // and it is what a log should show.
                    when = ts;
                    r.take();
                }
                "encoding" | "original-oid" | "gpgsig" => {
                    r.take();
                }
                "data" => {
                    r.take();
                    message = String::from_utf8_lossy(&r.data(&rest)?).into_owned();
                }
                "from" => {
                    saw_from = true;
                    from = Some(self.resolve(&rest)?);
                    r.take();
                }
                "merge" => {
                    merges.push(self.resolve(&rest)?);
                    r.take();
                }
                "M" | "D" | "C" | "R" | "deleteall" => {
                    // The tree this commit is built on, needed before any
                    // content is stored so each new version can be kept as the
                    // difference from the old one. Safe to resolve here and
                    // not earlier: the format puts `from` and `merge` ahead of
                    // every file command, so by now the parent is known.
                    if prior.is_none() {
                        let base = if saw_from {
                            from
                        } else {
                            self.branches.get(&refname).and_then(|b| b.tip)
                        };
                        prior = Some(match base.and_then(|c| self.tree_of(c)) {
                            Some(t) => crate::ingest::Prior::at(store, t),
                            None => crate::ingest::Prior::none(),
                        });
                    }
                    r.take();
                    let p = prior.take().expect("just set");
                    let change = self.parse_change(r, &word, &rest, &p);
                    prior = Some(p);
                    changes.push(change?);
                }
                _ => break,
            }
        }

        // Where this commit starts from. `from` is explicit; without it, git's
        // rule is that a commit continues the branch it names.
        let mut branch = if saw_from {
            match from {
                Some(parent) => self.branch_at(parent)?,
                None => Branch::default(),
            }
        } else {
            self.branches.get(&refname).cloned().unwrap_or_default()
        };
        let parent_tree = branch.tree;

        for change in changes {
            self.apply(&mut branch.paths, change)?;
        }

        // Trees are derived from the flat map. Only the objects that actually
        // differ get written -- everything else is already in the store under
        // the same hash, so it costs a lookup and nothing more.
        let prior = match parent_tree {
            Some(t) => crate::ingest::Prior::at(self.store, t),
            None => crate::ingest::Prior::none(),
        };
        let (tree, _size) = crate::ingest::build_paths(self.sink, &branch.paths, &prior)?;

        let mut parents = Vec::new();
        if let Some(p) = from.or(branch.tip) {
            parents.push(p);
        }
        parents.extend(merges);

        let commit = Object::Commit(Commit {
            tree,
            parents,
            author: author.or(committer).unwrap_or_else(|| "unknown <>".into()),
            timestamp: when,
            message,
        });
        let (hash, _) = self.sink.put(&commit)?;

        branch.tip = Some(hash);
        branch.tree = Some(tree);
        if let Some(m) = mark {
            self.marks.insert(m, Mark::Commit(hash));
        }
        self.branches.insert(refname, branch);
        self.commits += 1;
        if self.every > 0
            && self.commits.is_multiple_of(self.every)
            && let Some(f) = &self.on_progress
        {
            f(self.commits, self.blobs);
        }
        Ok(())
    }

    fn do_reset<R: BufRead>(&mut self, r: &mut Reader<R>, refname: &str) -> Result<()> {
        let refname = refname.trim().to_string();
        if let Some(line) = r.peek()? {
            let (word, rest) = split_once(&line);
            if word == "from" {
                let rest = rest.to_string();
                r.take();
                let at = self.resolve(&rest)?;
                let b = self.branch_at(at)?;
                self.branches.insert(refname, b);
                return Ok(());
            }
        }
        // A reset with no `from` deletes the ref, which for our purposes means
        // it simply stops existing in the report.
        self.branches.remove(&refname);
        Ok(())
    }

    /// Annotated tags carry a payload we have nowhere to put, so the object is
    /// read past rather than stored. The lightweight ref is what matters and it
    /// arrives as a `reset`.
    fn do_tag<R: BufRead>(&mut self, r: &mut Reader<R>) -> Result<()> {
        while let Some(line) = r.peek()? {
            let (word, rest) = split_once(&line);
            let rest = rest.to_string();
            match word {
                "mark" | "from" | "tagger" | "original-oid" => {
                    r.take();
                }
                "data" => {
                    r.take();
                    let _ = r.data(&rest)?;
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        Ok(())
    }

    fn parse_change<R: BufRead>(
        &mut self,
        r: &mut Reader<R>,
        word: &str,
        rest: &str,
        prior: &crate::ingest::Prior,
    ) -> Result<Change> {
        match word {
            "deleteall" => Ok(Change::All),
            "D" => Ok(Change::Del(unquote(rest.trim()))),
            "C" | "R" => {
                let (a, b) = split_pair(rest)?;
                Ok(if word == "C" { Change::Copy(a, b) } else { Change::Move(a, b) })
            }
            "M" => {
                // `M <mode> <dataref> <path>`, where dataref is a mark, a hash,
                // or the word `inline` with the content on the next line.
                let mut it = rest.splitn(3, ' ');
                let mode = it.next().unwrap_or("").trim();
                let dataref = it.next().unwrap_or("").trim().to_string();
                let path = unquote(it.next().unwrap_or("").trim());
                let kind = match mode {
                    "100644" => EntryKind::File { exec: false },
                    "100755" => EntryKind::File { exec: true },
                    "120000" => EntryKind::Symlink,
                    "160000" => EntryKind::Submodule,
                    "040000" | "40000" => {
                        bail!("this stream modifies a whole subtree at {path}, which \
                               this importer does not implement")
                    }
                    other => bail!("unknown file mode {other} for {path}"),
                };
                let (hash, size) = if dataref == "inline" {
                    let Some(line) = r.line()? else { bail!("inline content ended early") };
                    let (w, dr) = split_once(&line);
                    if w != "data" {
                        bail!("expected data after an inline modify, got {w}");
                    }
                    let bytes = r.data(dr)?;
                    let was = prior.chunks_for(&path);
                    let ing =
                        crate::ingest::ingest_reader_after(self.sink, bytes.as_slice(), &was)?;
                    (ing.hash, ing.size)
                } else if let Some(m) = dataref.strip_prefix(':') {
                    let m: u32 = m.parse().context("a mark that is not a number")?;
                    match self.marks.get(&m) {
                        Some(Mark::Blob(h, n)) => (*h, *n),
                        Some(Mark::Commit(h)) => (*h, 0), // a submodule pin
                        // Held content, now that there is a path for it. The
                        // mark becomes an ordinary blob so that a second use
                        // of the same content costs nothing.
                        Some(Mark::Pending(_)) => {
                            let Some(Mark::Pending(bytes)) = self.marks.remove(&m) else {
                                unreachable!("just matched on Pending")
                            };
                            self.pending_bytes -= bytes.len();
                            let was = prior.chunks_for(&path);
                            let ing = crate::ingest::ingest_reader_after(
                                self.sink,
                                bytes.as_slice(),
                                &was,
                            )?;
                            self.marks.insert(m, Mark::Blob(ing.hash, ing.size));
                            (ing.hash, ing.size)
                        }
                        None => bail!("mark :{m} was used before it was defined"),
                    }
                } else if let Some(h) = Hash::from_hex(&dataref) {
                    // A hash this store can name, which for a submodule is a
                    // commit already imported here.
                    (h, 0)
                } else if kind == EntryKind::Submodule {
                    // A pin naming a commit in a repository that is not this
                    // one and was never in the stream. fkit points a submodule
                    // at a commit in this same store, so there is nothing
                    // truthful to record -- the entry is dropped and counted,
                    // and the count is reported, because a tree that quietly
                    // differs from the one exported is worse than a loud gap.
                    self.skipped_submodules += 1;
                    return Ok(Change::Skip);
                } else {
                    bail!(
                        "{path} refers to content that is not in this stream \
                         ({dataref}); a stream exported without its blobs cannot \
                         be imported"
                    )
                };
                Ok(Change::Set(path, kind, hash, size))
            }
            other => bail!("unknown change directive {other}"),
        }
    }

    fn apply(&self, paths: &mut BTreeMap<String, TreeEntry>, c: Change) -> Result<()> {
        match c {
            Change::Skip => {}
            Change::All => paths.clear(),
            Change::Set(path, kind, hash, size) => {
                let name = leaf(&path);
                paths.insert(path, TreeEntry { name, kind, hash, size });
            }
            Change::Del(path) => {
                // A path may name a file or a whole directory.
                paths.remove(&path);
                let prefix = format!("{path}/");
                paths.retain(|k, _| !k.starts_with(&prefix));
            }
            Change::Copy(from, to) | Change::Move(from, to) => {
                let moving = matches!(c_kind(&from, &to), ());
                let _ = moving;
                let mut moved: Vec<(String, TreeEntry)> = Vec::new();
                if let Some(e) = paths.get(&from) {
                    moved.push((to.clone(), e.clone()));
                }
                let prefix = format!("{from}/");
                for (k, v) in paths.iter() {
                    if let Some(tail) = k.strip_prefix(&prefix) {
                        moved.push((format!("{to}/{tail}"), v.clone()));
                    }
                }
                if moved.is_empty() {
                    // Copying something absent is not an error in the format;
                    // it simply has no effect.
                    return Ok(());
                }
                for (k, mut v) in moved {
                    v.name = leaf(&k);
                    paths.insert(k, v);
                }
            }
        }
        Ok(())
    }

    /// The tree a commit points at, if it is one we can read.
    fn tree_of(&self, commit: Hash) -> Option<Hash> {
        for b in self.branches.values() {
            if b.tip == Some(commit) {
                return b.tree;
            }
        }
        match self.store.get(commit) {
            Ok(Object::Commit(c)) => Some(c.tree),
            _ => None,
        }
    }

    /// Store held content whose path never arrived.
    ///
    /// Only reached by a producer that emits a great many blobs before placing
    /// any of them. Storing the oldest whole is a worse outcome than patching
    /// it, and a far better one than running out of memory.
    fn relieve_pressure(&mut self) -> Result<()> {
        if self.pending_bytes <= PENDING_LIMIT {
            return Ok(());
        }
        let marks: Vec<u32> = self
            .marks
            .iter()
            .filter(|(_, m)| matches!(m, Mark::Pending(_)))
            .map(|(k, _)| *k)
            .collect();
        for m in marks {
            if self.pending_bytes <= PENDING_LIMIT / 2 {
                break;
            }
            let Some(Mark::Pending(bytes)) = self.marks.remove(&m) else { continue };
            self.pending_bytes -= bytes.len();
            let ing = crate::ingest::ingest_bytes(self.sink, &bytes)?;
            self.marks.insert(m, Mark::Blob(ing.hash, ing.size));
        }
        Ok(())
    }

    /// The state a branch would have if it continued from `commit`.
    fn branch_at(&mut self, commit: Hash) -> Result<Branch> {
        // Almost always the tip of a branch we already hold, which costs
        // nothing. Otherwise the tree is read back out of the store.
        for b in self.branches.values() {
            if b.tip == Some(commit) {
                return Ok(b.clone());
            }
        }
        let Object::Commit(c) = self.store.get(commit)? else {
            bail!("{} is not a commit", commit.short());
        };
        let mut paths = BTreeMap::new();
        flatten(self.store, c.tree, "", &mut paths)?;
        Ok(Branch { paths, tip: Some(commit), tree: Some(c.tree) })
    }

    fn resolve(&self, r: &str) -> Result<Hash> {
        let r = r.trim();
        if let Some(m) = r.strip_prefix(':') {
            let m: u32 = m.parse().context("a mark that is not a number")?;
            return match self.marks.get(&m) {
                Some(Mark::Commit(h)) | Some(Mark::Blob(h, _)) => Ok(*h),
                // `from` and `merge` name commits. Blob content that has not
                // been placed yet is not one, and treating it as a parent
                // would build a history that never existed.
                Some(Mark::Pending(_)) => {
                    bail!("mark :{m} is file content, but was used where a commit belongs")
                }
                None => bail!("mark :{m} was used before it was defined"),
            };
        }
        if let Some(b) = self.branches.get(r).and_then(|b| b.tip) {
            return Ok(b);
        }
        Hash::from_hex(r).with_context(|| format!("cannot resolve {r}"))
    }
}

fn c_kind(_a: &str, _b: &str) {}

enum Change {
    Set(String, EntryKind, Hash, u64),
    /// Something the stream described that this repository cannot hold.
    Skip,
    Del(String),
    Copy(String, String),
    Move(String, String),
    All,
}

fn leaf(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Every file in a tree, by full path.
fn flatten(
    store: &Store,
    tree: Hash,
    prefix: &str,
    out: &mut BTreeMap<String, TreeEntry>,
) -> Result<()> {
    for e in crate::ingest::read_entries(store, tree)? {
        let path =
            if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
        match e.kind {
            EntryKind::Dir => flatten(store, e.hash, &path, out)?,
            _ => {
                out.insert(path, e);
            }
        }
    }
    Ok(())
}

fn split_once(line: &str) -> (&str, &str) {
    match line.find(' ') {
        Some(i) => (&line[..i], &line[i + 1..]),
        None => (line, ""),
    }
}

fn parse_mark(rest: &str) -> Result<u32> {
    rest.trim()
        .strip_prefix(':')
        .context("a mark must look like :N")?
        .parse()
        .context("a mark that is not a number")
}

/// `Name <email> <unix seconds> <timezone>` -> the name and the seconds.
fn parse_ident(rest: &str) -> Result<(String, i64)> {
    let close = rest.rfind('>').context("an identity with no closing angle bracket")?;
    let who = rest[..=close].trim().to_string();
    let tail = rest[close + 1..].trim();
    let secs = tail
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    Ok((who, secs))
}

fn split_pair(rest: &str) -> Result<(String, String)> {
    // Either `"quoted src" dst` or `src dst`.
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"').context("an unterminated quoted path")?;
        let a = unquote(&rest[..end + 2]);
        let b = unquote(rest[end + 2..].trim());
        return Ok((a, b));
    }
    let mut it = rest.splitn(2, ' ');
    let a = it.next().unwrap_or("").trim().to_string();
    let b = it.next().unwrap_or("").trim().to_string();
    if b.is_empty() {
        bail!("expected two paths, got {rest}");
    }
    Ok((a, unquote(&b)))
}

/// Undo the C-style quoting the format uses for awkward paths.
fn unquote(s: &str) -> String {
    let s = s.trim();
    if !(s.starts_with('"') && s.ends_with('"') && s.len() >= 2) {
        return s.to_string();
    }
    let inner = &s[1..s.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(d @ '0'..='7') => {
                let mut v = d.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match it.clone().next() {
                        Some(n @ '0'..='7') => {
                            v = v * 8 + n.to_digit(8).unwrap_or(0);
                            it.next();
                        }
                        _ => break,
                    }
                }
                out.push(char::from_u32(v).unwrap_or('?'));
            }
            Some(other) => out.push(other),
            None => break,
        }
    }
    out
}

/// A line reader that can also take exact byte counts, and look one line ahead.
struct Reader<R> {
    inner: R,
    held: Option<String>,
}

impl<R: BufRead> Reader<R> {
    fn new(inner: R) -> Self {
        Reader { inner, held: None }
    }

    fn line(&mut self) -> Result<Option<String>> {
        if let Some(l) = self.held.take() {
            return Ok(Some(l));
        }
        let mut buf = Vec::new();
        let n = self.inner.read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    }

    /// The next line without consuming it.
    ///
    /// Owned rather than borrowed: every caller looks at a line, decides, and
    /// then consumes it or hands the reader onward, and a borrow that outlives
    /// the decision makes all of that unexpressible.
    fn peek(&mut self) -> Result<Option<String>> {
        if self.held.is_none() {
            self.held = self.line()?;
        }
        Ok(self.held.clone())
    }

    fn take(&mut self) {
        self.held = None;
    }

    /// The payload of a `data` directive: either `<count>` or `<<delimiter`.
    fn data(&mut self, spec: &str) -> Result<Vec<u8>> {
        let spec = spec.trim();
        if let Some(delim) = spec.strip_prefix("<<") {
            let mut out = Vec::new();
            loop {
                let Some(l) = self.line()? else { bail!("delimited data never ended") };
                if l == delim {
                    return Ok(out);
                }
                out.extend_from_slice(l.as_bytes());
                out.push(b'\n');
            }
        }
        let n: usize = spec.parse().with_context(|| format!("bad data length: {spec}"))?;
        let mut buf = vec![0u8; n];
        std::io::Read::read_exact(&mut self.inner, &mut buf)?;
        // A trailing newline after the payload is optional in the format.
        if let Some(l) = self.line()?
            && !l.is_empty()
        {
            self.held = Some(l);
        }
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "fkit-fastimport-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn import(tag: &str, stream: &str) -> (Store, Report) {
        let dir = tmp(tag);
        let store = Store::open(dir.join("objects")).unwrap();
        let report = {
            let sink = Sink::writing(&store);
            FastImport::new(&sink, &store).run(stream.as_bytes()).unwrap()
        };
        (store, report)
    }

    /// Read a path out of a commit, so a test can assert on content rather
    /// than on hashes it would have to hardcode.
    fn read(store: &Store, commit: Hash, path: &str) -> Option<String> {
        let Object::Commit(c) = store.get(commit).ok()? else { return None };
        let mut at = c.tree;
        let mut parts = path.split('/').peekable();
        while let Some(name) = parts.next() {
            let e = crate::ingest::read_entries(store, at)
                .ok()?
                .into_iter()
                .find(|e| e.name == name)?;
            if parts.peek().is_some() {
                at = e.hash;
            } else {
                let mut out = Vec::new();
                crate::ingest::read_file(store, e.hash, &mut out).ok()?;
                return Some(String::from_utf8_lossy(&out).into_owned());
            }
        }
        None
    }

    const TWO: &str = "\
blob
mark :1
data 6
hello

commit refs/heads/main
mark :2
author Ada <ada@example.com> 1000000000 +0000
committer Ada <ada@example.com> 1000000000 +0000
data 5
first
M 100644 :1 a.txt

blob
mark :3
data 6
world

commit refs/heads/main
mark :4
author Ada <ada@example.com> 1000000100 +0000
committer Ada <ada@example.com> 1000000100 +0000
data 6
second
from :2
M 100644 :3 b.txt

";

    #[test]
    fn builds_a_history_from_a_stream() {
        let (store, r) = import("basic", TWO);
        assert_eq!(r.commits, 2);
        assert_eq!(r.blobs, 2);

        let (_, tip) = r.refs.iter().find(|(n, _)| n == "refs/heads/main").unwrap();
        // The second commit keeps the first commit's file: a change names only
        // what changed, and everything else is carried by the tree.
        assert_eq!(read(&store, *tip, "a.txt").as_deref(), Some("hello\n"));
        assert_eq!(read(&store, *tip, "b.txt").as_deref(), Some("world\n"));

        let Object::Commit(c) = store.get(*tip).unwrap() else { panic!("not a commit") };
        assert_eq!(c.message, "second");
        assert_eq!(c.timestamp, 1000000100);
        assert_eq!(c.parents.len(), 1);
    }

    #[test]
    fn a_delete_removes_the_path_and_nothing_else() {
        let stream = format!(
            "{TWO}commit refs/heads/main
author Ada <ada@example.com> 1000000200 +0000
committer Ada <ada@example.com> 1000000200 +0000
data 5
third
D a.txt

"
        );
        let (store, r) = import("delete", &stream);
        assert_eq!(r.commits, 3);
        let (_, tip) = r.refs.iter().find(|(n, _)| n == "refs/heads/main").unwrap();
        assert_eq!(read(&store, *tip, "a.txt"), None, "the deleted path is still there");
        assert_eq!(read(&store, *tip, "b.txt").as_deref(), Some("world\n"));
    }

    #[test]
    fn nested_paths_and_inline_content() {
        let stream = "\
commit refs/heads/main
author Ada <ada@example.com> 1 +0000
committer Ada <ada@example.com> 1 +0000
data 4
deep
M 100644 inline src/very/deep/file.txt
data 5
here

";
        let (store, r) = import("nested", stream);
        let (_, tip) = r.refs.iter().find(|(n, _)| n == "refs/heads/main").unwrap();
        assert_eq!(read(&store, *tip, "src/very/deep/file.txt").as_deref(), Some("here\n"));
    }

    #[test]
    fn an_unknown_directive_is_refused_rather_than_skipped() {
        let dir = tmp("unknown");
        let store = Store::open(dir.join("objects")).unwrap();
        let sink = Sink::writing(&store);
        // Silently ignoring something we do not understand would import a
        // history that is not the one that was exported.
        let err = FastImport::new(&sink, &store)
            .run("frobnicate everything\n".as_bytes())
            .unwrap_err();
        assert!(
            err.to_string().contains("frobnicate"),
            "the error should name what it did not understand, got: {err}"
        );
    }

    #[test]
    fn a_mark_used_before_it_is_defined_is_an_error() {
        let dir = tmp("badmark");
        let store = Store::open(dir.join("objects")).unwrap();
        let sink = Sink::writing(&store);
        let err = FastImport::new(&sink, &store)
            .run(
                "commit refs/heads/main
author A <a@b> 1 +0000
committer A <a@b> 1 +0000
data 1
x
M 100644 :99 f.txt

"
                .as_bytes(),
            )
            .unwrap_err();
        assert!(err.to_string().contains(":99"), "got: {err}");
    }
}
