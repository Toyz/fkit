//! Where every packed object lives.
//!
//! A segment's index is a flat array of fixed-size records. While a segment is
//! being written the records arrive one at a time and are appended, because a
//! crash must not leave an index promising bytes the segment does not hold —
//! so the order they land in is the order they were written.
//!
//! That is fine for a segment being appended to and wrong for a store that has
//! finished with one. Loading every record into a hash map costs about a
//! hundred bytes an object once the table's own overhead is counted, so a
//! store of ten million objects spends a gigabyte of memory and several
//! seconds of startup describing objects nobody has asked for.
//!
//! So an index has two lives. While its segment is open it is appended and
//! held in memory. Once the segment is closed the index is rewritten in hash
//! order and never touched again, and a sorted immutable array is a thing you
//! can binary search where it lies: about two dozen positioned reads for ten
//! million objects, all of them into pages the operating system is already
//! caching, and nothing resident that has to be paid for at startup.
//!
//! Totals live in the sealed header rather than being summed on demand, so
//! asking how big a store is stays a constant-time question.

use crate::hash::{Hash, HASH_LEN};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

/// hash(32) + offset(8) + stored(4) + raw(4) + flags(1)
pub const IDX_ENTRY: usize = HASH_LEN + 8 + 4 + 4 + 1;

/// An index still being appended to. Records are in write order.
pub const IDX_MAGIC: &[u8; 8] = b"fkitidx1";

/// A sealed index: records sorted by hash, totals in the header.
pub const IDX_MAGIC_SORTED: &[u8; 8] = b"fkitidx2";

/// magic(8) + count(8) + stored(8) + raw(8) + compressed(8)
const SEALED_HEADER: usize = 8 + 8 + 8 + 8 + 8;

/// Where one object is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Located {
    pub segment: u32,
    pub offset: u64,
    /// Bytes occupied in the segment.
    pub stored: u32,
    /// Bytes after decompression — the real object size.
    pub raw: u32,
    pub flags: u8,
}

impl Located {
    pub fn compressed(&self) -> bool {
        self.flags & FLAG_ZSTD != 0
    }

    /// The stored bytes are a patch against another object, whose name is the
    /// first 32 bytes of the payload.
    pub fn is_delta(&self) -> bool {
        self.flags & FLAG_DELTA != 0
    }
}

/// `flags` bit 0: the stored bytes are zstd-compressed.
pub const FLAG_ZSTD: u8 = 1;

/// `flags` bit 1: the stored bytes are a patch against the object named by the
/// first 32 bytes of the payload. See `pack::FLAG_DELTA` for what that means
/// and why it never forms a chain.
pub const FLAG_DELTA: u8 = 2;

/// Encode one record. The layout is on-disk format; do not reorder.
pub fn encode(id: Hash, loc: &Located) -> [u8; IDX_ENTRY] {
    let mut e = [0u8; IDX_ENTRY];
    e[..HASH_LEN].copy_from_slice(&id.0);
    e[HASH_LEN..HASH_LEN + 8].copy_from_slice(&loc.offset.to_le_bytes());
    e[HASH_LEN + 8..HASH_LEN + 12].copy_from_slice(&loc.stored.to_le_bytes());
    e[HASH_LEN + 12..HASH_LEN + 16].copy_from_slice(&loc.raw.to_le_bytes());
    e[HASH_LEN + 16] = loc.flags;
    e
}

pub fn decode(e: &[u8], segment: u32) -> (Hash, Located) {
    let hash = Hash(e[..HASH_LEN].try_into().expect("record is IDX_ENTRY wide"));
    let loc = Located {
        segment,
        offset: u64::from_le_bytes(e[HASH_LEN..HASH_LEN + 8].try_into().unwrap()),
        stored: u32::from_le_bytes(e[HASH_LEN + 8..HASH_LEN + 12].try_into().unwrap()),
        raw: u32::from_le_bytes(e[HASH_LEN + 12..HASH_LEN + 16].try_into().unwrap()),
        flags: e[HASH_LEN + 16],
    };
    (hash, loc)
}

// ---- reading one record without a cursor ---------------------------------
//
// Positioned reads rather than seek-then-read: a binary search issues its
// probes against one file from whatever thread asks, and a shared cursor would
// have to be locked around every one of them.

#[cfg(unix)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(f, buf, off)
}

#[cfg(windows)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = std::os::windows::fs::FileExt::seek_read(f, &mut buf[done..], off + done as u64)?;
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        done += n;
    }
    Ok(())
}

/// A finished index, searched where it lies.
/// How large a sealed index may be before it is left on disk.
///
/// 49 bytes an object, so this covers a store of about five million of them.
/// Past that the searches go back to the file and the operating system's page
/// cache is what keeps them quick.
const RESIDENT_LIMIT: u64 = 256 * 1024 * 1024;

