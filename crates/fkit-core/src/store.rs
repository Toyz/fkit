//! The content-addressed store: a flat, immutable pool of objects on disk.
//!
//! # Layout
//!
//! ```text
//!   .fkit/objects/pack/w1234-0000.seg   <- where objects actually go
//!   .fkit/objects/a3/f2e1....           <- legacy loose object, still read
//! ```
//!
//! **Writes always go to packed segments.** Content-defined chunking produces
//! roughly one object per 8 KiB of content, so a 9 GB repository is over a
//! million objects — as loose files that is a million inodes, each rounding up
//! to a 4 KiB block to hold a few hundred bytes. Loose storage is never the
//! right answer for this shape of store, so it is not a mode anyone has to
//! choose: the pack is created on the first write.
//!
//! Loose objects are still *read*, so a store written by an older version keeps
//! working and migrates as `fkit pack` folds the remainder in.
//!
//! # The one invariant
//!
//! **A file's entire contents hash to its own name.** We store the type tag as
//! the first byte of the file, and the id is `blake3(tag || body)` — so the id
//! is just `blake3(whole file)`. That means verification is trivially
//! self-contained: read the bytes, hash them, compare to the filename. Nothing
//! else needs to be trusted, which is exactly the property that lets us accept
//! objects from an untrusted network peer.
//!
//! # Why writes are safe to race
//!
//! Objects are immutable and named by content, so two processes writing the
//! same object write *identical bytes*. We still write to a temp file and
//! `rename` into place (rename is atomic on POSIX) so a reader never observes a
//! half-written object. If the object already exists, we skip the write
//! entirely — that check is the dedup.

use crate::hash::Hash;
use crate::object::{Kind, Object, TAG_CHUNK, TAG_COMMIT, TAG_ENTRIES, TAG_FILE, TAG_TREE};
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct Store {
    root: PathBuf,
    /// Packed segments, read on every miss of the loose layout.
    ///
    /// Reads consult the pack first (it holds the bulk of a packed repository)
    /// and fall back to loose files, so a store can hold both at once and a
    /// repository can be packed incrementally without a flag day.
    ///
    /// A `Mutex` rather than a `RefCell` because ingest runs on every core: the
    /// expensive work (reading, chunking, hashing, compressing) happens outside
    /// the lock, and only the segment append is serialised.
    pack: std::sync::Mutex<Option<crate::pack::Pack>>,

    /// Recently-read object bytes, so a graph walk does not go back to disk
    /// for the same commit and tree on every step.
    ///
    /// Safe to hold indefinitely because an object's name is a digest of its
    /// bytes: a cached entry cannot become wrong, only unwanted. Deletion is
    /// the one event that invalidates it, and collection clears it.
    cache: std::sync::Arc<dyn crate::cache::ObjectCache>,
}

/// Stats from a write, so callers can report what actually moved.
#[derive(Debug, Default, Clone, Copy)]
pub struct WriteStats {
    pub objects_written: u64,
    pub bytes_written: u64,
    pub objects_deduped: u64,
    pub bytes_deduped: u64,
}

impl WriteStats {
    pub fn merge(&mut self, other: WriteStats) {
        self.objects_written += other.objects_written;
        self.bytes_written += other.bytes_written;
        self.objects_deduped += other.objects_deduped;
        self.bytes_deduped += other.bytes_deduped;
    }
}

