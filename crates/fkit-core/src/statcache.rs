//! Remembering what a file hashed to, so an unchanged one is not read again.
//!
//! # What this is, and is not
//!
//! fkit has no index and this does not give it one. There is still no staging
//! area and nothing here can be committed, staged or lost: `commit` snapshots
//! the working tree as it is, exactly as before. This is a cache of answers to
//! one question -- *what does this file hash to?* -- kept because on a large
//! tree asking it means opening, reading and hashing every file, every time,
//! and nearly every answer is the same as last time.
//!
//! It is a cache in the strict sense: delete `.fkit/stat` and the only thing
//! that changes is how long the next `status` takes.
//!
//! # When an answer is reused
//!
//! A file is taken to be unchanged when everything the filesystem says about
//! it is what it said when it was last read: size, modification time, change
//! time, inode and device. The change time is the one that matters most. A
//! modification time can be set to anything by anyone, and restoring a backup
//! or extracting an archive does exactly that; the change time is kept by the
//! kernel, moves on every write, rename and `chmod`, and cannot be set back.
//!
//! Two further conditions, both there because *looking* unchanged is not the
//! same as *being* unchanged:
//!
//! * **Nothing recent is remembered.** A file written twice inside one tick of
//!   the filesystem's clock, to the same length, has the same timestamps both
//!   times. So an answer is only kept for a file last modified comfortably
//!   before the snapshot that read it began ([`SETTLE`]), and is ignored if
//!   the file is as new as the cache itself. A file being worked on right now
//!   is simply read each time, which is what happened to every file before.
//! * **The content must still be in the store.** The cache names a file by
//!   its root hash; a commit built from that name is only sound if every
//!   chunk under it exists. A `status` hashes without storing, and garbage
//!   collection can remove a chunk, so each reuse checks that what it is
//!   about to name is actually there, and reads the file if it is not.
//!
//! # What can still fool it
//!
//! A filesystem with coarse timestamps (one or two seconds: FAT, HFS+, some
//! network mounts) whose clock also disagrees with this machine's by more
//! than [`SETTLE`], and a file rewritten to the same length within the same
//! tick as a snapshot read it. That is the same residue git's index has, for
//! the same reason, and removing the cache file is the same remedy. On a
//! platform with no change time or inode to consult, a same-length rewrite
//! with its modification time put back is also invisible.

use crate::hash::Hash;
use crate::object::Object;
use crate::store::Store;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long ago a file must have last changed for its hash to be remembered.
///
/// Longer than the coarsest timestamp any filesystem in use keeps -- FAT's two
/// seconds -- so that a write landing after the read cannot share a tick with
/// the one before it.
pub const SETTLE: Duration = Duration::from_secs(3);

/// A point in time as a filesystem records it.
pub type Stamp = (i64, u32);

fn stamp_of(t: SystemTime) -> Stamp {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        // Before 1970. Nothing depends on the value being meaningful, only on
        // it being the same value for the same file.
        Err(e) => (-(e.duration().as_secs() as i64), e.duration().subsec_nanos()),
    }
}

/// Everything the filesystem will say about a file without opening it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub size: u64,
    pub mtime: Stamp,
    pub ctime: Stamp,
    pub ino: u64,
    pub dev: u64,
    pub exec: bool,
}

impl Stat {
    #[cfg(unix)]
    pub fn of(m: &fs::Metadata) -> Stat {
        use std::os::unix::fs::MetadataExt;
        Stat {
            size: m.len(),
            mtime: (m.mtime(), m.mtime_nsec() as u32),
            ctime: (m.ctime(), m.ctime_nsec() as u32),
            ino: m.ino(),
            dev: m.dev(),
            exec: m.mode() & 0o111 != 0,
        }
    }

    #[cfg(not(unix))]
    pub fn of(m: &fs::Metadata) -> Stat {
        Stat {
            size: m.len(),
            mtime: m.modified().map(stamp_of).unwrap_or((0, 0)),
            ctime: (0, 0),
            ino: 0,
            dev: 0,
            exec: false,
        }
    }

    /// Whether a file with these times has been still for long enough, as of
    /// a snapshot that began at `began`, for its hash to be worth keeping.
    fn settled(&self, began: SystemTime) -> bool {
        let Some(limit) = began.checked_sub(SETTLE) else { return false };
        let limit = stamp_of(limit);
        self.mtime < limit && self.ctime < limit
    }
}

/// What a file was made of when it was last read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cached {
    pub stat: Stat,
    /// The file's root node.
    pub hash: Hash,
    /// How many chunks it was cut into.
    pub chunks: u64,
    /// The one chunk of a file that is a single chunk -- which is nearly every
    /// source file. Kept so that checking the content is still stored is two
    /// lookups in an index rather than reading the root node back to find out
    /// what it points at.
    pub only: Option<Hash>,
}

impl Cached {
    /// Is everything this names still in the store?
    ///
    /// Returns how many objects that is, so a caller reporting what a commit
    /// did not have to write can count them.
    fn present(&self, store: &Store) -> Option<u64> {
        if !store.has(self.hash) {
            return None;
        }
        match (self.chunks, self.only) {
            (0, _) => Some(1),
            (1, Some(chunk)) => store.has(chunk).then_some(2),
            // Larger files: walk the nodes. Only the nodes are read; a leaf
            // is a chunk of content, and being there is all that is asked.
            _ => {
                let mut count = 0u64;
                let mut stack = vec![self.hash];
                while let Some(h) = stack.pop() {
                    count += 1;
                    match store.get(h).ok()? {
                        Object::File { level: 0, children } => {
                            for (chunk, _) in children {
                                if !store.has(chunk) {
                                    return None;
                                }
                                count += 1;
                            }
                        }
                        Object::File { children, .. } => {
                            stack.extend(children.into_iter().map(|(h, _)| h))
                        }
                        _ => return None,
                    }
                }
                Some(count)
            }
        }
    }
}