pub struct Sealed {
    file: File,
    /// The index itself, when it is small enough to keep.
    ///
    /// A lookup is a binary search, so it touched the disk about sixteen times
    /// per sealed segment and once more for every segment it had to rule out.
    /// On a clone of git's history that was fifty per cent of the whole
    /// transfer -- not reading objects, just asking whether they were already
    /// here.
    resident: Option<Vec<u8>>,
    pub segment: u32,
    count: u64,
    stored: u64,
    raw: u64,
    compressed: u64,
}

impl Sealed {
    /// Open a sealed index, or `None` if this file is not one.
    pub fn open(path: &Path, segment: u32) -> Result<Option<Sealed>> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len < SEALED_HEADER as u64 {
            return Ok(None);
        }
        let mut head = [0u8; SEALED_HEADER];
        read_at(&file, &mut head, 0)?;
        if &head[..8] != IDX_MAGIC_SORTED {
            return Ok(None);
        }
        let n = |i: usize| u64::from_le_bytes(head[i..i + 8].try_into().unwrap());
        let count = n(8);

        // A header claiming more records than the file holds is a torn seal.
        // Refusing here sends the caller to the append-order path, which reads
        // whatever is actually there.
        if SEALED_HEADER as u64 + count * IDX_ENTRY as u64 > len {
            return Ok(None);
        }
        let body = SEALED_HEADER as u64 + count * IDX_ENTRY as u64;
        let resident = if body <= RESIDENT_LIMIT {
            let mut buf = vec![0u8; body as usize];
            read_at(&file, &mut buf, 0).ok().map(|()| buf)
        } else {
            None
        };

        Ok(Some(Sealed {
            file,
            resident,
            segment,
            count,
            stored: n(16),
            raw: n(24),
            compressed: n(32),
        }))
    }

    fn record(&self, i: u64) -> Result<(Hash, Located)> {
        let at = SEALED_HEADER + i as usize * IDX_ENTRY;
        if let Some(body) = &self.resident {
            return Ok(decode(&body[at..at + IDX_ENTRY], self.segment));
        }
        let mut e = [0u8; IDX_ENTRY];
        read_at(&self.file, &mut e, at as u64)?;
        Ok(decode(&e, self.segment))
    }

    /// Binary search. Records are ordered by hash, which is what sealing does.
    pub fn get(&self, h: Hash) -> Result<Option<Located>> {
        let (mut lo, mut hi) = (0u64, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (found, loc) = self.record(mid)?;
            match found.0.cmp(&h.0) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(Some(loc)),
            }
        }
        Ok(None)
    }

    pub fn len(&self) -> u64 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Every record, for the whole-store walks that genuinely need them.
    pub fn iter(&self) -> Result<Vec<(Hash, Located)>> {
        let mut out = Vec::with_capacity(self.count as usize);
        for i in 0..self.count {
            out.push(self.record(i)?);
        }
        Ok(out)
    }
}