fn tag_to_kind(tag: u8) -> Result<Kind> {
    Ok(match tag {
        TAG_CHUNK => Kind::Chunk,
        TAG_FILE => Kind::File,
        TAG_TREE => Kind::Tree,
        TAG_COMMIT => Kind::Commit,
        TAG_ENTRIES => Kind::Entries,
        _ => bail!("unknown object tag {tag}"),
    })
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Store> {
        let root = root.into();
        fs::create_dir_all(&root)
            .with_context(|| format!("creating object store at {}", root.display()))?;

        // Only opened if a pack directory already exists; an unpacked store
        // pays nothing for this.
        let pack_dir = root.join("pack");
        let pack = if pack_dir.is_dir() {
            Some(crate::pack::Pack::open(&pack_dir)?)
        } else {
            None
        };

        Ok(Store {
            root,
            pack: std::sync::Mutex::new(pack),
            cache: std::sync::Arc::new(crate::cache::MemoryCache::default()),
        })
    }

    /// Begin writing new objects into packed segments.
    pub fn enable_pack(&self) -> Result<()> {
        let mut slot = self.pack.lock().unwrap();
        if slot.is_none() {
            *slot = Some(crate::pack::Pack::open(self.root.join("pack"))?);
        }
        Ok(())
    }

    pub fn is_packed(&self) -> bool {
        self.pack.lock().unwrap().is_some()
    }

    /// Move every loose object into segments and delete the loose copies.
    ///
    /// Safe to interrupt: a loose file is only removed once its packed copy has
    /// been written and flushed, so a crash leaves duplicates, never a gap.
    pub fn pack_loose(&self) -> Result<(usize, u64)> {
        self.enable_pack()?;
        let mut moved = 0usize;
        let mut bytes = 0u64;

        for id in self.loose_ids()? {
            let framed = fs::read(self.path_for(id))?;
            {
                let mut slot = self.pack.lock().unwrap();
                let pack = slot.as_mut().expect("enabled above");
                pack.put(id, &framed)?;
                pack.sync()?;
            }
            fs::remove_file(self.path_for(id))?;
            moved += 1;
            bytes += framed.len() as u64;
        }

        // Packing is the point at which a store settles into the segments it
        // will keep for a while, so it is the natural moment to put their
        // indexes in hash order and stop holding them in memory.
        {
            let mut slot = self.pack.lock().unwrap();
            if let Some(pack) = slot.as_mut() {
                pack.seal_all()?;
            }
        }
        Ok((moved, bytes))
    }

    /// Put every segment index in hash order, so lookups happen on disk.
    ///
    /// Idempotent, and safe to call on a store that is already sealed — which
    /// is why packing can simply always end with it rather than trying to work
    /// out whether anything changed.
    pub fn seal_indexes(&self) -> Result<()> {
        self.enable_pack()?;
        let mut slot = self.pack.lock().unwrap();
        if let Some(pack) = slot.as_mut() {
            pack.seal_all()?;
        }
        Ok(())
    }

    /// Merge small segments into fewer, larger ones.
    ///
    /// Every writing process opens its own segment — that is what removes the
    /// need for locking — so a repository accumulates one segment per commit
    /// unless something consolidates them. This is that something.
    ///
    /// Uses the same crash-safe path as garbage collection: survivors are
    /// written and fsynced before any original is removed.
    pub fn consolidate(&self, min_segment: u64) -> Result<usize> {
        let Some(targets) = self.with_pack(|pack| {
            pack.segments()
                .into_iter()
                .filter(|(_, path)| {
                    std::fs::metadata(path).map(|m| m.len() < min_segment).unwrap_or(false)
                })
                .map(|(id, _)| id)
                .collect::<Vec<u32>>()
        }) else {
            return Ok(0);
        };
        // One small segment is already consolidated.
        if targets.len() < 2 {
            return Ok(0);
        }

        // Keep everything: this is a reorganisation, not a collection.
        let keep: std::collections::HashSet<Hash> = self
            .with_pack(|p| targets.iter().flat_map(|s| p.ids_in(*s)).collect())
            .unwrap_or_default();

        self.compact_segments(&targets, &keep)?;
        Ok(targets.len())
    }

    /// How many segment files this store has.
    pub fn segment_count(&self) -> usize {
        self.with_pack(|p| p.segments().len()).unwrap_or(0)
    }

    pub fn packed_count(&self) -> usize {
        self.pack.lock().unwrap().as_ref().map(|p| p.len()).unwrap_or(0)
    }

    /// Bytes packed segments occupy, and what they would occupy uncompressed.
    pub fn packed_bytes(&self) -> (u64, u64) {
        self.pack
            .lock().unwrap()
            .as_ref()
            .map(|p| (p.bytes(), p.raw_bytes()))
            .unwrap_or((0, 0))
    }

    pub fn packed_compressed(&self) -> usize {
        self.pack.lock().unwrap().as_ref().map(|p| p.compressed_count()).unwrap_or(0)
    }

    /// Where a loose object lives, whether or not it exists.
    pub fn loose_path(&self, h: Hash) -> PathBuf {
        self.path_for(h)
    }

    /// Read-only access to the pack, if there is one. Used by `gc` to plan.
    pub fn with_pack<T>(&self, f: impl FnOnce(&crate::pack::Pack) -> T) -> Option<T> {
        self.pack.lock().unwrap().as_ref().map(f)
    }

    /// Rewrite the given segments keeping only `keep`.
    pub fn compact_segments(
        &self,
        segments: &[u32],
        keep: &std::collections::HashSet<Hash>,
    ) -> Result<crate::pack::CompactStats> {
        // Objects are about to stop existing. A cache still holding their
        // bytes would answer `get` for something `has` denies — the one way a
        // content-addressed cache can be wrong.
        self.cache.clear();
        let mut slot = self.pack.lock().unwrap();
        let pack = slot
            .as_mut()
            .context("this store has no packed segments to compact")?;
        let stats = pack.compact(segments, keep)?;
        // The segments compaction just wrote are final by construction.
        pack.seal_all()?;
        Ok(stats)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, h: Hash) -> PathBuf {
        let hex = h.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    pub fn has(&self, h: Hash) -> bool {
        if let Some(p) = self.pack.lock().unwrap().as_ref()
            && p.contains(h)
        {
            return true;
        }
        self.path_for(h).exists()
    }

    /// Store an object, returning its id. Idempotent: storing the same object
    /// twice is a no-op the second time.
    pub fn put(&self, obj: &Object) -> Result<(Hash, WriteStats)> {
        self.put_based(obj, None)
    }

    /// Store an object, offering `base` as something it may be a small edit
    /// away from.
    ///
    /// A hint and nothing more: the pack decides whether a patch against it is
    /// actually smaller, and stores the object whole when it is not. Callers
    /// never have to be right, only plausible.
    pub fn put_based(&self, obj: &Object, base: Option<Hash>) -> Result<(Hash, WriteStats)> {
        let body = obj.encode();
        let mut file = Vec::with_capacity(body.len() + 1);
        file.push(obj.kind().tag());
        file.extend_from_slice(&body);
        let id = Hash(*blake3::hash(&file).as_bytes());
        debug_assert_eq!(id, obj.id(), "store framing must match Object::id()");

        self.put_raw_based(id, &file, base)
    }

    /// Store pre-framed bytes under a claimed id — the path used when receiving
    /// objects from the network.
    ///
    /// We recompute the hash rather than trusting the sender. This is the entire
    /// security model: a malicious peer cannot make us store bytes under a name
    /// that does not hash to those bytes, so a verified root hash pins every
    /// byte beneath it.
    pub fn put_raw(&self, claimed: Hash, framed: &[u8]) -> Result<(Hash, WriteStats)> {
        self.put_raw_based(claimed, framed, None)
    }

    /// As `put_raw`, with a hint about what this object may be a patch against.
    pub fn put_raw_based(
        &self,
        claimed: Hash,
        framed: &[u8],
        base: Option<Hash>,
    ) -> Result<(Hash, WriteStats)> {
        let actual = Hash(*blake3::hash(framed).as_bytes());
        if actual != claimed {
            bail!(
                "hash mismatch: content hashes to {} but was offered as {}",
                actual.short(),
                claimed.short()
            );
        }

        // Writes always go to a segment, creating the pack on first use. There
        // is deliberately no way to ask for loose objects: at ~8 KiB per object
        // it is the wrong storage for this store, always.
        self.enable_pack()?;
        {
            let mut slot = self.pack.lock().unwrap();
            if let Some(pack) = slot.as_mut() {
                let wrote = pack.put_based(actual, framed, base)?;
                return Ok((
                    actual,
                    if wrote {
                        WriteStats {
                            objects_written: 1,
                            bytes_written: framed.len() as u64,
                            ..Default::default()
                        }
                    } else {
                        WriteStats {
                            objects_deduped: 1,
                            bytes_deduped: framed.len() as u64,
                            ..Default::default()
                        }
                    },
                ));
            }
        }

        // Unreachable in practice — `enable_pack` above always succeeds — but
        // kept as a correct fallback rather than an `unreachable!()` that would
        // turn an unexpected I/O failure into a panic.
        let path = self.path_for(actual);
        if path.exists() {
            return Ok((
                actual,
                WriteStats {
                    objects_deduped: 1,
                    bytes_deduped: framed.len() as u64,
                    ..Default::default()
                },
            ));
        }

        let dir = path.parent().expect("object path always has a parent");
        fs::create_dir_all(dir)?;

        // Temp name is derived from the hash, so concurrent writers of the same
        // object collide harmlessly (identical bytes) and writers of different
        // objects never collide at all.
        let tmp = dir.join(format!(".tmp-{}", actual.short()));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(framed)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;

        Ok((
            actual,
            WriteStats {
                objects_written: 1,
                bytes_written: framed.len() as u64,
                ..Default::default()
            },
        ))
    }

    /// Read the raw framed bytes (tag + body) of an object.
    pub fn get_raw(&self, h: Hash) -> Result<Vec<u8>> {
        Ok(self.get_shared(h)?.as_ref().clone())
    }

    /// The framed bytes, without copying them out of the cache.
    ///
    /// Every read goes through here. A hit avoids a `read` syscall for a loose
    /// object, and for a packed one also avoids decompressing it again — which
    /// is the larger saving, since almost everything is packed.
    pub fn get_shared(&self, h: Hash) -> Result<std::sync::Arc<Vec<u8>>> {
        if let Some(hit) = self.cache.get(h) {
            return Ok(hit);
        }

        let bytes = {
            let packed = {
                let guard = self.pack.lock().unwrap();
                match guard.as_ref() {
                    Some(pack) => pack.get(h)?,
                    None => None,
                }
            };
            match packed {
                Some(b) => b,
                None => {
                    let path = self.path_for(h);
                    fs::read(&path)
                        .with_context(|| format!("object {} not found in store", h.short()))?
                }
            }
        };

        let shared = std::sync::Arc::new(bytes);
        self.cache.put(h, std::sync::Arc::clone(&shared));
        Ok(shared)
    }

    pub fn get(&self, h: Hash) -> Result<Object> {
        let framed = self.get_shared(h)?;
        Self::decode_framed(&framed)
    }

    /// What the object cache has been doing.
    pub fn cache_stats(&self) -> crate::cache::CacheStats {
        self.cache.stats()
    }

    /// Drop one object from the cache, because it has left the store.
    pub fn forget_cached(&self, h: Hash) {
        self.cache.forget(h);
    }

    /// Replace the cache — to size it for a particular server, to share one
    /// across stores, or to turn it off entirely.
    ///
    /// Takes an `Arc` so several stores in one process can share a single
    /// budget rather than each keeping its own, which is what a server holding
    /// many repositories wants.
    pub fn set_cache(&mut self, cache: std::sync::Arc<dyn crate::cache::ObjectCache>) {
        self.cache = cache;
    }

    pub fn decode_framed(framed: &[u8]) -> Result<Object> {
        let (tag, body) = framed
            .split_first()
            .context("object file is empty (corrupt store)")?;
        Object::decode(tag_to_kind(*tag)?, body)
    }

    /// Read an object and check it actually hashes to the name we looked it up
    /// by.
    ///
    /// For *loose* objects this is the only integrity check, and `get` skips it
    /// for speed. Packed objects are verified on every read regardless: they
    /// pass through decompression, where a corrupt frame could otherwise
    /// produce plausible-looking wrong bytes, and one BLAKE3 pass at several
    /// GB/s is cheap next to that risk.
    pub fn get_verified(&self, h: Hash) -> Result<Object> {
        let framed = self.get_raw(h)?;
        let actual = Hash(*blake3::hash(&framed).as_bytes());
        if actual != h {
            bail!(
                "corrupt object: stored as {} but hashes to {}",
                h.short(),
                actual.short()
            );
        }
        Self::decode_framed(&framed)
    }

    /// Every object id in the store, in no particular order.
    pub fn iter_ids(&self) -> Result<Vec<Hash>> {
        let mut out = self
            .pack
            .lock().unwrap()
            .as_ref()
            .map(|p| p.ids())
            .unwrap_or_default();
        out.extend(self.loose_ids()?);
        Ok(out)
    }

    /// Ids present as loose files only.
    pub fn loose_ids(&self) -> Result<Vec<Hash>> {
        let mut out = Vec::new();
        let shards = match fs::read_dir(&self.root) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for shard in shards {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            let prefix = shard.file_name().to_string_lossy().to_string();
            if prefix == "pack" || prefix.len() != 2 {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                let rest = entry.file_name().to_string_lossy().to_string();
                if rest.starts_with(".tmp-") {
                    continue; // an in-flight write
                }
                if let Some(h) = Hash::from_hex(&format!("{prefix}{rest}")) {
                    out.push(h);
                }
            }
        }
        Ok(out)
    }

    /// Resolve a unique object by hash prefix, so users can type `a3f2e1`
    /// instead of 64 hex characters.
    pub fn resolve_prefix(&self, prefix: &str) -> Result<Hash> {
        if let Some(h) = Hash::from_hex(prefix) {
            return Ok(h);
        }
        if prefix.len() < 4 {
            bail!("hash prefix '{prefix}' is too short (need at least 4 characters)");
        }
        let matches: Vec<Hash> = self
            .iter_ids()?
            .into_iter()
            .filter(|h| h.to_hex().starts_with(prefix))
            .collect();
        match matches.len() {
            0 => bail!("no object matching '{prefix}'"),
            1 => Ok(matches[0]),
            n => bail!(
                "'{prefix}' is ambiguous: matches {n} objects ({}, ...)",
                matches[0].short()
            ),
        }
    }
}

/// Where ingested objects go.
///
/// `status` and `diff` need to *hash* the working tree to see what changed, but
/// they have no business writing anything: asking a question should not mutate
/// the store. A dry sink computes exactly the same hashes and reports exactly
/// what a real commit would write, without touching disk.
///
/// This works only because hashing is a pure function of content. There is no
/// "allocate an id" step that a dry run would have to fake.
pub struct Sink<'a> {
    store: &'a Store,
    dry: bool,
    /// Objects a dry run computed but did not write.
    ///
    /// Only non-chunk objects are kept. Trees and file nodes are small and
    /// bounded (a few KB each), while chunks are the bulk of the data — holding
    /// those would mean buffering the entire working tree in RAM. Everything
    /// that reads a dry snapshot (`status`, `diff`) walks trees only, so this is
    /// exactly enough.
    /// A `Mutex` for the same reason the pack is: ingest is parallel, and a dry
    /// run must still collect its trees from every thread.
    retained: std::sync::Mutex<std::collections::HashMap<Hash, Object>>,
}

