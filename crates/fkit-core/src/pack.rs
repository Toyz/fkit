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
use crate::hash::HASH_LEN;
use crate::index::{Index, Sealed};

/// `flags` bit 0: the stored bytes are zstd-compressed.
const FLAG_ZSTD: u8 = 1;

/// `flags` bit 1: the stored bytes are a patch against another object.
///
/// # What a delta is here
///
/// The payload is `base_hash(32) || zstd(content, dictionary = base bytes)`.
/// zstd in dictionary mode is literally LZ matching against the base, so the
/// stored bytes come out proportional to what changed rather than to the size
/// of the object -- which is the entire point.
///
/// # Why this does not break content addressing
///
/// The object is still named by the BLAKE3 of its *materialized* bytes. Only
/// the representation on disk varies. Every reader still verifies the hash
/// after expanding, dedup is unaffected, Merkle proofs are unaffected, and the
/// wire protocol never sees a delta because it speaks in whole objects.
///
/// # Why there is no chain to bound
///
/// A delta may only name a base that is itself stored literally. Depth is one
/// by construction, so reading is fetch-base then apply-patch and there is no
/// walk, no cap, and no bookkeeping to get wrong. `pick_base` enforces it.
use crate::index::FLAG_DELTA;

/// Keep a patch only if it is meaningfully smaller than storing the object
/// outright. Below this it is not worth the extra read of the base.
const DELTA_RATIO: f64 = 0.75;

/// Never delta an object this small: the 32-byte base name plus a zstd frame
/// costs more than the object does.
const MIN_DELTA: usize = 128;

#[cfg(feature = "compression")]
fn squeeze_delta(bytes: &[u8], base: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < MIN_DELTA {
        return None;
    }
    let mut c = zstd::bulk::Compressor::with_dictionary(ZSTD_LEVEL, base).ok()?;
    let patch = c.compress(bytes).ok()?;
    // The base has to be named in the payload, and the patch has to beat
    // storing the thing outright by enough to justify the second read.
    let cost = patch.len() + HASH_LEN;
    let plain = squeeze(bytes).map(|v| v.len()).unwrap_or(bytes.len());
    if (cost as f64) < plain as f64 * DELTA_RATIO {
        let mut out = Vec::with_capacity(cost);
        out.extend_from_slice(&[0u8; HASH_LEN]); // filled in by the caller
        out.extend_from_slice(&patch);
        Some(out)
    } else {
        None
    }
}

#[cfg(not(feature = "compression"))]
fn squeeze_delta(_bytes: &[u8], _base: &[u8]) -> Option<Vec<u8>> {
    None
}

#[cfg(feature = "compression")]
fn expand_delta(payload: &[u8], base: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    let patch = payload
        .get(HASH_LEN..)
        .context("a delta payload is shorter than the base name it must carry")?;
    let mut d = zstd::bulk::Decompressor::with_dictionary(base)?;
    d.decompress(patch, raw_len).context("applying a patch to its base")
}

