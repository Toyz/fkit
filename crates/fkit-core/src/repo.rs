//! The repository: everything mutable in a system built out of immutable parts.
//!
//! ```text
//!   .fkit/
//!     objects/          the CAS — append-only, immutable, content-named
//!     refs/heads/main   a branch: 64 hex chars naming a commit
//!     HEAD              which branch (or commit) we are on
//!     config            author name, remote URL
//! ```
//!
//! Note how little of this is mutable. Objects are never modified or deleted;
//! the *only* thing that ever changes is which commit a ref points at. A branch
//! is a one-line file. That is the entire difference between "history" and
//! "a pile of hashed data".
//!
//! # No staging area, on purpose
//!
//! fkit has no index. `commit` snapshots the working tree as it is. Git's index
//! exists largely because hashing every file on every status check was too slow
//! in 2005; with BLAKE3 and content-defined chunking, re-snapshotting is cheap
//! enough that the extra concept is not worth its cost in confusion. If you want
//! partial commits later, the honest way to add them is a "staged tree hash",
//! not a parallel mutable index format.

use crate::hash::Hash;
use crate::ingest::{ingest_dir, Ignore};
use crate::object::{Commit, EntryKind, Object, TreeEntry};
use crate::store::{Sink, Store};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const META_DIR: &str = ".fkit";
pub const DEFAULT_BRANCH: &str = "main";

pub struct Repo {
    pub root: PathBuf,
    pub store: Store,
}

/// What HEAD points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// On a branch. Committing advances the branch.
    Branch(String),
    /// Detached at a specific commit. Committing does not move any branch.
    Detached(Hash),
}

impl Repo {
    fn meta(&self) -> PathBuf {
        self.root.join(META_DIR)
    }

    /// Create a new repository rooted at `path`.
    pub fn init(path: &Path) -> Result<Repo> {
        let meta = path.join(META_DIR);
        if meta.exists() {
            bail!("{} already contains a fkit repository", path.display());
        }
        fs::create_dir_all(meta.join("objects"))?;
        fs::create_dir_all(meta.join("refs").join("heads"))?;
        fs::write(meta.join("HEAD"), format!("ref: refs/heads/{DEFAULT_BRANCH}\n"))?;
        fs::write(
            meta.join("config"),
            "# fkit repository config\n\
             # author = Your Name <you@example.com>\n\
             # remote = ws://localhost:7420/your-repo\n",
        )?;
        Repo::open(path)
    }

    pub fn open(root: &Path) -> Result<Repo> {
        let store = Store::open(root.join(META_DIR).join("objects"))?;
        Ok(Repo { root: root.to_path_buf(), store })
    }

    /// Walk upward from `start` looking for a `.fkit` directory, the way `git`
    /// and `cargo` locate their roots.
    pub fn discover(start: &Path) -> Result<Repo> {
        let mut cur = fs::canonicalize(start)?;
        loop {
            if cur.join(META_DIR).is_dir() {
                return Repo::open(&cur);
            }
            match cur.parent() {
                Some(p) => cur = p.to_path_buf(),
                None => bail!(
                    "not a fkit repository (no {META_DIR} directory found in {} or any parent)",
                    start.display()
                ),
            }
        }
    }

    // ---- config ---------------------------------------------------------

    /// A config value: this repository first, then the user-level file.
    ///
    /// The layering is what makes `author` a thing you set once rather than in
    /// every repository you create.
    pub fn config_get(&self, key: &str) -> Option<String> {
        self.config_get_local(key)
            .or_else(|| crate::config::global_get(key))
    }

    /// Only this repository's own config, ignoring the user-level file.
    pub fn config_get_local(&self, key: &str) -> Option<String> {
        let text = fs::read_to_string(self.meta().join("config")).ok()?;
        crate::config::parse(&text, key)
    }