impl<'a> Sink<'a> {
    /// A sink that actually persists objects.
    pub fn writing(store: &'a Store) -> Sink<'a> {
        Sink { store, dry: false, retained: Default::default() }
    }

    /// A sink that computes hashes and reports what *would* be written.
    pub fn dry(store: &'a Store) -> Sink<'a> {
        Sink { store, dry: true, retained: Default::default() }
    }

    /// Consume the sink and return whatever a dry run held on to.
    pub fn into_retained(self) -> std::collections::HashMap<Hash, Object> {
        self.retained.into_inner().unwrap_or_default()
    }

    pub fn store(&self) -> &Store {
        self.store
    }

    pub fn put(&self, obj: &Object) -> Result<(Hash, WriteStats)> {
        self.put_based(obj, None)
    }

    /// Store an object, offering `base` as a likely predecessor. See
    /// [`Store::put_based`].
    pub fn put_based(&self, obj: &Object, base: Option<Hash>) -> Result<(Hash, WriteStats)> {
        if !self.dry {
            return self.store.put_based(obj, base);
        }
        let id = obj.id();
        let size = obj.encode().len() as u64 + 1; // +1 for the framing tag
        if obj.kind() != Kind::Chunk {
            self.retained.lock().unwrap().insert(id, obj.clone());
        }
        let stats = if self.store.has(id) {
            WriteStats { objects_deduped: 1, bytes_deduped: size, ..Default::default() }
        } else {
            WriteStats { objects_written: 1, bytes_written: size, ..Default::default() }
        };
        Ok((id, stats))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Commit, EntryKind, TreeEntry};

    fn tmp_store() -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "fkit-store-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        (Store::open(&dir).unwrap(), dir)
    }

    #[test]
    fn roundtrips_every_object_kind() {
        let (s, dir) = tmp_store();

        let objects = vec![
            Object::Chunk(b"hello fkit".to_vec()),
            Object::File {
                level: 0,
                children: vec![(Hash([7u8; 32]), 10), (Hash([9u8; 32]), 20)],
            },
            Object::Entries(vec![TreeEntry {
                name: "README.md".into(),
                kind: EntryKind::File { exec: false },
                hash: Hash([3u8; 32]),
                size: 42,
            }]),
            Object::Tree {
                level: 0,
                children: vec![crate::object::TreeChild {
                    hash: Hash([5u8; 32]),
                    entries: 1,
                    size: 42,
                }],
            },
            Object::Commit(Commit {
                tree: Hash([1u8; 32]),
                parents: vec![Hash([2u8; 32])],
                author: "helba".into(),
                timestamp: 1_700_000_000,
                message: "first".into(),
            }),
        ];

        for obj in &objects {
            let (id, _) = s.put(obj).unwrap();
            let back = s.get_verified(id).unwrap();
            assert_eq!(&back, obj, "roundtrip failed for {:?}", obj.kind());
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn identical_content_dedupes() {
        let (s, dir) = tmp_store();
        let obj = Object::Chunk(b"same bytes".to_vec());

        let (id1, st1) = s.put(&obj).unwrap();
        let (id2, st2) = s.put(&obj).unwrap();

        assert_eq!(id1, id2, "same content must yield the same id");
        assert_eq!(st1.objects_written, 1);
        assert_eq!(st2.objects_written, 0, "second put should dedupe");
        assert_eq!(st2.objects_deduped, 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_bytes_that_do_not_match_the_claimed_hash() {
        let (s, dir) = tmp_store();
        let lie = Hash([0xAB; 32]);
        let err = s.put_raw(lie, b"\x01totally different bytes").unwrap_err();
        assert!(
            err.to_string().contains("hash mismatch"),
            "expected hash mismatch, got: {err}"
        );
        assert!(!s.has(lie), "must not store an object under a false name");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn detects_corruption_in_a_packed_object() {
        let (s, dir) = tmp_store();
        let (id, _) = s.put(&Object::Chunk(b"trust me".to_vec())).unwrap();
        assert!(s.is_packed(), "writes go to segments by default");

        // Bit rot inside the segment.
        let seg = fs::read_dir(dir.join("pack"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().unwrap() == "seg")
            .unwrap();
        let mut bytes = fs::read(&seg).unwrap();
        let n = bytes.len();
        bytes[n - 2] ^= 0xFF;
        fs::write(&seg, bytes).unwrap();

        // Packed reads verify unconditionally, so even the plain `get` refuses
        // to hand back bytes that do not match their name.
        assert!(s.get(id).is_err(), "a corrupt packed object must not be returned");
        assert!(s.get_verified(id).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn detects_corruption_in_a_loose_object() {
        let (s, dir) = tmp_store();
        // Write a loose object directly: the format is still read, so its
        // integrity check still has to work.
        let obj = Object::Chunk(b"trust me".to_vec());
        let id = obj.id();
        let hex = id.to_hex();
        fs::create_dir_all(dir.join(&hex[..2])).unwrap();
        fs::write(dir.join(&hex[..2]).join(&hex[2..]), b"\x01trust me NOT").unwrap();

        assert!(s.get(id).is_ok(), "unverified loose get is intentionally trusting");
        let err = s.get_verified(id).unwrap_err();
        assert!(err.to_string().contains("corrupt"), "got: {err}");
        let _ = fs::remove_dir_all(dir);
    }
}
