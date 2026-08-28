//! The object model: every node in fkit's Merkle DAG.
//!
//! There are exactly four kinds of object, and they stack into a tree of trees:
//!
//! ```text
//!   Commit ──tree──> Tree ──> Entries ──entry──> Tree ──> ... ──> File ──> Chunk
//!     │              (levels)  (a run of          (a subdir)     (levels)  (bytes)
//!     │                         directory entries)
//!     └──parent──> Commit ──> ...
//! ```
//!
//! Note the symmetry: `Tree`/`Entries` is to a directory exactly what
//! `File`/`Chunk` is to a file. Both are interior nodes over content-defined
//! runs of leaves. A directory is not a flat list — that is git's design, and it
//! means adding one file to a directory of 100 000 rewrites all of it.
//!
//! Every arrow is a *hash*, never a pointer. That single fact is what makes the
//! whole structure a Merkle DAG: a node's id covers the ids of its children,
//! which cover theirs, all the way down to the raw bytes. Change one byte in one
//! chunk and every hash on the path up to the commit changes — and *nothing*
//! else does.

use crate::hash::{Hash, HASH_LEN};
use anyhow::{bail, Result};

// Domain-separation tags. These are part of the hash, so they are permanent:
// changing one renames every object of that kind.
pub const TAG_CHUNK: u8 = 1;
pub const TAG_FILE: u8 = 2;
pub const TAG_TREE: u8 = 3;
pub const TAG_COMMIT: u8 = 4;
pub const TAG_ENTRIES: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Chunk,
    File,
    Tree,
    Commit,
    Entries,
}

impl Kind {
    pub fn tag(self) -> u8 {
        match self {
            Kind::Chunk => TAG_CHUNK,
            Kind::File => TAG_FILE,
            Kind::Tree => TAG_TREE,
            Kind::Commit => TAG_COMMIT,
            Kind::Entries => TAG_ENTRIES,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Kind::Chunk => "chunk",
            Kind::File => "file",
            Kind::Tree => "tree",
            Kind::Commit => "commit",
            Kind::Entries => "entries",
        }
    }
}

/// What a directory entry points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file -> points at a FileNode root.
    File { exec: bool },
    /// A subdirectory -> points at another Tree.
    Dir,
    /// A symlink -> points at a FileNode whose bytes are the link target.
    Symlink,
}