    pub fn config_set(&self, key: &str, value: &str) -> Result<()> {
        let path = self.meta().join("config");
        let text = fs::read_to_string(&path).unwrap_or_default();
        let mut out = String::new();
        let mut replaced = false;
        for line in text.lines() {
            let is_key = line
                .split_once('=')
                .map(|(k, _)| !line.trim_start().starts_with('#') && k.trim() == key)
                .unwrap_or(false);
            if is_key {
                if !replaced {
                    out.push_str(&format!("{key} = {value}\n"));
                    replaced = true;
                }
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        if !replaced {
            out.push_str(&format!("{key} = {value}\n"));
        }
        fs::write(&path, out)?;
        Ok(())
    }

    /// The identity recorded on a commit.
    ///
    /// Configured as two keys rather than one, because `author = "Name <email>"`
    /// silently requires knowing a formatting convention — set the wrong shape
    /// and it is baked into commits before anyone notices. `author.name` and
    /// `author.email` cannot be misread.
    ///
    /// The single-key `author` is still honoured for repositories configured
    /// before the split.
    pub fn author(&self) -> String {
        let name = self.config_get("author.name");
        let email = self.config_get("author.email");

        match (name, email) {
            (Some(n), Some(e)) if !n.is_empty() && !e.is_empty() => format!("{n} <{e}>"),
            (Some(n), _) if !n.is_empty() => n,
            (None, Some(e)) if !e.is_empty() => e,
            _ => self
                .config_get("author")
                .or_else(|| std::env::var("FKIT_AUTHOR").ok())
                .or_else(|| std::env::var("USER").ok())
                .unwrap_or_else(|| "unknown".to_string()),
        }
    }

    /// True when nothing identifies the committer beyond a login name.
    pub fn author_is_default(&self) -> bool {
        self.config_get("author.name").is_none()
            && self.config_get("author.email").is_none()
            && self.config_get("author").is_none()
            && std::env::var("FKIT_AUTHOR").is_err()
    }

    // ---- refs and HEAD --------------------------------------------------

    pub fn head(&self) -> Result<Head> {
        let text = fs::read_to_string(self.meta().join("HEAD"))
            .context("reading HEAD")?
            .trim()
            .to_string();
        if let Some(r) = text.strip_prefix("ref: refs/heads/") {
            Ok(Head::Branch(r.to_string()))
        } else if let Some(h) = Hash::from_hex(&text) {
            Ok(Head::Detached(h))
        } else {
            bail!("HEAD is corrupt: {text:?}")
        }
    }

    pub fn set_head(&self, head: &Head) -> Result<()> {
        let text = match head {
            Head::Branch(b) => format!("ref: refs/heads/{b}\n"),
            Head::Detached(h) => format!("{h}\n"),
        };
        fs::write(self.meta().join("HEAD"), text)?;
        Ok(())
    }

    fn ref_path(&self, branch: &str) -> PathBuf {
        self.meta().join("refs").join("heads").join(branch)
    }

    /// Tags travel in the same ref namespace as branches, distinguished by
    /// this prefix. One namespace keeps the wire protocol and the hub's refs
    /// table unchanged — a tag is a ref like any other — while a branch called
    /// `tags/x` is impossible because the prefix is stripped before a branch
    /// name is ever validated.
    pub const TAG_PREFIX: &'static str = "tags/";

    fn tag_path(&self, name: &str) -> PathBuf {
        self.meta().join("refs").join("tags").join(name)
    }

    pub fn read_tag(&self, name: &str) -> Result<Option<Hash>> {
        read_hash_file(&self.tag_path(name))
    }

    /// Point a tag at a commit.
    ///
    /// Refuses to move an existing tag unless `force`. A tag is a claim about
    /// what a name meant at a moment; silently repointing it makes every
    /// checkout of that name a different tree, and nothing downstream can tell.
    pub fn write_tag(&self, name: &str, commit: Hash, force: bool) -> Result<()> {
        if !force && let Some(old) = self.read_tag(name)? {
            if old == commit {
                return Ok(());
            }
            bail!("tag '{name}' already points at {} — pass --force to move it", old.short());
        }
        let p = self.tag_path(name);
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(p, format!("{commit}\n"))?;
        Ok(())
    }

    pub fn delete_tag(&self, name: &str) -> Result<()> {
        fs::remove_file(self.tag_path(name))
            .with_context(|| format!("no such tag: {name}"))?;
        Ok(())
    }

    pub fn list_tags(&self) -> Result<BTreeMap<String, Hash>> {
        read_ref_dir(&self.meta().join("refs").join("tags"))
    }

    pub fn read_ref(&self, branch: &str) -> Result<Option<Hash>> {
        read_hash_file(&self.ref_path(branch))
    }

    pub fn write_ref(&self, branch: &str, commit: Hash) -> Result<()> {
        let p = self.ref_path(branch);
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(p, format!("{commit}\n"))?;
        Ok(())
    }

    pub fn delete_ref(&self, branch: &str) -> Result<()> {
        fs::remove_file(self.ref_path(branch))
            .with_context(|| format!("no such branch: {branch}"))?;
        Ok(())
    }

    pub fn list_refs(&self) -> Result<BTreeMap<String, Hash>> {
        read_ref_dir(&self.meta().join("refs").join("heads"))
    }

    /// Every ref, in the one namespace the protocol and the hub speak: branches
    /// under their own names, tags under `tags/`.
    pub fn all_refs(&self) -> Result<BTreeMap<String, Hash>> {
        let mut out = self.list_refs()?;
        for (name, hash) in self.list_tags()? {
            out.insert(format!("{}{name}", Self::TAG_PREFIX), hash);
        }
        Ok(out)
    }

    // ---- the stash ----
    //
    // Work set aside so the working tree can go back to HEAD. A stash is an
    // ordinary commit with the working tree as its content and HEAD as its
    // parent, kept alive by a ref like anything else — this store has no
    // notion of a dangling object worth keeping, and would rather not grow
    // one.
    //
    // Deliberately outside `all_refs`, which is the namespace the wire
    // protocol and the hub share. A stash is unfinished work on one machine;
    // pushing it would publish it, and a hub receiving a ref it has no concept
    // of is a worse problem than that.

    fn stash_dir(&self) -> PathBuf {
        self.meta().join("refs").join("stash")
    }

    /// The stash stack, newest first.
    ///
    /// Entries are numbered by when they were made and never renumbered, so a
    /// hash written down stays valid after something below it is dropped. The
    /// position shown to a person is the index in this list.
    pub fn list_stashes(&self) -> Result<Vec<(u64, Hash)>> {
        let dir = self.stash_dir();
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&dir) else { return Ok(out) };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(n) = name.parse::<u64>() else { continue };
            if let Some(h) = read_hash_file(&e.path())? {
                out.push((n, h));
            }
        }
        out.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
        Ok(out)
    }

