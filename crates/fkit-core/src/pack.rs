//! Packed segment storage.
//!
//! One file per object is git's loose-object format, and content-defined
//! chunking makes its weakness acute: this repository stores ~1800 objects for
//! 12 MB of content, each one an inode, a directory entry, and a 4 KiB
//! allocation unit. A 5 GB repository becomes hundreds of thousands of tiny
//! files.
//!
//! Segments fix that. Objects are appended to a small number of large files and
//! located through an index:
//!
//! ```text
//!   objects/pack/<writer>-0000.seg   framed object bytes, appended
//!   objects/pack/<writer>-0000.idx   hash -> (offset, length), appended
//! ```
//!
//! # Why no locking
//!
//! Each writer owns its own segment, named after its process. Two `fkit`
//! processes packing into the same store never write to the same file, so there
//! is nothing to lock and a crash can only ever truncate the writer's own tail.
//! Objects are immutable and content-named, so the same object landing in two
//! segments is harmless duplication rather than a conflict.
//!
//! # Compression
//!
//! Objects are zstd-compressed individually, not as a stream, so any one can
//! still be read with a single seek. Compression is attempted and then *kept
//! only if it helped*: chunks of already-compressed or random data would
//! otherwise grow by a few bytes each and cost CPU on every read. The index
//! records both lengths, so a reader knows which case it is looking at without
//! a trial decode.
//!
//! # Recovery
//!
//! The index is a plain append-only record of fixed-size entries. A torn write
//! at the end leaves a partial entry, which is detected by size and ignored —
//! the object it described is simply not found, and gets written again. There is
//! no state that a crash can leave *wrong*, only state it can leave *absent*.

use crate::hash::Hash;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// Rotate to a new segment past this size.
pub const SEGMENT_LIMIT: u64 = 512 * 1024 * 1024;

pub use crate::index::{Located, IDX_ENTRY, IDX_MAGIC};
use crate::index::{Index, Sealed};

/// `flags` bit 0: the stored bytes are zstd-compressed.
const FLAG_ZSTD: u8 = 1;

/// Below this, compression cannot pay for its own framing.
const MIN_COMPRESS: usize = 96;

/// Keep the compressed form only if it saves at least this fraction.
const COMPRESS_RATIO: f64 = 0.95;

/// Level 1, not 3. On a repository that is mostly build output and disk images
/// the compressor runs over every byte and keeps almost none of it, so its
/// throughput matters more than its ratio. Level 1 is several times faster and
/// still gave ~20x on source.
#[cfg(feature = "compression")]
const ZSTD_LEVEL: i32 = 1;


/// Cheap screen for data that cannot compress.
///
/// Counts distinct byte values in a small sample. Compressed archives, disk
/// images and encrypted blobs use nearly the whole alphabet uniformly; text and
/// code do not. Running zstd over a gigabyte of such data only to discard every
/// result is the single most wasteful thing this module could do.
fn likely_compressible(bytes: &[u8]) -> bool {
    const SAMPLE: usize = 512;
    let sample = &bytes[..bytes.len().min(SAMPLE)];
    if sample.len() < 64 {
        return true;
    }
    let mut seen = [false; 256];
    let mut distinct = 0usize;
    for b in sample {
        if !seen[*b as usize] {
            seen[*b as usize] = true;
            distinct += 1;
        }
    }
    // A 512-byte sample of random data touches ~200 distinct values; English
    // text or source touches ~60-90.
    distinct < 180
}

#[cfg(feature = "compression")]
fn squeeze(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < MIN_COMPRESS || !likely_compressible(bytes) {
        return None;
    }
    let out = zstd::encode_all(bytes, ZSTD_LEVEL).ok()?;
    // Random or already-compressed data (most chunks of a binary file) comes
    // back slightly larger; storing that would cost space *and* decode time.
    if (out.len() as f64) < bytes.len() as f64 * COMPRESS_RATIO {
        Some(out)
    } else {
        None
    }
}

#[cfg(not(feature = "compression"))]
fn squeeze(_bytes: &[u8]) -> Option<Vec<u8>> {
    None
}