impl EntryKind {
    fn code(self) -> u8 {
        match self {
            EntryKind::File { exec: false } => 0,
            EntryKind::File { exec: true } => 1,
            EntryKind::Dir => 2,
            EntryKind::Symlink => 3,
        }
    }
    fn from_code(c: u8) -> Result<EntryKind> {
        Ok(match c {
            0 => EntryKind::File { exec: false },
            1 => EntryKind::File { exec: true },
            2 => EntryKind::Dir,
            3 => EntryKind::Symlink,
            _ => bail!("unknown entry kind code {c}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    pub hash: Hash,
    /// Total bytes of content underneath this entry. Cheap `du` for free, and it
    /// lets a client decide whether it wants to fetch a subtree before doing so.
    pub size: u64,
}

/// A child of a [`Object::Tree`] node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeChild {
    pub hash: Hash,
    /// Directory entries beneath this child. Lets a caller report "312 entries"
    /// without descending, and bounds a proof before fetching it.
    pub entries: u32,
    /// Content bytes beneath this child.
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub tree: Hash,
    /// Zero parents = root commit. One = normal. Two or more = a merge.
    pub parents: Vec<Hash>,
    pub author: String,
    /// Unix seconds. Deliberately *not* part of what makes content identical —
    /// but it IS part of the commit's own hash, so two commits made a second
    /// apart with identical trees are still distinct commits.
    pub timestamp: i64,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Object {
    /// A leaf: a run of raw file bytes, cut by the content-defined chunker.
    Chunk(Vec<u8>),

    /// An interior node of a *file's* Merkle tree.
    ///
    /// `level == 0` means children are Chunks. `level > 0` means children are
    /// FileNodes one level down. Building upward like this keeps any single
    /// object small no matter how huge the file is, and gives us real Merkle
    /// proofs: to prove "byte range X of this file is Y" you ship one sibling
    /// hash per level, not the whole file.
    File {
        level: u8,
        /// (child hash, number of content bytes that child covers)
        children: Vec<(Hash, u64)>,
    },

    /// A run of directory entries, sorted by name — the leaf of a directory's
    /// Merkle tree, and the exact counterpart of [`Object::Chunk`].
    ///
    /// Sorted order is not a nicety: without a canonical encoding the same
    /// directory contents would hash differently depending on how they were
    /// assembled, and content addressing would silently break.
    Entries(Vec<TreeEntry>),

    /// An interior node of a *directory's* Merkle tree.
    ///
    /// `level == 0` means children are [`Object::Entries`] runs; `level > 0`
    /// means children are `Tree` nodes one level down. Identical in shape to
    /// [`Object::File`], because it is solving the identical problem one
    /// abstraction up.
    Tree {
        level: u8,
        children: Vec<TreeChild>,
    },

    Commit(Commit),
}

impl Object {
    pub fn kind(&self) -> Kind {
        match self {
            Object::Chunk(_) => Kind::Chunk,
            Object::File { .. } => Kind::File,
            Object::Tree { .. } => Kind::Tree,
            Object::Entries(_) => Kind::Entries,
            Object::Commit(_) => Kind::Commit,
        }
    }

    /// The object's id: `blake3(tag || canonical_encoding)`.
    pub fn id(&self) -> Hash {
        Hash::of(self.kind().tag(), &self.encode())
    }

    /// Total content bytes reachable from this object.
    pub fn content_size(&self) -> u64 {
        match self {
            Object::Chunk(d) => d.len() as u64,
            Object::File { children, .. } => children.iter().map(|(_, n)| *n).sum(),
            Object::Tree { children, .. } => children.iter().map(|c| c.size).sum(),
            Object::Entries(entries) => entries.iter().map(|e| e.size).sum(),
            Object::Commit(_) => 0,
        }
    }

    /// Every object this one directly references. This is the edge set of the
    /// DAG — walking it is how we verify, garbage-collect, and sync.
    pub fn links(&self) -> Vec<Hash> {
        match self {
            Object::Chunk(_) => vec![],
            Object::File { children, .. } => children.iter().map(|(h, _)| *h).collect(),
            Object::Tree { children, .. } => children.iter().map(|c| c.hash).collect(),
            Object::Entries(entries) => entries.iter().map(|e| e.hash).collect(),
            Object::Commit(c) => {
                let mut v = vec![c.tree];
                v.extend(c.parents.iter().copied());
                v
            }
        }
    }

    // ---- canonical binary encoding -------------------------------------
    // Hand-rolled on purpose. The exact bytes here define every hash in the
    // system, so it must be explicit, stable, and readable. No derive macro
    // should ever get to silently change this.

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Object::Chunk(data) => w.raw(data),
            Object::File { level, children } => {
                w.u8(*level);
                w.u32(children.len() as u32);
                for (h, n) in children {
                    w.hash(*h);
                    w.u64(*n);
                }
            }
            Object::Tree { level, children } => {
                w.u8(*level);
                w.u32(children.len() as u32);
                for c in children {
                    w.hash(c.hash);
                    w.u32(c.entries);
                    w.u64(c.size);
                }
            }
            Object::Entries(entries) => {
                w.u32(entries.len() as u32);
                for e in entries {
                    w.str(&e.name);
                    w.u8(e.kind.code());
                    w.hash(e.hash);
                    w.u64(e.size);
                }
            }
            Object::Commit(c) => {
                w.hash(c.tree);
                w.u32(c.parents.len() as u32);
                for p in &c.parents {
                    w.hash(*p);
                }
                w.str(&c.author);
                w.u64(c.timestamp as u64);
                w.str(&c.message);
            }
        }
        w.0
    }

    pub fn decode(kind: Kind, buf: &[u8]) -> Result<Object> {
        let mut r = Reader::new(buf);
        let obj = match kind {
            Kind::Chunk => Object::Chunk(buf.to_vec()),
            Kind::File => {
                let level = r.u8()?;
                let n = r.u32()? as usize;
                let mut children = Vec::with_capacity(n);
                for _ in 0..n {
                    children.push((r.hash()?, r.u64()?));
                }
                Object::File { level, children }
            }
            Kind::Tree => {
                let level = r.u8()?;
                let n = r.u32()? as usize;
                let mut children = Vec::with_capacity(n);
                for _ in 0..n {
                    children.push(TreeChild { hash: r.hash()?, entries: r.u32()?, size: r.u64()? });
                }
                Object::Tree { level, children }
            }
            Kind::Entries => {
                let n = r.u32()? as usize;
                let mut entries = Vec::with_capacity(n);
                for _ in 0..n {
                    entries.push(TreeEntry {
                        name: r.str()?,
                        kind: EntryKind::from_code(r.u8()?)?,
                        hash: r.hash()?,
                        size: r.u64()?,
                    });
                }
                Object::Entries(entries)
            }
            Kind::Commit => {
                let tree = r.hash()?;
                let n = r.u32()? as usize;
                let mut parents = Vec::with_capacity(n);
                for _ in 0..n {
                    parents.push(r.hash()?);
                }
                Object::Commit(Commit {
                    tree,
                    parents,
                    author: r.str()?,
                    timestamp: r.u64()? as i64,
                    message: r.str()?,
                })
            }
        };
        Ok(obj)
    }
}

struct Writer(Vec<u8>);
impl Writer {
    fn new() -> Self {
        Writer(Vec::new())
    }
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn hash(&mut self, h: Hash) {
        self.0.extend_from_slice(&h.0);
    }
    fn raw(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.raw(s.as_bytes());
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            bail!("truncated object: wanted {n} bytes at offset {}", self.pos);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn hash(&mut self) -> Result<Hash> {
        Ok(Hash(self.take(HASH_LEN)?.try_into().unwrap()))
    }
    fn str(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        Ok(String::from_utf8(self.take(n)?.to_vec())?)
    }
}