    /// Record a stash, returning the number it was given.
    pub fn push_stash(&self, commit: Hash) -> Result<u64> {
        let next = self.list_stashes()?.first().map(|(n, _)| n + 1).unwrap_or(0);
        let dir = self.stash_dir();
        fs::create_dir_all(&dir)?;
        fs::write(dir.join(next.to_string()), format!("{commit}
"))?;
        Ok(next)
    }

    pub fn drop_stash(&self, n: u64) -> Result<()> {
        fs::remove_file(self.stash_dir().join(n.to_string()))
            .with_context(|| format!("no stash numbered {n}"))?;
        Ok(())
    }

    /// The commit HEAD currently resolves to, if any. `None` on a fresh repo
    /// whose branch has no commits yet.
    pub fn head_commit(&self) -> Result<Option<Hash>> {
        match self.head()? {
            Head::Branch(b) => self.read_ref(&b),
            Head::Detached(h) => Ok(Some(h)),
        }
    }

    // ---- merge state ----
    //
    // A conflicted merge deliberately does NOT commit. The merged tree (markers
    // and all) goes into the working directory and `MERGE_HEAD` records the
    // other parent, so the eventual `commit` still records two parents. Without
    // this the resolution would land as an ordinary commit and the history
    // would claim the branches never merged.

    fn merge_head_path(&self) -> PathBuf {
        self.meta().join("MERGE_HEAD")
    }

    pub fn merge_head(&self) -> Result<Option<Hash>> {
        match fs::read_to_string(self.merge_head_path()) {
            Ok(s) => Ok(Hash::from_hex(s.trim())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_merge_head(&self, h: Hash) -> Result<()> {
        fs::write(self.merge_head_path(), format!("{h}\n"))?;
        Ok(())
    }

    pub fn clear_merge_head(&self) {
        let _ = fs::remove_file(self.merge_head_path());
    }

    pub fn head_tree(&self) -> Result<Option<Hash>> {
        match self.head_commit()? {
            None => Ok(None),
            Some(c) => match self.store.get(c)? {
                Object::Commit(c) => Ok(Some(c.tree)),
                other => bail!("HEAD points at a {}, not a commit", other.kind().name()),
            },
        }
    }

    // ---- snapshotting and committing ------------------------------------

    pub fn ignore(&self) -> Ignore {
        Ignore::load(&self.root)
    }

    /// Hash the working tree *without writing anything*. Used by `status`,
    /// `diff`, and the cleanliness check in `checkout`.
    ///
    /// The returned snapshot carries the tree objects in memory, so you can
    /// walk and diff it even though nothing was persisted.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let sink = Sink::dry(&self.store);
        let ing = ingest_dir(&sink, &self.root, &self.ignore(), &self.mounts()?)?;
        Ok(Snapshot {
            hash: ing.hash,
            size: ing.size,
            stats: ing.stats,
            objects: sink.into_retained(),
        })
    }

    /// Hash the working tree and persist every object.
    ///
    /// The tree at HEAD is handed to ingest as the version this one is a
    /// revision of, so each chunk can be stored as a patch against whatever
    /// occupied its position before. That is only ever a hint: a first commit
    /// has none, and a wrong guess is noticed and discarded by the pack.
    pub fn snapshot_writing(&self) -> Result<crate::ingest::Ingested> {
        let prior = self.head_tree().unwrap_or(None);
        let prior = match prior {
            Some(root) => crate::ingest::Prior::at(&self.store, root),
            None => crate::ingest::Prior::none(),
        };
        crate::ingest::ingest_dir_after(
            &Sink::writing(&self.store),
            &self.root,
            &self.ignore(),
            &self.mounts()?,
            &prior,
        )
    }


    /// Submodule pins as currently recorded beside this repository.
    ///
    /// This is what a new commit will pin. The tree of an *existing* commit is
    /// the authority on what that commit pinned; the two are compared by
    /// `status`, which is how a submodule that moved shows up as a change.
    pub fn mounts(&self) -> Result<crate::ingest::Mounts> {
        Ok(crate::submodule::list(self)?
            .into_iter()
            .map(|(path, m)| (path, m.pin))
            .collect())
    }

    /// A read view over the store alone.
    pub fn view(&self) -> View<'_> {
        View { store: &self.store, overlay: Default::default() }
    }

    /// A read view over the store *plus* an unwritten snapshot's trees.
    pub fn view_with<'a>(&'a self, snap: &Snapshot) -> View<'a> {
        View { store: &self.store, overlay: snap.objects.clone() }
    }

    pub fn commit(&self, message: &str) -> Result<CommitResult> {
        self.commit_as(message, &CommitAs::default())
    }

    /// Commit, optionally recording someone else's name and someone else's
    /// clock.
    ///
    /// This exists for importers. Replaying a history from elsewhere has to
    /// preserve who wrote each commit and when, or the result is a single
    /// author committing an entire project in the same second — which is not a
    /// history, only a shape that resembles one.
    ///
    /// Both fields are part of the commit's hash, so an import is reproducible:
    /// replaying the same source twice produces the same fkit commits.
    pub fn commit_as(&self, message: &str, who: &CommitAs) -> Result<CommitResult> {
        let snap = self.snapshot_writing()?;
        let parent = self.head_commit()?;
        let merging = self.merge_head()?;

        // Refuse an empty commit: if the tree is unchanged there is genuinely
        // nothing new to record, and a commit whose only difference is its
        // timestamp is noise.
        //
        // A merge in progress is the exception. Recording that two histories
        // joined is meaningful even when the merged tree happens to equal ours.
        if merging.is_none()
            && let Some(p) = parent
            && let Object::Commit(pc) = self.store.get(p)?
            && pc.tree == snap.hash
        {
            bail!("nothing to commit: the working tree matches HEAD");
        }

        // The second parent comes from MERGE_HEAD, so a conflicted merge that
        // is resolved and committed later still records both histories.
        let mut parents: Vec<Hash> = parent.into_iter().collect();
        if let Some(other) = merging
            && !parents.contains(&other)
        {
            parents.push(other);
        }

        let commit = Commit {
            tree: snap.hash,
            parents,
            author: who.author.clone().unwrap_or_else(|| self.author()),
            timestamp: who.timestamp.unwrap_or_else(now_unix),
            message: message.to_string(),
        };
        let (id, _) = self.store.put(&Object::Commit(commit))?;

        match self.head()? {
            Head::Branch(b) => self.write_ref(&b, id)?,
            // On a detached HEAD we move HEAD itself, so the commit is not lost.
            Head::Detached(_) => self.set_head(&Head::Detached(id))?,
        }
        self.clear_merge_head();

        Ok(CommitResult { commit: id, tree: snap.hash, stats: snap.stats })
    }

    /// Walk first-parent history from `start`, newest first.
    pub fn history(&self, start: Hash, limit: usize) -> Result<Vec<(Hash, Commit)>> {
        let mut out = Vec::new();
        let mut cur = Some(start);
        while let Some(h) = cur {
            if out.len() >= limit {
                break;
            }
            match self.store.get(h)? {
                Object::Commit(c) => {
                    cur = c.parents.first().copied();
                    out.push((h, c));
                }
                other => bail!("{} is a {}, not a commit", h.short(), other.kind().name()),
            }
        }
        Ok(out)
    }

    /// Flatten a tree into `path -> entry`, recursing into subdirectories.
    pub fn walk_tree(&self, tree: Hash) -> Result<BTreeMap<String, TreeEntry>> {
        self.view().walk_tree(tree)
    }
}

/// Trees nest, and submodules nest through them. Content addressing rules out a
/// real cycle, since a tree would have to contain its own hash to make one, but
/// a damaged store can return anything and blowing the stack is a poor way to
/// discover that.
const MAX_TREE_DEPTH: usize = 100;

/// A read-only view of objects: an in-memory overlay checked first, then the
/// on-disk store. This is what lets `status` diff a snapshot that was never
/// written to disk.
pub struct View<'a> {
    pub store: &'a Store,
    pub overlay: std::collections::HashMap<Hash, Object>,
}