#[cfg(feature = "compression")]
fn expand(bytes: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    zstd::decode_all(bytes)
        .context("decompressing a packed object")
        .inspect(|v| debug_assert_eq!(v.len(), raw_len))
}

#[cfg(not(feature = "compression"))]
fn expand(_bytes: &[u8], _raw_len: usize) -> Result<Vec<u8>> {
    anyhow::bail!(
        "this object is zstd-compressed but fkit-core was built without the \
         `compression` feature"
    )
}

pub struct Pack {
    dir: PathBuf,
    /// Where every packed object lives. Closed segments are searched on disk;
    /// only the one being appended to is held in memory.
    index: Index,
    /// Segment ids in this store, so a reader can open them by number.
    names: HashMap<u32, PathBuf>,
    /// This process's writable segment.
    current: Option<(u32, File, File, u64)>,
    writer_id: String,
    next_id: u32,
}

impl Pack {
    /// Open (or create) the pack directory and load every index.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Pack> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating pack dir {}", dir.display()))?;

        let mut index = Index::default();
        let mut hot: HashMap<Hash, Located> = HashMap::new();
        let mut names = HashMap::new();
        let mut max_id = 0u32;

        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("idx") {
                continue;
            }
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
            let Some(id) = stem.rsplit('-').next().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            max_id = max_id.max(id);
            names.insert(id, path.with_extension("seg"));