const MAGIC: &[u8; 8] = b"fkstat1\n";

/// The cache as it was read from disk, and what this snapshot learns.
pub struct StatCache {
    path: PathBuf,
    entries: HashMap<String, Cached>,
    /// When the file was written, by the filesystem's own clock.
    written: Stamp,
}

impl StatCache {
    /// Read the cache at `path`.
    ///
    /// Never fails. A missing file, a truncated one, one from a version that
    /// wrote something else -- each is an empty cache, and an empty cache is
    /// only slow.
    pub fn load(path: impl Into<PathBuf>) -> StatCache {
        let path = path.into();
        let read = || -> Option<(HashMap<String, Cached>, Stamp)> {
            let written = stamp_of(fs::metadata(&path).ok()?.modified().ok()?);
            let bytes = fs::read(&path).ok()?;
            // The last 32 bytes are a digest of the rest: a write cut short
            // by a full disk or a killed process must not be half-believed.
            let (body, sum) = bytes.split_at_checked(bytes.len().checked_sub(32)?)?;
            if blake3::hash(body).as_bytes() != sum {
                return None;
            }
            let mut r = Reader(body);
            if r.take(8)? != MAGIC {
                return None;
            }
            let count = r.u64()?;
            let mut entries = HashMap::with_capacity(count.min(1 << 24) as usize);
            for _ in 0..count {
                let len = r.u32()? as usize;
                let rel = String::from_utf8(r.take(len)?.to_vec()).ok()?;
                let stat = Stat {
                    size: r.u64()?,
                    mtime: (r.u64()? as i64, r.u32()?),
                    ctime: (r.u64()? as i64, r.u32()?),
                    ino: r.u64()?,
                    dev: r.u64()?,
                    exec: r.take(1)?[0] != 0,
                };
                let hash = Hash(r.take(32)?.try_into().ok()?);
                let chunks = r.u64()?;
                let only = match r.take(1)?[0] {
                    0 => None,
                    _ => Some(Hash(r.take(32)?.try_into().ok()?)),
                };
                entries.insert(rel, Cached { stat, hash, chunks, only });
            }
            Some((entries, written))
        };
        let (entries, written) = read().unwrap_or_default();
        StatCache { path, entries, written }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// What `rel` hashed to, if it is still the file that was hashed and the
    /// content is still stored. The second value is the number of objects.
    pub fn hit(&self, rel: &str, now: &Stat, store: &Store) -> Option<(Cached, u64)> {
        let was = self.entries.get(rel)?;
        if was.stat != *now {
            return None;
        }
        // As new as the cache itself, by the filesystem's clock rather than
        // this machine's: it may have been written again in the same tick it
        // was read in, and nothing here could tell.
        if was.stat.mtime >= self.written || was.stat.ctime >= self.written {
            return None;
        }
        Some((*was, was.present(store)?))
    }

    /// Replace the cache with what a snapshot found, and write it down.
    ///
    /// `seen` is every file the snapshot read or reused. Paths that are not in
    /// it have left the tree and leave the cache with them; entries too fresh
    /// to trust ([`SETTLE`], against `began`) are dropped here rather than
    /// stored and doubted later.
    ///
    /// Best effort. A cache that cannot be written is a slower next run, not a
    /// failed commit, so nothing here is reported.
    pub fn replace(&mut self, seen: impl Iterator<Item = (String, Cached)>, began: SystemTime) {
        let next: HashMap<String, Cached> = seen.filter(|(_, c)| c.stat.settled(began)).collect();
        // Nothing learned and nothing forgotten: leave the file alone, so a
        // `status` on a quiet tree writes nothing at all.
        if next == self.entries {
            return;
        }
        self.entries = next;
        let _ = self.save();
    }

    fn save(&mut self) -> std::io::Result<()> {
        let mut out = Vec::with_capacity(64 + self.entries.len() * 160);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        for (rel, c) in &self.entries {
            out.extend_from_slice(&(rel.len() as u32).to_le_bytes());
            out.extend_from_slice(rel.as_bytes());
            out.extend_from_slice(&c.stat.size.to_le_bytes());
            out.extend_from_slice(&(c.stat.mtime.0 as u64).to_le_bytes());
            out.extend_from_slice(&c.stat.mtime.1.to_le_bytes());
            out.extend_from_slice(&(c.stat.ctime.0 as u64).to_le_bytes());
            out.extend_from_slice(&c.stat.ctime.1.to_le_bytes());
            out.extend_from_slice(&c.stat.ino.to_le_bytes());
            out.extend_from_slice(&c.stat.dev.to_le_bytes());
            out.push(c.stat.exec as u8);
            out.extend_from_slice(&c.hash.0);
            out.extend_from_slice(&c.chunks.to_le_bytes());
            match c.only {
                None => out.push(0),
                Some(h) => {
                    out.push(1);
                    out.extend_from_slice(&h.0);
                }
            }
        }
        let sum = blake3::hash(&out);
        out.extend_from_slice(sum.as_bytes());

        // Written beside itself and renamed into place, so a reader sees the
        // old cache or the new one and never part of each. Two commands
        // finishing together each write a whole file; the later one wins, and
        // either is correct.
        let tmp = self.path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(&tmp, &out)?;
        if let Err(e) = fs::rename(&tmp, &self.path) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        if let Ok(m) = fs::metadata(&self.path).and_then(|m| m.modified()) {
            self.written = stamp_of(m);
        }
        Ok(())
    }

    /// Where a repository keeps its cache, given its metadata directory.
    pub fn file(meta: &Path) -> PathBuf {
        meta.join("stat")
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}