impl<'a> View<'a> {
    /// Flatten a directory's runs into one sorted entry list.
    pub fn read_entries(&self, tree: Hash) -> Result<Vec<TreeEntry>> {
        let mut out = Vec::new();
        self.collect(tree, &mut out)?;
        Ok(out)
    }

    fn collect(&self, node: Hash, out: &mut Vec<TreeEntry>) -> Result<()> {
        match self.get(node)? {
            Object::Entries(e) => out.extend(e),
            Object::Tree { children, .. } => {
                for c in children {
                    self.collect(c.hash, out)?;
                }
            }
            other => bail!("expected a tree node, found a {}", other.kind().name()),
        }
        Ok(())
    }

    pub fn get(&self, h: Hash) -> Result<Object> {
        match self.overlay.get(&h) {
            Some(o) => Ok(o.clone()),
            None => self.store.get(h),
        }
    }

    /// Every path in a tree, with submodules expanded into their content.
    ///
    /// Expanding here is the reason nothing downstream needs to know that
    /// submodules exist. `checkout`, `archive` and `diff` all read a tree
    /// through this function, so a pinned submodule is ordinary content to
    /// them and cannot be half-applied by a caller that forgot to recurse.
    /// Use [`View::submodules`] when the boundary is what you actually want.
    pub fn walk_tree(&self, tree: Hash) -> Result<BTreeMap<String, TreeEntry>> {
        let mut out = BTreeMap::new();
        self.walk_tree_into(tree, "", &mut out, 0)?;
        Ok(out)
    }