            // A sealed index is searched where it lies. Anything else is a
            // segment that was still being written, so it is read into memory
            // the way it always was — and sealed the next time one is closed.
            match Sealed::open(&path, id)? {
                Some(sealed) => index.push_sealed(sealed),
                None => crate::index::load_append_order(&path, id, &mut hot)?,
            }
        }
        for (h, loc) in hot {
            index.insert(h, loc);
        }

        Ok(Pack {
            dir,
            index,
            names,
            current: None,
            writer_id: format!("w{}", std::process::id()),
            next_id: max_id + 1,
        })
    }

    pub fn contains(&self, h: Hash) -> bool {
        self.index.contains(h)
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn ids(&self) -> Vec<Hash> {
        self.index.ids().unwrap_or_default()
    }

    pub fn get(&self, h: Hash) -> Result<Option<Vec<u8>>> {
        let Some(loc) = self.index.get(h) else {
            return Ok(None);
        };
        let path = self
            .names
            .get(&loc.segment)
            .with_context(|| format!("segment {} is missing", loc.segment))?;

        let mut f = File::open(path)?;
        f.seek(SeekFrom::Start(loc.offset))?;
        let mut buf = vec![0u8; loc.stored as usize];
        f.read_exact(&mut buf)?;

        if loc.compressed() {
            buf = expand(&buf, loc.raw as usize)?;
        }

        // The store's invariant holds inside a segment too: bytes must hash to
        // the name they were filed under. Verifying *after* decompression means
        // a corrupted compressed frame is caught here rather than producing
        // plausible-looking wrong bytes.
        let actual = Hash(*blake3::hash(&buf).as_bytes());
        if actual != h {
            anyhow::bail!(
                "packed object {} is corrupt: it hashes to {}",
                h.short(),
                actual.short()
            );
        }
        Ok(Some(buf))
    }

    /// Append `framed` under `id`. Returns false if it was already packed.
    pub fn put(&mut self, id: Hash, framed: &[u8]) -> Result<bool> {
        if self.index.contains(id) {
            return Ok(false);
        }
        let squeezed = squeeze(framed);
        let (payload, flags) = match &squeezed {
            Some(z) => (z.as_slice(), FLAG_ZSTD),
            None => (framed, 0u8),
        };

        self.ensure_segment(payload.len() as u64)?;
        let (seg_id, seg, idx, offset) = self.current.as_mut().expect("segment open");

        seg.write_all(payload)?;

        // The data lands before the index entry that points at it. The reverse
        // order would let a crash leave an index promising bytes that are not
        // there — a dangling pointer that survives restarts.
        seg.flush()?;

        let mut entry = Vec::with_capacity(IDX_ENTRY);
        entry.extend_from_slice(&id.0);
        entry.extend_from_slice(&offset.to_le_bytes());
        entry.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        entry.extend_from_slice(&(framed.len() as u32).to_le_bytes());
        entry.push(flags);
        idx.write_all(&entry)?;
        idx.flush()?;

        self.index.insert(
            id,
            Located {
                segment: *seg_id,
                offset: *offset,
                stored: payload.len() as u32,
                raw: framed.len() as u32,
                flags,
            },
        );
        *offset += payload.len() as u64;
        Ok(true)
    }

    /// Put one segment's index in hash order and search it on disk from now on.
    ///
    /// Not an optimisation that can be skipped on failure: leaving the records
    /// in memory is correct, just expensive, so a seal that cannot be written
    /// is reported and the hot copy kept.
    fn seal_segment(&mut self, id: u32) -> Result<()> {
        let Some(seg) = self.names.get(&id).cloned() else { return Ok(()) };
        let idx = crate::index::idx_path(&seg);
        if !idx.exists() {
            return Ok(());
        }
        crate::index::seal(&idx, id)?;
        if let Some(sealed) = Sealed::open(&idx, id)? {
            // Everything this segment held is now answered from disk.
            self.index.forget_segment(id);
            self.index.push_sealed(sealed);
        }
        Ok(())
    }

    /// Seal every segment, including the one being written.
    ///
    /// Called from packing, which is the point at which a store says it is
    /// finished for now. The open segment is closed first: sealing an index
    /// that is still being appended to would put it in hash order and then let
    /// the next write append out of order behind it. A later write simply
    /// opens a new segment, which is what happens after a rollover anyway.
    pub fn seal_all(&mut self) -> Result<()> {
        if let Some((id, seg, idx, _)) = self.current.take() {
            seg.sync_all()?;
            idx.sync_all()?;
            drop((seg, idx));
            self.seal_segment(id)?;
        }

        let already: std::collections::HashSet<u32> =
            self.index.sealed_segments().into_iter().collect();
        let ids: Vec<u32> =
            self.names.keys().copied().filter(|id| !already.contains(id)).collect();
        for id in ids {
            self.seal_segment(id)?;
        }
        Ok(())
    }

    /// Flush and fsync this writer's segment.
    pub fn sync(&mut self) -> Result<()> {
        if let Some((_, seg, idx, _)) = self.current.as_mut() {
            seg.sync_all()?;
            idx.sync_all()?;
        }
        Ok(())
    }

    fn ensure_segment(&mut self, incoming: u64) -> Result<()> {
        if let Some((_, _, _, offset)) = &self.current
            && *offset + incoming <= SEGMENT_LIMIT
        {
            return Ok(());
        }
        // The segment being replaced is finished, so its index can be put in
        // hash order and handed to the on-disk search. Until this happens the
        // records must stay in write order, because a crash mid-append has to
        // leave an index that describes only bytes that are really there.
        if let Some((old, _, _, _)) = self.current.take() {
            self.seal_segment(old)?;
        }

        let id = self.next_id;
        self.next_id += 1;

        let base = self.dir.join(format!("{}-{id:04}", self.writer_id));
        let seg_path = base.with_extension("seg");
        let idx_path = base.with_extension("idx");

        let seg = OpenOptions::new().create(true).append(true).open(&seg_path)?;
        let mut idx = OpenOptions::new().create(true).append(true).open(&idx_path)?;
        if idx.metadata()?.len() == 0 {
            idx.write_all(IDX_MAGIC)?;
        }
        let offset = seg.metadata()?.len();

        self.names.insert(id, seg_path);
        self.current = Some((id, seg, idx, offset));
        Ok(())
    }

    /// Bytes actually occupied on disk.
    pub fn bytes(&self) -> u64 {
        self.index.stored_bytes()
    }

    /// Bytes the same objects would occupy uncompressed.
    pub fn raw_bytes(&self) -> u64 {
        self.index.raw_bytes()
    }

    pub fn compressed_count(&self) -> usize {
        self.index.compressed_count()
    }
}