#[cfg(not(feature = "compression"))]
fn expand_delta(_p: &[u8], _b: &[u8], _r: usize) -> Result<Vec<u8>> {
    anyhow::bail!(
        "this object is stored as a patch but fkit-core was built without the \
         `compression` feature"
    )
}

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

        if loc.is_delta() {
            // Depth is one, so this recursion cannot go further: `pick_base`
            // only ever names an object that is stored literally.
            let base = Hash(
                buf.get(..HASH_LEN)
                    .and_then(|b| <[u8; HASH_LEN]>::try_from(b).ok())
                    .context("a delta payload is missing the name of its base")?,
            );
            let base_bytes = self
                .get(base)?
                .with_context(|| format!("the base {} of a patch is gone", base.short()))?;
            buf = expand_delta(&buf, &base_bytes, loc.raw as usize)?;
        } else if loc.compressed() {
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

    /// The payload of an object exactly as it sits in its segment.
    fn stored_bytes(&self, loc: &Located) -> Option<Vec<u8>> {
        let path = self.names.get(&loc.segment)?;
        let mut f = File::open(path).ok()?;
        f.seek(SeekFrom::Start(loc.offset)).ok()?;
        let mut buf = vec![0u8; loc.stored as usize];
        f.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    /// Append an object's stored form verbatim, keeping how it was encoded.
    ///
    /// For relocation only. The bytes are already whatever they are -- literal,
    /// compressed, or a patch -- and this must not change that, because a patch
    /// re-encoded as a literal is a patch destroyed.
    fn put_stored(&mut self, id: Hash, payload: &[u8], flags: u8, raw: u32) -> Result<bool> {
        if self.index.contains(id) {
            return Ok(false);
        }
        self.ensure_segment(payload.len() as u64)?;
        let (seg_id, seg, idx, offset) = self.current.as_mut().expect("segment open");
        seg.write_all(payload)?;
        seg.flush()?;

        let mut entry = Vec::with_capacity(IDX_ENTRY);
        entry.extend_from_slice(&id.0);
        entry.extend_from_slice(&offset.to_le_bytes());
        entry.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        entry.extend_from_slice(&raw.to_le_bytes());
        entry.push(flags);
        idx.write_all(&entry)?;
        idx.flush()?;

        let loc = Located {
            segment: *seg_id,
            offset: *offset,
            stored: payload.len() as u32,
            raw,
            flags,
        };
        *offset += payload.len() as u64;
        self.index.insert(id, loc);
        Ok(true)
    }

    /// The patch `id` is stored as, if it is stored as one.
    ///
    /// The payload is returned exactly as it sits in the segment, which means
    /// it already names its own base -- so it can be handed to another store
    /// verbatim, and that store can find the base without being told.
    pub fn stored_patch(&self, id: Hash) -> Option<(Hash, u32, Vec<u8>)> {
        let loc = self.index.get(id)?;
        if !loc.is_delta() {
            return None;
        }
        let payload = self.stored_bytes(&loc)?;
        let base = Hash(payload.get(..HASH_LEN)?.try_into().ok()?);
        Some((base, loc.raw, payload))
    }

    /// Take a patch produced by another store, keeping it as a patch.
    ///
    /// The base it names has to be here already; the caller is responsible for
    /// that ordering. Returns the bytes it expands to, because the caller
    /// needs them anyway and expanding twice would be a waste.
    ///
    /// Nothing is taken on trust: the patch is applied and the result must
    /// hash to `id` before any of it is written. A patch that expands to
    /// something else is refused exactly as forged literal bytes would be.
    pub fn put_patch(&mut self, id: Hash, raw_len: u32, payload: &[u8]) -> Result<Vec<u8>> {
        let base = Hash(
            payload
                .get(..HASH_LEN)
                .and_then(|b| <[u8; HASH_LEN]>::try_from(b).ok())
                .context("a patch is missing the name of its base")?,
        );
        let base_bytes = self
            .get(base)?
            .with_context(|| format!("the base {} of a patch is not here", base.short()))?;
        let framed = expand_delta(payload, &base_bytes, raw_len as usize)?;

        let actual = Hash(*blake3::hash(&framed).as_bytes());
        if actual != id {
            anyhow::bail!(
                "a patch offered as {} expands to {}",
                id.short(),
                actual.short()
            );
        }
        self.put_stored(id, payload, FLAG_DELTA, raw_len)?;
        Ok(framed)
    }

    /// Append `framed` under `id`. Returns false if it was already packed.
    pub fn put(&mut self, id: Hash, framed: &[u8]) -> Result<bool> {
        self.put_based(id, framed, None)
    }

    /// The object a new one should be stored as a patch against.
    ///
    /// Given what occupied this position in the previous version, returns an
    /// anchor: that object if it is stored literally, or *its* base if it is
    /// itself a patch. One hop, never more, because a patch's base is always
    /// literal -- which is the invariant this function exists to maintain.
    ///
    /// Following that hop is the whole difference between re-anchoring every
    /// second version and holding one anchor for as long as it keeps paying.
    /// Refusing to patch against a patch would force literal, patch, literal,
    /// patch down the history and throw away half the saving; hopping instead
    /// lets a run of revisions all diff against the same literal, and the size
    /// check in `squeeze_delta` re-anchors on its own once drift has made the
    /// patch no longer worth it. Depth stays at one either way.
    fn anchor_for(&self, base: Hash) -> Option<(Hash, Vec<u8>)> {
        let loc = self.index.get(base)?;
        if !loc.is_delta() {
            return Some((base, self.get(base).ok().flatten()?));
        }
        let anchor = self.base_named_by(&loc)?;
        // A patch always names a literal, so this cannot recurse further.
        debug_assert!(!self.index.get(anchor)?.is_delta());
        Some((anchor, self.get(anchor).ok().flatten()?))
    }

    /// The base a stored patch names, read from the front of its payload.
    fn base_named_by(&self, loc: &Located) -> Option<Hash> {
        let path = self.names.get(&loc.segment)?;
        let mut f = File::open(path).ok()?;
        f.seek(SeekFrom::Start(loc.offset)).ok()?;
        let mut name = [0u8; HASH_LEN];
        f.read_exact(&mut name).ok()?;
        Some(Hash(name))
    }

    /// Append `framed` under `id`, storing it as a patch against `base` when
    /// that is meaningfully smaller.
    ///
    /// `base` is a hint, never a requirement: if it is absent, is itself a
    /// patch, or the patch does not pay for itself, the object is stored whole
    /// and nothing downstream can tell the difference.
    pub fn put_based(&mut self, id: Hash, framed: &[u8], base: Option<Hash>) -> Result<bool> {
        if self.index.contains(id) {
            return Ok(false);
        }

        let patch = base.filter(|b| *b != id).and_then(|b| {
            let (anchor, bytes) = self.anchor_for(b)?;
            if anchor == id {
                return None;
            }
            let mut p = squeeze_delta(framed, &bytes)?;
            p[..HASH_LEN].copy_from_slice(&anchor.0);
            Some(p)
        });

        let squeezed = if patch.is_none() { squeeze(framed) } else { None };
        let (payload, flags) = match (&patch, &squeezed) {
            // The patch is already a zstd frame; FLAG_DELTA implies it.
            (Some(d), _) => (d.as_slice(), FLAG_DELTA),
            (None, Some(z)) => (z.as_slice(), FLAG_ZSTD),
            (None, None) => (framed, 0u8),
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

        // Read everything we intend to preserve *before* touching the
        // originals -- and read it exactly as it is stored.
        //
        // Materializing here and writing the result back would silently undo
        // every patch in these segments: expanding a patch yields the whole
        // object, and storing that again with no base makes it literal. Since
        // this runs whenever a repository has collected a couple of dozen
        // segments, that quietly flattened almost everything back to whole
        // copies as fast as it was written.
        //
        // Compaction moves bytes. It does not re-encode them. That keeps the
        // patches, and is also less work than decompressing and recompressing
        // every object in the store.
        let mut carry: Vec<(Hash, Vec<u8>, u8, u32)> = Vec::new();
        for seg in segments {
            for id in self.ids_in(*seg) {
                let Some(loc) = self.index.get(id) else { continue };
                stats.examined += 1;
                if keep.contains(&id) {
                    let bytes = self.stored_bytes(&loc).with_context(|| {
                        format!("object {} vanished mid-compaction", id.short())
                    })?;
                    carry.push((id, bytes, loc.flags, loc.raw));
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
        for (id, bytes, flags, raw) in carry {
            self.put_stored(id, &bytes, flags, raw)?;
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

#[cfg(all(test, feature = "compression"))]
mod delta_tests {
    use super::*;
    use crate::hash::Hash;

    /// The same idiom the other tests here use: no dependency for a temp dir.
    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "fkit-delta-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn hash_of(b: &[u8]) -> Hash {
        Hash(*blake3::hash(b).as_bytes())
    }

    fn source(n: usize, marker: &str) -> Vec<u8> {
        let mut s = String::new();
        for i in 0..n {
            if i == n / 2 {
                s.push_str(&format!("    int x = compute({marker});\n"));
            } else {
                s.push_str(&format!("    int v{i} = compute({i}, flags);\n"));
            }
        }
        s.into_bytes()
    }

    #[test]
    fn a_patch_reads_back_as_its_original() {
        let dir = tmp("roundtrip");
        let mut pack = Pack::open(dir.clone()).unwrap();

        let v1 = source(200, "one");
        let v2 = source(200, "two");
        let (h1, h2) = (hash_of(&v1), hash_of(&v2));

        pack.put(h1, &v1).unwrap();
        pack.put_based(h2, &v2, Some(h1)).unwrap();

        // The whole point: what comes back is the object, not the patch.
        assert_eq!(pack.get(h1).unwrap().unwrap(), v1);
        assert_eq!(pack.get(h2).unwrap().unwrap(), v2);
    }

    #[test]
    fn a_patch_is_much_smaller_than_the_object() {
        let dir = tmp("smaller");
        let mut pack = Pack::open(dir.clone()).unwrap();

        let v1 = source(400, "one");
        let v2 = source(400, "two");
        let (h1, h2) = (hash_of(&v1), hash_of(&v2));

        pack.put(h1, &v1).unwrap();
        pack.put_based(h2, &v2, Some(h1)).unwrap();

        let whole = pack.index.get(h1).unwrap().stored;
        let patch = pack.index.get(h2).unwrap().stored;
        assert!(
            patch * 4 < whole,
            "a one-line edit stored {patch} bytes against a {whole}-byte object; \
             the patch should be a small fraction of it"
        );
    }

    #[test]
    fn a_patch_never_names_a_patch() {
        let dir = tmp("depth");
        let mut pack = Pack::open(dir.clone()).unwrap();

        // A run of versions, each offered the one before it as a base. Depth
        // must stay at one however long the run gets, which is what removes
        // the need for a chain limit.
        let mut prev: Option<Hash> = None;
        let mut hashes = Vec::new();
        for i in 0..12 {
            let v = source(300, &format!("rev{i}"));
            let h = hash_of(&v);
            pack.put_based(h, &v, prev).unwrap();
            hashes.push((h, v));
            prev = Some(h);
        }

        for (h, _) in &hashes {
            let loc = pack.index.get(*h).unwrap();
            if loc.is_delta() {
                let base_bytes = pack.get(*h).unwrap().unwrap();
                let _ = base_bytes;
                // Read the payload back to find the base it names.
                let raw = {
                    let path = pack.names.get(&loc.segment).unwrap();
                    let mut f = std::fs::File::open(path).unwrap();
                    use std::io::{Read, Seek, SeekFrom};
                    f.seek(SeekFrom::Start(loc.offset)).unwrap();
                    let mut b = vec![0u8; loc.stored as usize];
                    f.read_exact(&mut b).unwrap();
                    b
                };
                let base = Hash(<[u8; 32]>::try_from(&raw[..32]).unwrap());
                assert!(
                    !pack.index.get(base).unwrap().is_delta(),
                    "a patch named a base that is itself a patch — depth is no longer one"
                );
            }
        }

        // And every one of them still reads back correctly.
        for (h, v) in &hashes {
            assert_eq!(&pack.get(*h).unwrap().unwrap(), v);
        }
    }

    #[test]
    fn a_corrupt_patch_is_caught_rather_than_returned() {
        let dir = tmp("corrupt");
        let mut pack = Pack::open(dir.clone()).unwrap();

        let v1 = source(300, "one");
        let v2 = source(300, "two");
        let (h1, h2) = (hash_of(&v1), hash_of(&v2));
        pack.put(h1, &v1).unwrap();
        pack.put_based(h2, &v2, Some(h1)).unwrap();
        pack.seal_all().unwrap();

        let loc = pack.index.get(h2).unwrap();
        assert!(loc.is_delta(), "the second version should have been stored as a patch");

        // Flip a byte inside the patch itself.
        {
            use std::io::{Read, Seek, SeekFrom, Write};
            let path = pack.names.get(&loc.segment).unwrap().clone();
            let mut f = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
            let at = loc.offset + 32 + (loc.stored as u64 - 32) / 2;
            f.seek(SeekFrom::Start(at)).unwrap();
            let mut b = [0u8; 1];
            f.read_exact(&mut b).unwrap();
            f.seek(SeekFrom::Start(at)).unwrap();
            f.write_all(&[b[0] ^ 0xFF]).unwrap();
        }

        let again = Pack::open(dir.clone()).unwrap();
        // Verifying after materializing is what makes this detectable at all:
        // a damaged patch would otherwise expand into plausible wrong bytes.
        assert!(
            again.get(h2).is_err() || again.get(h2).unwrap().as_deref() != Some(v2.as_slice()),
            "a corrupted patch was accepted and returned as if it were the object"
        );
    }
}