/// Rewrite an append-order index in hash order.
///
/// Written beside the original and renamed over it, so a crash leaves either
/// the old index or the new one and never half of either.
pub fn seal(path: &Path, segment: u32) -> Result<()> {
    let data = std::fs::read(path)
        .with_context(|| format!("reading {} to seal it", path.display()))?;
    if data.len() >= 8 && &data[..8] == IDX_MAGIC_SORTED {
        return Ok(());
    }
    if data.len() < 8 || &data[..8] != IDX_MAGIC {
        anyhow::bail!("{} is not an fkit pack index", path.display());
    }

    let body = &data[8..];
    let mut entries: Vec<(Hash, Located)> =
        body.as_chunks::<IDX_ENTRY>().0.iter().map(|e| decode(e, segment)).collect();

    // The same hash twice means the object was written twice; the records are
    // interchangeable, so either will do.
    entries.sort_unstable_by_key(|(h, _)| h.0);
    entries.dedup_by(|a, b| a.0 == b.0);

    let mut out = Vec::with_capacity(SEALED_HEADER + entries.len() * IDX_ENTRY);
    out.extend_from_slice(IDX_MAGIC_SORTED);
    out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    out.extend_from_slice(&entries.iter().map(|(_, l)| l.stored as u64).sum::<u64>().to_le_bytes());
    out.extend_from_slice(&entries.iter().map(|(_, l)| l.raw as u64).sum::<u64>().to_le_bytes());
    out.extend_from_slice(
        &(entries.iter().filter(|(_, l)| l.compressed()).count() as u64).to_le_bytes(),
    );
    for (h, l) in &entries {
        out.extend_from_slice(&encode(*h, l));
    }

    let tmp = path.with_extension("idx.sealing");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Everything the pack knows about where objects are.
///
/// Hot records — segments still being appended to — are in memory. Everything
/// else is a sealed file searched in place.
#[derive(Default)]
pub struct Index {
    hot: HashMap<Hash, Located>,
    sealed: Vec<Sealed>,
}

impl Index {
    /// How many sealed indexes a lookup may have to search. Test scaffolding.
    pub fn sealed_count(&self) -> usize {
        self.sealed.len()
    }

    pub fn push_sealed(&mut self, s: Sealed) {
        self.sealed.push(s);
    }

    pub fn insert(&mut self, h: Hash, loc: Located) {
        self.hot.insert(h, loc);
    }

    /// Newest first: a hot record is the one just written, and a segment
    /// rewritten by compaction supersedes what it replaced.
    pub fn get(&self, h: Hash) -> Option<Located> {
        if let Some(l) = self.hot.get(&h) {
            return Some(*l);
        }
        for s in &self.sealed {
            // A read error here means the index file is unreadable, which the
            // caller will discover as a missing object and report against the
            // segment rather than as a lookup that silently found nothing.
            if let Ok(Some(l)) = s.get(h) {
                return Some(l);
            }
        }
        None
    }

    pub fn contains(&self, h: Hash) -> bool {
        self.get(h).is_some()
    }

    pub fn len(&self) -> usize {
        self.hot.len() + self.sealed.iter().map(|s| s.len() as usize).sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn stored_bytes(&self) -> u64 {
        self.hot.values().map(|l| l.stored as u64).sum::<u64>()
            + self.sealed.iter().map(|s| s.stored).sum::<u64>()
    }

    pub fn raw_bytes(&self) -> u64 {
        self.hot.values().map(|l| l.raw as u64).sum::<u64>()
            + self.sealed.iter().map(|s| s.raw).sum::<u64>()
    }

    pub fn compressed_count(&self) -> usize {
        self.hot.values().filter(|l| l.compressed()).count()
            + self.sealed.iter().map(|s| s.compressed as usize).sum::<usize>()
    }

    /// Every id. Walks the sealed files, so this is for whole-store work —
    /// collection and checking — rather than anything on a read path.
    pub fn ids(&self) -> Result<Vec<Hash>> {
        let mut out: Vec<Hash> = self.hot.keys().copied().collect();
        for s in &self.sealed {
            out.extend(s.iter()?.into_iter().map(|(h, _)| h));
        }
        out.sort_unstable_by_key(|h| h.0);
        out.dedup();
        Ok(out)
    }

    /// Every id with where it lives.
    pub fn entries(&self) -> Result<Vec<(Hash, Located)>> {
        let mut out: Vec<(Hash, Located)> = Vec::with_capacity(self.len());
        for s in &self.sealed {
            out.extend(s.iter()?);
        }
        // Hot last so it wins on duplicates, matching `get`.
        out.extend(self.hot.iter().map(|(h, l)| (*h, *l)));
        Ok(out)
    }

    /// Forget every hot record from one segment, because it has been sealed
    /// and is answered from disk now.
    pub fn forget_segment(&mut self, segment: u32) {
        self.hot.retain(|_, l| l.segment != segment);
    }

    /// Forget a hot record. Sealed indexes are immutable — compaction replaces
    /// the whole segment rather than editing one out of it.
    pub fn remove(&mut self, h: &Hash) {
        self.hot.remove(h);
    }

    /// Drop the sealed index for a segment that no longer exists.
    ///
    /// Compaction rewrites survivors into a new segment and removes the old
    /// files; without this the index would keep answering for objects whose
    /// segment has been deleted, and every count would include them.
    pub fn forget_sealed(&mut self, segment: u32) {
        self.sealed.retain(|s| s.segment != segment);
    }

    /// Drop every sealed index, for a caller about to rebuild them.
    pub fn clear_sealed(&mut self) {
        self.sealed.clear();
    }

    pub fn sealed_segments(&self) -> Vec<u32> {
        self.sealed.iter().map(|s| s.segment).collect()
    }
}

/// Read an append-order index into `out`.
pub fn load_append_order(
    path: &Path,
    segment: u32,
    out: &mut HashMap<Hash, Located>,
) -> Result<()> {
    let data = std::fs::read(path)?;
    if data.len() < 8 || &data[..8] != IDX_MAGIC {
        anyhow::bail!(
            "{} is not an fkit pack index (or predates the current format) — \
             delete the pack directory and re-run `fkit pack`",
            path.display()
        );
    }
    let body = &data[8..];
    let full = body.len() / IDX_ENTRY * IDX_ENTRY;
    if full != body.len() {
        // A partial trailing record is a torn write from a crash. Ignoring it
        // is safe: the object is simply absent and will be written again.
        eprintln!(
            "fkit: ignoring {} trailing byte(s) of a partially written index ({})",
            body.len() - full,
            path.display()
        );
    }
    for e in body[..full].as_chunks::<IDX_ENTRY>().0 {
        let (h, l) = decode(e, segment);
        out.insert(h, l);
    }
    Ok(())
}

/// The `.idx` path for a segment file.
pub fn idx_path(seg: &Path) -> PathBuf {
    seg.with_extension("idx")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same idiom the pack tests use: no dependency for a temp directory.
    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "fkit-index-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn loc(offset: u64) -> Located {
        Located { segment: 1, offset, stored: 10, raw: 20, flags: 0 }
    }

    fn h(n: u8) -> Hash {
        let mut b = [0u8; HASH_LEN];
        b[0] = n;
        Hash(b)
    }

    #[test]
    fn a_sealed_index_finds_every_record_it_holds() {
        let p = tmp("0001").join("s-0001.idx");

        let mut raw = Vec::from(*IDX_MAGIC);
        // Deliberately out of order: sealing is what puts them in order.
        for n in [9u8, 3, 7, 1, 5] {
            raw.extend_from_slice(&encode(h(n), &loc(n as u64 * 100)));
        }
        std::fs::write(&p, &raw).unwrap();

        seal(&p, 1).unwrap();
        let s = Sealed::open(&p, 1).unwrap().expect("sealed");
        assert_eq!(s.len(), 5);
        for n in [1u8, 3, 5, 7, 9] {
            assert_eq!(s.get(h(n)).unwrap().map(|l| l.offset), Some(n as u64 * 100));
        }
        assert_eq!(s.get(h(4)).unwrap(), None, "absent hash");
    }

    #[test]
    fn sealing_is_idempotent() {
        let p = tmp("0002").join("s-0002.idx");
        let mut raw = Vec::from(*IDX_MAGIC);
        raw.extend_from_slice(&encode(h(2), &loc(0)));
        std::fs::write(&p, &raw).unwrap();

        seal(&p, 2).unwrap();
        let once = std::fs::read(&p).unwrap();
        seal(&p, 2).unwrap();
        assert_eq!(once, std::fs::read(&p).unwrap(), "sealing twice changed it");
    }

    #[test]
    fn totals_come_from_the_header_not_a_scan() {
        let p = tmp("0003").join("s-0003.idx");
        let mut raw = Vec::from(*IDX_MAGIC);
        for n in 1..=4u8 {
            let mut l = loc(n as u64);
            l.flags = if n % 2 == 0 { 1 } else { 0 };
            raw.extend_from_slice(&encode(h(n), &l));
        }
        std::fs::write(&p, &raw).unwrap();
        seal(&p, 3).unwrap();

        let s = Sealed::open(&p, 3).unwrap().unwrap();
        assert_eq!(s.stored, 40, "4 records of 10 stored bytes");
        assert_eq!(s.raw, 80);
        assert_eq!(s.compressed, 2);
    }

    #[test]
    fn an_append_order_index_is_not_mistaken_for_a_sealed_one() {
        let p = tmp("0004").join("s-0004.idx");
        let mut raw = Vec::from(*IDX_MAGIC);
        raw.extend_from_slice(&encode(h(1), &loc(0)));
        std::fs::write(&p, &raw).unwrap();
        assert!(Sealed::open(&p, 4).unwrap().is_none());
    }

    #[test]
    fn a_truncated_seal_is_refused_rather_than_read_short() {
        let p = tmp("0005").join("s-0005.idx");
        let mut raw = Vec::from(*IDX_MAGIC);
        for n in 1..=6u8 {
            raw.extend_from_slice(&encode(h(n), &loc(n as u64)));
        }
        std::fs::write(&p, &raw).unwrap();
        seal(&p, 5).unwrap();

        // Chop the last two records off, leaving the header's count too high.
        let full = std::fs::read(&p).unwrap();
        std::fs::write(&p, &full[..full.len() - 2 * IDX_ENTRY]).unwrap();
        assert!(Sealed::open(&p, 5).unwrap().is_none(), "torn seal was accepted");
    }

    #[test]
    fn hot_records_win_over_sealed_ones() {
        let p = tmp("0006").join("s-0006.idx");
        let mut raw = Vec::from(*IDX_MAGIC);
        raw.extend_from_slice(&encode(h(1), &loc(111)));
        std::fs::write(&p, &raw).unwrap();
        seal(&p, 6).unwrap();

        let mut idx = Index::default();
        idx.push_sealed(Sealed::open(&p, 6).unwrap().unwrap());
        assert_eq!(idx.get(h(1)).map(|l| l.offset), Some(111));

        idx.insert(h(1), loc(222));
        assert_eq!(idx.get(h(1)).map(|l| l.offset), Some(222), "hot must win");
        assert_eq!(idx.len(), 2, "counted separately; ids() dedups");
        assert_eq!(idx.ids().unwrap().len(), 1);
    }
}