impl Pack {
    /// Segment ids present, with the age of each segment file.
    pub fn segments(&self) -> Vec<(u32, PathBuf)> {
        let mut v: Vec<(u32, PathBuf)> = self.names.iter().map(|(k, p)| (*k, p.clone())).collect();
        v.sort_by_key(|(k, _)| *k);
        v
    }

    /// Which objects live in a given segment.
    pub fn ids_in(&self, segment: u32) -> Vec<Hash> {
        self.index
            .entries()
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, l)| l.segment == segment)
            .map(|(h, _)| h)
            .collect()
    }

    /// Rewrite `segments` keeping only objects in `keep`, then remove the
    /// originals.
    ///
    /// Ordering is what makes this crash-safe: every surviving object is
    /// written and fsynced into a *new* segment before any old file is removed.
    /// A crash therefore leaves duplicates (harmless — objects are immutable
    /// and content-named) and never a gap.
    pub fn compact(&mut self, segments: &[u32], keep: &std::collections::HashSet<Hash>) -> Result<CompactStats> {
        let mut stats = CompactStats::default();

        // Read everything we intend to preserve *before* touching the originals.
        let mut carry: Vec<(Hash, Vec<u8>)> = Vec::new();
        for seg in segments {
            for id in self.ids_in(*seg) {
                let Some(loc) = self.index.get(id) else { continue };
                stats.examined += 1;
                if keep.contains(&id) {
                    let bytes = self
                        .get(id)?
                        .with_context(|| format!("object {} vanished mid-compaction", id.short()))?;
                    carry.push((id, bytes));
                } else {
                    stats.dropped += 1;
                    stats.reclaimed += loc.stored as u64;
                }
            }
        }

        // Forget the old locations so `put` does not treat these as present and
        // skip writing them into the new segment. Both halves have to go: the
        // hot records, and the sealed index still answering for the segment.
        // Leaving the sealed half in place made `put` skip every survivor and
        // then the old file was deleted underneath them.
        for seg in segments {
            for id in self.ids_in(*seg) {
                self.index.remove(&id);
            }
            self.index.forget_sealed(*seg);
        }

        // Force a fresh segment so survivors never land back in a file we are
        // about to delete.
        self.current = None;
        for (id, bytes) in carry {
            self.put(id, &bytes)?;
            stats.kept += 1;
        }
        self.sync()?;

        for seg in segments {
            if let Some(path) = self.names.remove(seg) {
                let _ = std::fs::remove_file(path.with_extension("idx"));
                let _ = std::fs::remove_file(&path);
            }
        }
        Ok(stats)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CompactStats {
    pub examined: usize,
    pub kept: usize,
    pub dropped: usize,
    pub reclaimed: u64,
}


#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "fkit-pack-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn framed(body: &[u8]) -> (Hash, Vec<u8>) {
        let mut v = vec![1u8];
        v.extend_from_slice(body);
        (Hash(*blake3::hash(&v).as_bytes()), v)
    }

    #[test]
    fn objects_round_trip_through_a_segment() {
        let dir = tmp("rt");
        let mut p = Pack::open(&dir).unwrap();
        let (h, bytes) = framed(b"hello pack");

        assert!(p.put(h, &bytes).unwrap());
        assert!(p.contains(h));
        assert_eq!(p.get(h).unwrap().unwrap(), bytes);

        // Second put of the same object is a no-op.
        assert!(!p.put(h, &bytes).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_index_survives_reopening() {
        let dir = tmp("reopen");
        let (h, bytes) = framed(b"persist me");
        {
            let mut p = Pack::open(&dir).unwrap();
            p.put(h, &bytes).unwrap();
            p.sync().unwrap();
        }
        let p = Pack::open(&dir).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p.get(h).unwrap().unwrap(), bytes);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn many_objects_land_in_few_files() {
        let dir = tmp("few");
        let mut p = Pack::open(&dir).unwrap();
        for i in 0..500u32 {
            let (h, b) = framed(&i.to_le_bytes());
            p.put(h, &b).unwrap();
        }
        p.sync().unwrap();

        // The entire point: 500 objects, not 500 files.
        let files = std::fs::read_dir(&dir).unwrap().count();
        assert!(files <= 2, "500 objects produced {files} files");
        assert_eq!(p.len(), 500);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_torn_index_entry_is_ignored_not_fatal() {
        let dir = tmp("torn");
        let (h, bytes) = framed(b"good object");
        {
            let mut p = Pack::open(&dir).unwrap();
            p.put(h, &bytes).unwrap();
            p.sync().unwrap();
        }
        // Simulate a crash mid-append: half an index entry.
        let idx = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().unwrap() == "idx")
            .unwrap();
        let mut data = std::fs::read(&idx).unwrap();
        data.extend_from_slice(&[0xAB; 17]);
        std::fs::write(&idx, data).unwrap();

        let p = Pack::open(&dir).unwrap();
        assert_eq!(p.len(), 1, "the complete entry must still load");
        assert_eq!(p.get(h).unwrap().unwrap(), bytes);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupted_segment_is_caught_on_read() {
        let dir = tmp("corrupt");
        let (h, bytes) = framed(b"trust me");
        {
            let mut p = Pack::open(&dir).unwrap();
            p.put(h, &bytes).unwrap();
            p.sync().unwrap();
        }
        let seg = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().unwrap() == "seg")
            .unwrap();
        let mut data = std::fs::read(&seg).unwrap();
        data[3] ^= 0xFF;
        std::fs::write(&seg, data).unwrap();

        let p = Pack::open(&dir).unwrap();
        let err = p.get(h).unwrap_err();
        assert!(err.to_string().contains("corrupt"), "got: {err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn compressible_objects_shrink_and_still_verify() {
        let dir = tmp("zstd");
        let mut p = Pack::open(&dir).unwrap();

        // Highly repetitive: exactly the shape of source code or a manifest.
        let body = "the quick brown fox jumps over the lazy dog\n".repeat(200);
        let (h, bytes) = framed(body.as_bytes());
        p.put(h, &bytes).unwrap();

        assert_eq!(p.get(h).unwrap().unwrap(), bytes, "must round-trip exactly");
        if cfg!(feature = "compression") {
            assert!(p.bytes() < p.raw_bytes() / 4, "repetitive data should compress hard");
            assert_eq!(p.compressed_count(), 1);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn incompressible_objects_are_stored_raw() {
        let dir = tmp("raw");
        let mut p = Pack::open(&dir).unwrap();

        // Pseudo-random bytes: zstd cannot help, and storing its slightly
        // larger output would cost space and decode time for nothing.
        let mut body = Vec::new();
        let mut x = 0x2545F4914F6CDD1Du64;
        while body.len() < 8192 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            body.extend_from_slice(&x.to_le_bytes());
        }
        let (h, bytes) = framed(&body);
        p.put(h, &bytes).unwrap();

        assert_eq!(p.get(h).unwrap().unwrap(), bytes);
        assert_eq!(p.compressed_count(), 0, "random data must not be stored compressed");
        assert_eq!(p.bytes(), p.raw_bytes());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tiny_objects_skip_compression_entirely() {
        let dir = tmp("tiny");
        let mut p = Pack::open(&dir).unwrap();
        let (h, bytes) = framed(b"hi");
        p.put(h, &bytes).unwrap();
        assert_eq!(p.compressed_count(), 0);
        assert_eq!(p.get(h).unwrap().unwrap(), bytes);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_index_without_the_magic_header_is_refused() {
        let dir = tmp("nomagic");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("w1-0001.idx"), vec![0u8; IDX_ENTRY * 2]).unwrap();
        let err = match Pack::open(&dir) {
            Ok(_) => panic!("a headerless index must be refused, not silently accepted"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("not an fkit pack index"), "got: {err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_object_is_none_not_an_error() {
        let dir = tmp("absent");
        let p = Pack::open(&dir).unwrap();
        assert!(p.get(Hash([9u8; 32])).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