    /// The submodule pins declared directly by a tree, keyed by path.
    ///
    /// Deliberately does not descend through a pin: a submodule's own
    /// submodules belong to it, not to this repository.
    pub fn submodules(&self, tree: Hash) -> Result<BTreeMap<String, Hash>> {
        let mut out = BTreeMap::new();
        self.submodules_into(tree, "", &mut out, 0)?;
        Ok(out)
    }

    fn submodules_into(
        &self,
        tree: Hash,
        prefix: &str,
        out: &mut BTreeMap<String, Hash>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_TREE_DEPTH {
            bail!("directory nesting deeper than {MAX_TREE_DEPTH}");
        }
        for e in self.read_entries(tree)? {
            let path =
                if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
            match e.kind {
                EntryKind::Dir => self.submodules_into(e.hash, &path, out, depth + 1)?,
                EntryKind::Submodule => {
                    out.insert(path, e.hash);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The tree a submodule pin resolves to.
    ///
    /// A pin names a commit rather than a tree, so that what is recorded is a
    /// point in the submodule's history and not merely a bag of files: the
    /// message, author and parents come with it.
    pub fn submodule_tree(&self, pin: Hash, path: &str) -> Result<Hash> {
        match self.get(pin) {
            Ok(Object::Commit(c)) => Ok(c.tree),
            Ok(other) => bail!(
                "submodule {path} is pinned at {pin}, which is a {} and not a commit",
                other.kind().name()
            ),
            Err(_) => bail!(
                "submodule {path} is pinned at {pin}, which this store does not have\n\
                 run `fkit submodule fetch` to bring it in"
            ),
        }
    }

    fn walk_tree_into(
        &self,
        tree: Hash,
        prefix: &str,
        out: &mut BTreeMap<String, TreeEntry>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_TREE_DEPTH {
            bail!("directory nesting deeper than {MAX_TREE_DEPTH}");
        }
        // A directory is a Merkle tree over entry runs, so this walks levels
        // and runs before it ever sees a name. `read_entries_via` flattens that.
        let entries = self.read_entries(tree)?;
        for e in entries {
            let path = if prefix.is_empty() {
                e.name.clone()
            } else {
                format!("{prefix}/{}", e.name)
            };
            match e.kind {
                EntryKind::Dir => self.walk_tree_into(e.hash, &path, out, depth + 1)?,
                EntryKind::Submodule => {
                    let tree = self.submodule_tree(e.hash, &path)?;
                    self.walk_tree_into(tree, &path, out, depth + 1)?;
                }
                _ => {
                    out.insert(path, e);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct Snapshot {
    pub hash: Hash,
    pub size: u64,
    pub stats: crate::store::WriteStats,
    /// Trees and file nodes that were computed but never written to disk.
    pub objects: std::collections::HashMap<Hash, Object>,
}

/// Who a commit is recorded as, and when. `None` on either field means the
/// ordinary thing: the configured author, and now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitAs {
    pub author: Option<String>,
    /// Unix seconds.
    pub timestamp: Option<i64>,
}

#[derive(Debug)]
pub struct CommitResult {
    pub commit: Hash,
    pub tree: Hash,
    pub stats: crate::store::WriteStats,
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---- diffing ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added { path: String, size: u64 },
    Removed { path: String, size: u64 },
    Modified { path: String, old: Hash, new: Hash, old_size: u64, new_size: u64 },
    TypeChanged { path: String },
}

impl Change {
    pub fn path(&self) -> &str {
        match self {
            Change::Added { path, .. }
            | Change::Removed { path, .. }
            | Change::Modified { path, .. }
            | Change::TypeChanged { path } => path,
        }
    }
    pub fn sigil(&self) -> char {
        match self {
            Change::Added { .. } => '+',
            Change::Removed { .. } => '-',
            Change::Modified { .. } => '~',
            Change::TypeChanged { .. } => 't',
        }
    }
}

/// Compare two trees.
///
/// The Merkle property makes this fast in the common case: if two subtrees have
/// the same hash they are byte-for-byte identical and we can skip them entirely
/// without reading a single file. Comparing two 100 GB checkouts that differ in
/// one file touches a handful of objects.
pub fn diff_trees(view: &View, old: Option<Hash>, new: Option<Hash>) -> Result<Vec<Change>> {
    if old == new {
        return Ok(vec![]); // identical roots: provably no differences
    }
    let a = match old {
        Some(t) => view.walk_tree(t)?,
        None => BTreeMap::new(),
    };
    let b = match new {
        Some(t) => view.walk_tree(t)?,
        None => BTreeMap::new(),
    };

    let mut changes = Vec::new();
    for (path, oe) in &a {
        match b.get(path) {
            None => changes.push(Change::Removed { path: path.clone(), size: oe.size }),
            Some(ne) if ne.hash == oe.hash && ne.kind == oe.kind => {}
            Some(ne) if ne.kind != oe.kind => {
                changes.push(Change::TypeChanged { path: path.clone() })
            }
            Some(ne) => changes.push(Change::Modified {
                path: path.clone(),
                old: oe.hash,
                new: ne.hash,
                old_size: oe.size,
                new_size: ne.size,
            }),
        }
    }
    for (path, ne) in &b {
        if !a.contains_key(path) {
            changes.push(Change::Added { path: path.clone(), size: ne.size });
        }
    }
    changes.sort_by(|x, y| x.path().cmp(y.path()));
    Ok(changes)
}

fn read_hash_file(path: &Path) -> Result<Option<Hash>> {
    match fs::read_to_string(path) {
        Ok(s) => Hash::from_hex(s.trim())
            .map(Some)
            .context("ref file does not contain a valid hash"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        // An empty ref name resolves to the refs directory itself, and a
        // nested branch leaves a directory where a sibling ref looks for a
        // file. Neither is a ref; both used to surface as a raw
        // "Is a directory (os error 21)".
        Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Every ref in one flat directory. Not recursive: a ref name may contain `/`
/// on the wire, but on disk branches and tags live in separate directories and
/// a nested name would be a name with a slash in it, which is rejected.
fn read_ref_dir(dir: &Path) -> Result<BTreeMap<String, Hash>> {
    let mut out = BTreeMap::new();
    if !dir.exists() {
        return Ok(out);
    }
    for e in fs::read_dir(dir)? {
        let e = e?;
        if !e.file_type()?.is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(h) = read_hash_file(&e.path())? {
            out.insert(name, h);
        }
    }
    Ok(out)
}
