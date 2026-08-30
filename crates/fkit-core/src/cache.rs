//! A bounded cache of object bytes, in front of the disk.
//!
//! # Why this needs no invalidation on write
//!
//! An object's name is a digest of its bytes. `a1b2c3…` names one byte
//! sequence and will never name another, in this repository or any other. So
//! a cached entry cannot go stale the way a row cached by primary key can:
//! there is no update, only existence and absence.
//!
//! That removes the hard half of caching. What remains is bounding memory,
//! which is what the size limit and the age limit are for — neither is a
//! correctness mechanism.
//!
//! # What does need invalidating
//!
//! Deletion. Garbage collection removes objects and rewrites segments, and a
//! cache still holding those bytes would serve something the store no longer
//! has — `has()` would say no while `get()` said yes. So collection clears the
//! cache, which is cheap because it happens rarely and is not on any read
//! path.
//!
//! # Why bytes rather than entries
//!
//! Objects here run from a forty-byte commit to a four-megabyte chunk. A cache
//! of "one thousand entries" holds either forty kilobytes or four gigabytes
//! depending on what a repository happens to contain, which is not a bound.

use crate::hash::Hash;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How much object data to keep in memory, by default.
///
/// Small enough to be unremarkable on a server running several repositories,
/// large enough to hold the commits, trees and entry lists a page walks
/// repeatedly — those are the objects read over and over, and they are tens of
/// bytes each.
pub const DEFAULT_CAPACITY: usize = 64 * 1024 * 1024;

/// How long an untouched entry may squat.
///
/// Not a correctness bound — content-addressed bytes never expire — but a way
/// to hand memory back after a burst of traffic to one repository, rather than
/// holding its working set until something else evicts it.
pub const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);

/// The largest share of the cache one object may occupy, as a divisor.
///
/// No single object may take more than a quarter, so admitting one can never
/// evict most of what is worth keeping. Large chunks are streamed to a client
/// and rarely wanted twice; the small structural objects — commits, trees,
/// entry lists — are what a graph walk hits again and again, and they are tens
/// of bytes each.
///
/// A fraction of the *configured* capacity rather than a fixed size, so a
/// small cache is bounded by the same rule a large one is.
const MAX_ENTRY_DIVISOR: usize = 4;

struct Entry {
    bytes: Arc<Vec<u8>>,
    /// Bumped on every hit; the least recently used is evicted first.
    used: u64,
    stored: Instant,
    /// Set per entry for a blob; the object cache passes its global figure.
    ttl: Duration,
}

struct Inner<K> {
    map: HashMap<K, Entry>,
    /// Keys in least-recently-used order, so a victim is the first one.
    ///
    /// Finding the victim by scanning for the smallest `used` is fine while a
    /// cache evicts in bursts, and quadratic when it evicts continuously --
    /// which is what a full cache under a large transfer does, once per stored
    /// object. On a hub serving a second clone of a big history that scan was
    /// most of the process's CPU, and it got worse the longer the process ran.
    order: BTreeSet<(u64, K)>,
    bytes: usize,
    capacity: usize,
    ttl: Duration,
    clock: u64,
    hits: u64,
    misses: u64,
}

impl<K: Eq + std::hash::Hash + Ord + Clone> Inner<K> {
    /// Record a use, keeping the order in step with it.
    fn touch(&mut self, key: &K) {
        self.clock += 1;
        let now = self.clock;
        if let Some(e) = self.map.get_mut(key) {
            let was = e.used;
            e.used = now;
            self.order.remove(&(was, key.clone()));
            self.order.insert((now, key.clone()));
        }
    }

    fn admit(&mut self, key: K, entry: Entry) {
        self.bytes += entry.bytes.len();
        self.order.insert((entry.used, key.clone()));
        self.map.insert(key, entry);
    }

    /// Remove one entry, wherever it is being removed from.
    fn evict(&mut self, key: &K) -> Option<Entry> {
        let e = self.map.remove(key)?;
        self.order.remove(&(e.used, key.clone()));
        self.bytes -= e.bytes.len();
        Some(e)
    }

    /// Drop the oldest until what is held fits in what was allowed.
    fn shrink(&mut self) {
        while self.bytes > self.capacity {
            let Some((_, key)) = self.order.pop_first() else { break };
            if let Some(e) = self.map.remove(&key) {
                self.bytes -= e.bytes.len();
            }
        }
    }
}

/// Somewhere to keep object bytes that are worth not re-reading.
///
/// A trait so the store does not care where they are kept. In one process,
/// memory. Across several, something shared — Valkey or Redis — where the win
/// is that a restart or a second hub does not start cold.
///
/// Deliberately synchronous, because [`Store::get`] is, and every read goes
/// through it. A shared implementation therefore needs a blocking client;
/// making this async would turn every object read in the program into an
/// async fn to serve a backend that may not be configured.
///
/// Implementations must tolerate lying about absence — returning `None` for
/// something they hold is always safe, since the store falls back to disk.
/// They must never return bytes for a hash that does not name them, which
/// content addressing makes hard to get wrong: the key *is* the checksum.
pub trait ObjectCache: Send + Sync {
    fn get(&self, h: Hash) -> Option<Arc<Vec<u8>>>;
    fn put(&self, h: Hash, bytes: Arc<Vec<u8>>);
    /// Drop one object, because it has left the store.
    fn forget(&self, h: Hash);
    /// Drop everything, because collection has rewritten the store.
    fn clear(&self);
    fn stats(&self) -> CacheStats;
}

/// Somewhere to keep bytes that are expensive to produce and named by
/// something other than their content.
///
/// [`ObjectCache`] is keyed by hash, which makes it safe in a way this is not:
/// there, the key *is* the checksum, so a wrong answer is impossible. Here the
/// key is a name someone chose — a page path, a commit whose ancestors were
/// counted — so an implementation may only hold values that cannot go stale
/// within their lifetime, or must set a lifetime short enough that staleness
/// does not matter.
///
/// Synchronous for the same reason as [`ObjectCache`]: the callers are, and
/// making this async would spread `.await` through them to serve a backend
/// that may not be configured.
pub trait BlobCache: Send + Sync {
    fn get(&self, key: &str) -> Option<Arc<Vec<u8>>>;
    /// Keep `bytes` under `key` for at most `ttl`.
    ///
    /// The lifetime is the caller's to state, because only the caller knows
    /// how long its answer stays true. A rendered card carries a description
    /// and a tip hash and is wrong within minutes of either changing; a count
    /// of a commit's ancestors is a fact about an immutable hash and can be
    /// kept for as long as there is room.
    fn put(&self, key: &str, bytes: Arc<Vec<u8>>, ttl: Duration);
    /// Drop one entry, because whatever it described has changed.
    fn forget(&self, key: &str);
    fn stats(&self) -> CacheStats;
}

/// A bounded, age-limited blob cache in this process.
pub struct MemoryBlobs {
    inner: Mutex<Inner<String>>,
}

impl MemoryBlobs {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        MemoryBlobs {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                order: BTreeSet::new(),
                bytes: 0,
                capacity,
                ttl,
                clock: 0,
                hits: 0,
                misses: 0,
            }),
        }
    }
}

impl BlobCache for MemoryBlobs {
    fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        let mut in_ = self.inner.lock().unwrap();
        let expired = match in_.map.get(key) {
            Some(e) => e.stored.elapsed() > e.ttl,
            None => {
                in_.misses += 1;
                return None;
            }
        };
        if expired {
            in_.evict(&key.to_string());
            in_.misses += 1;
            return None;
        }
        in_.touch(&key.to_string());
        in_.hits += 1;
        let e = in_.map.get(key).expect("checked just above");
        Some(Arc::clone(&e.bytes))
    }

    fn put(&self, key: &str, bytes: Arc<Vec<u8>>, ttl: Duration) {
        let mut in_ = self.inner.lock().unwrap();
        if bytes.len() > in_.capacity / MAX_ENTRY_DIVISOR {
            return;
        }
        in_.evict(&key.to_string());
        in_.clock += 1;
        let used = in_.clock;
        in_.admit(key.to_string(), Entry { bytes, used, stored: Instant::now(), ttl });
        in_.shrink();
    }

    fn forget(&self, key: &str) {
        let mut in_ = self.inner.lock().unwrap();
        in_.evict(&key.to_string());
    }

    fn stats(&self) -> CacheStats {
        let in_ = self.inner.lock().unwrap();
        CacheStats {
            entries: in_.map.len(),
            bytes: in_.bytes,
            capacity: in_.capacity,
            hits: in_.hits,
            misses: in_.misses,
        }
    }
}

/// A blob cache that keeps nothing.
pub struct NoBlobs;

impl BlobCache for NoBlobs {
    fn get(&self, _key: &str) -> Option<Arc<Vec<u8>>> {
        None
    }
    fn put(&self, _key: &str, _bytes: Arc<Vec<u8>>, _ttl: Duration) {}
    fn forget(&self, _key: &str) {}
    fn stats(&self) -> CacheStats {
        CacheStats { entries: 0, bytes: 0, capacity: 0, hits: 0, misses: 0 }
    }
}

/// Near and far, for blobs. Same bargain as [`Tiered`].
pub struct TieredBlobs {
    near: Arc<dyn BlobCache>,
    far: Arc<dyn BlobCache>,
}

impl TieredBlobs {
    pub fn new(near: Arc<dyn BlobCache>, far: Arc<dyn BlobCache>) -> Self {
        TieredBlobs { near, far }
    }
}

impl BlobCache for TieredBlobs {
    fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        if let Some(hit) = self.near.get(key) {
            return Some(hit);
        }
        let hit = self.far.get(key)?;
        // Promoted with the near cache's own lifetime: the far one is already
        // counting down and this copy should not outlive it by much.
        self.near.put(key, Arc::clone(&hit), Duration::from_secs(60));
        Some(hit)
    }

    fn put(&self, key: &str, bytes: Arc<Vec<u8>>, ttl: Duration) {
        self.near.put(key, Arc::clone(&bytes), ttl);
        self.far.put(key, bytes, ttl);
    }

    fn forget(&self, key: &str) {
        self.near.forget(key);
        self.far.forget(key);
    }

    /// The near cache's, which is the one with a capacity worth reporting.
    fn stats(&self) -> CacheStats {
        self.near.stats()
    }
}

/// A cache that keeps nothing, for a store that would rather not.
pub struct NoCache;

impl ObjectCache for NoCache {
    fn get(&self, _h: Hash) -> Option<Arc<Vec<u8>>> {
        None
    }
    fn put(&self, _h: Hash, _bytes: Arc<Vec<u8>>) {}
    fn forget(&self, _h: Hash) {}
    fn clear(&self) {}
    fn stats(&self) -> CacheStats {
        CacheStats { entries: 0, bytes: 0, capacity: 0, hits: 0, misses: 0 }
    }
}

/// A bounded, age-limited cache of framed object bytes, in this process.
pub struct MemoryCache {
    inner: Mutex<Inner<Hash>>,
}

impl Default for MemoryCache {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY, DEFAULT_TTL)
    }
}

/// What the cache has been doing, for anyone who wants to know whether it is
/// earning its memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub entries: usize,
    pub bytes: usize,
    pub capacity: usize,
    pub hits: u64,
    pub misses: u64,
}

impl MemoryCache {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        MemoryCache {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                order: BTreeSet::new(),
                bytes: 0,
                capacity,
                ttl,
                clock: 0,
                hits: 0,
                misses: 0,
            }),
        }
    }

}

impl ObjectCache for MemoryCache {
    fn get(&self, h: Hash) -> Option<Arc<Vec<u8>>> {
        let mut in_ = self.inner.lock().unwrap();
        let ttl = in_.ttl;

        // Age is checked before taking a mutable borrow, so the expired case
        // can remove the entry without holding one.
        let expired = match in_.map.get(&h) {
            None => {
                in_.misses += 1;
                return None;
            }
            Some(e) => !ttl.is_zero() && e.stored.elapsed() >= ttl,
        };

        // Expired entries are dropped where they are found rather than by a
        // sweep: the reader is already holding the lock and knows the key.
        if expired {
            in_.evict(&h);
            in_.misses += 1;
            return None;
        }

        in_.touch(&h);
        let bytes = Arc::clone(&in_.map.get(&h).expect("checked just above").bytes);
        in_.hits += 1;
        Some(bytes)
    }

    fn put(&self, h: Hash, bytes: Arc<Vec<u8>>) {
        let mut in_ = self.inner.lock().unwrap();
        if in_.capacity == 0 || bytes.len() > in_.capacity / MAX_ENTRY_DIVISOR {
            return;
        }
        if in_.map.contains_key(&h) {
            return;
        }

        in_.clock += 1;
        let used = in_.clock;
        let ttl = in_.ttl;
        in_.admit(h, Entry { bytes, used, stored: Instant::now(), ttl });
        in_.shrink();
    }

    fn forget(&self, h: Hash) {
        let mut in_ = self.inner.lock().unwrap();
        in_.evict(&h);
    }

    fn clear(&self) {
        let mut in_ = self.inner.lock().unwrap();
        in_.map.clear();
        in_.order.clear();
        in_.bytes = 0;
    }

    fn stats(&self) -> CacheStats {
        let in_ = self.inner.lock().unwrap();
        CacheStats {
            entries: in_.map.len(),
            bytes: in_.bytes,
            capacity: in_.capacity,
            hits: in_.hits,
            misses: in_.misses,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> Hash {
        Hash([n; 32])
    }
    fn bytes(n: usize) -> Arc<Vec<u8>> {
        Arc::new(vec![7u8; n])
    }

    #[test]
    fn a_stored_object_comes_back() {
        let c = MemoryCache::default();
        c.put(h(1), bytes(10));
        assert_eq!(c.get(h(1)).map(|b| b.len()), Some(10));
        assert!(c.get(h(2)).is_none());
    }

    #[test]
    fn hits_and_misses_are_counted() {
        let c = MemoryCache::default();
        c.put(h(1), bytes(10));
        c.get(h(1));
        c.get(h(2));
        let s = c.stats();
        assert_eq!((s.hits, s.misses), (1, 1));
    }

    #[test]
    fn the_bound_is_bytes_and_the_coldest_goes_first() {
        let c = MemoryCache::new(1200, DEFAULT_TTL);
        c.put(h(1), bytes(300));
        c.put(h(2), bytes(300));
        c.put(h(3), bytes(300));

        // Touch 1 and 3, leaving 2 as the least recently used.
        c.get(h(1));
        c.get(h(3));
        c.put(h(4), bytes(300));
        c.put(h(5), bytes(300));

        assert!(c.get(h(2)).is_none(), "the coldest should have gone");
        assert!(c.get(h(5)).is_some(), "the newest should be here");
        assert!(c.stats().bytes <= 1200);
    }

    #[test]
    fn no_object_may_take_more_than_its_share() {
        // Admitting one object must never be able to evict most of the rest.
        let c = MemoryCache::new(1000, DEFAULT_TTL);
        c.put(h(1), bytes(400));
        assert!(c.get(h(1)).is_none(), "over a quarter of the cache");
        assert_eq!(c.stats().entries, 0);

        c.put(h(2), bytes(250));
        assert!(c.get(h(2)).is_some(), "exactly a quarter is allowed");
    }

    #[test]
    fn an_expired_entry_reads_as_absent() {
        let c = MemoryCache::new(1000, Duration::from_millis(30));
        c.put(h(1), bytes(10));
        assert!(c.get(h(1)).is_some());
        std::thread::sleep(Duration::from_millis(50));
        assert!(c.get(h(1)).is_none(), "past its age limit");
        assert_eq!(c.stats().bytes, 0, "and its bytes are handed back");
    }

    #[test]
    fn forgetting_and_clearing_return_the_bytes() {
        let c = MemoryCache::default();
        c.put(h(1), bytes(10));
        c.put(h(2), bytes(20));
        c.forget(h(1));
        assert_eq!(c.stats().bytes, 20);
        c.clear();
        assert_eq!(c.stats(), CacheStats { entries: 0, bytes: 0, ..c.stats() });
    }

    #[test]
    fn a_cache_of_no_capacity_holds_nothing() {
        let c = MemoryCache::new(0, DEFAULT_TTL);
        c.put(h(1), bytes(10));
        assert!(c.get(h(1)).is_none());
    }

    #[test]
    fn the_null_cache_holds_nothing_and_says_so() {
        let c = NoCache;
        c.put(h(1), bytes(10));
        assert!(c.get(h(1)).is_none());
        assert_eq!(c.stats().entries, 0);
    }

    /// The store holds `Arc<dyn ObjectCache>`, so this has to be object-safe.
    #[test]
    fn the_trait_is_usable_behind_a_pointer() {
        let c: Arc<dyn ObjectCache> = Arc::new(MemoryCache::default());
        c.put(h(1), bytes(10));
        assert_eq!(c.get(h(1)).map(|b| b.len()), Some(10));
        c.clear();
        assert!(c.get(h(1)).is_none());
    }
}

// ---- tiering, and a shared backend ---------------------------------------

/// Two caches, near one in front of far one.
///
/// The near cache is memory and answers in nanoseconds. The far cache is
/// shared — Valkey or Redis — and answers in a network round trip.
///
/// This ordering is not a detail, it is the whole reason the far cache is
/// worth having at all. Reading a packed object from a local disk here costs
/// about seven microseconds; a round trip to Redis on the same host costs
/// hundreds. A shared cache used *instead of* the disk would therefore be
/// slower than the thing it replaced, by a factor of ten or more.
///
/// It earns its place only where a miss is genuinely expensive: several hub
/// processes that would each otherwise start cold, or objects on storage a
/// good deal slower than a local disk. Then the far cache saves the miss, and
/// the near cache saves the round trip.
pub struct Tiered {
    near: Arc<dyn ObjectCache>,
    far: Arc<dyn ObjectCache>,
}

impl Tiered {
    pub fn new(near: Arc<dyn ObjectCache>, far: Arc<dyn ObjectCache>) -> Self {
        Tiered { near, far }
    }
}

impl ObjectCache for Tiered {
    fn get(&self, h: Hash) -> Option<Arc<Vec<u8>>> {
        if let Some(hit) = self.near.get(h) {
            return Some(hit);
        }
        // Promote what the far cache had, so the second read of it is local.
        let hit = self.far.get(h)?;
        self.near.put(h, Arc::clone(&hit));
        Some(hit)
    }

    fn put(&self, h: Hash, bytes: Arc<Vec<u8>>) {
        self.near.put(h, Arc::clone(&bytes));
        self.far.put(h, bytes);
    }

    fn forget(&self, h: Hash) {
        self.near.forget(h);
        self.far.forget(h);
    }

    fn clear(&self) {
        self.near.clear();
        self.far.clear();
    }

    /// The near cache's, plus the far cache's traffic folded in — one number
    /// for "how often did this avoid a disk read".
    fn stats(&self) -> CacheStats {
        let n = self.near.stats();
        let f = self.far.stats();
        CacheStats {
            entries: n.entries,
            bytes: n.bytes,
            capacity: n.capacity,
            hits: n.hits + f.hits,
            misses: f.misses,
        }
    }
}

#[cfg(feature = "redis-cache")]
mod shared {
    use super::{CacheStats, Hash, ObjectCache};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// Object bytes in Valkey or Redis, shared by every process that points at
    /// it.
    ///
    /// Read [`super::Tiered`] before reaching for this: on a single host with
    /// local storage it is slower than the disk it would replace. It is for
    /// the case where the miss is the expensive part.
    ///
    /// # Keys and expiry
    ///
    /// Keyed by the object's hash under a prefix, so several repositories —
    /// and several *servers* — can share one instance without colliding: the
    /// key is a digest of the value, so two writers of the same key are
    /// necessarily writing the same bytes.
    ///
    /// Expiry is Redis's own, set per key. There is no eviction policy here
    /// beyond that and whatever `maxmemory-policy` the server is configured
    /// with, which is the right division: how much memory to spend is the
    /// operator's decision, not this program's.
    pub struct RedisCache {
        pool: Mutex<Vec<redis::Connection>>,
        client: redis::Client,
        prefix: String,
        ttl_secs: u64,
        max_entry: usize,
        hits: AtomicU64,
        misses: AtomicU64,
    }

    /// How many connections to keep. A read is a single round trip, so a small
    /// pool covers a lot of concurrency; opening one per call would cost more
    /// than the cache saves.
    const POOL: usize = 8;

    impl RedisCache {
        /// Connect, and check the server actually answers.
        ///
        /// Eagerly rather than on first use, so a misconfigured URL is a
        /// startup failure the operator sees rather than a silent miss on
        /// every read for the life of the process.
        pub fn connect(url: &str, prefix: &str, ttl: std::time::Duration) -> anyhow::Result<Self> {
            let client = redis::Client::open(url)?;
            let mut probe = client.get_connection()?;
            redis::cmd("PING").exec(&mut probe)?;

            Ok(RedisCache {
                pool: Mutex::new(vec![probe]),
                client,
                prefix: prefix.to_string(),
                ttl_secs: ttl.as_secs().max(1),
                // The same share rule the memory cache uses, in absolute
                // terms: a multi-megabyte chunk is not worth a round trip.
                max_entry: 1024 * 1024,
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
            })
        }

        fn key(&self, h: Hash) -> String {
            format!("{}{}", self.prefix, h.to_hex())
        }

        /// Run one command on a pooled connection.
        ///
        /// A failure is never propagated: a cache that cannot be reached is a
        /// cache that misses, and the store falls back to disk. Turning a
        /// Redis outage into an error on every object read would take the
        /// server down to protect an optimisation.
        fn with<T>(&self, f: impl FnOnce(&mut redis::Connection) -> redis::RedisResult<T>) -> Option<T> {
            let mut conn = {
                let mut pool = self.pool.lock().unwrap();
                match pool.pop() {
                    Some(c) => c,
                    None => self.client.get_connection().ok()?,
                }
            };

            let out = f(&mut conn);

            // A connection that just errored may be in an unknown state, so it
            // is dropped rather than returned to the pool.
            if out.is_ok() {
                let mut pool = self.pool.lock().unwrap();
                if pool.len() < POOL {
                    pool.push(conn);
                }
            }
            out.ok()
        }
    }

    impl super::BlobCache for RedisCache {
        fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
            let key = format!("{}{key}", self.prefix);
            let got: Option<Vec<u8>> = self.with(|c| redis::cmd("GET").arg(&key).query(c))?;
            match got {
                Some(bytes) if !bytes.is_empty() => {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    Some(Arc::new(bytes))
                }
                _ => {
                    self.misses.fetch_add(1, Ordering::Relaxed);
                    None
                }
            }
        }

        fn put(&self, key: &str, bytes: Arc<Vec<u8>>, ttl: std::time::Duration) {
            if bytes.len() > self.max_entry {
                return;
            }
            let key = format!("{}{key}", self.prefix);
            self.with(|c| {
                redis::cmd("SET")
                    .arg(&key)
                    .arg(bytes.as_slice())
                    .arg("EX")
                    .arg(ttl.as_secs().max(1))
                    .exec(c)
            });
        }

        fn forget(&self, key: &str) {
            let key = format!("{}{key}", self.prefix);
            self.with(|c| redis::cmd("DEL").arg(&key).exec(c));
        }

        fn stats(&self) -> CacheStats {
            <Self as super::ObjectCache>::stats(self)
        }
    }

    impl ObjectCache for RedisCache {
        fn get(&self, h: Hash) -> Option<Arc<Vec<u8>>> {
            let key = self.key(h);
            let got: Option<Vec<u8>> = self.with(|c| redis::cmd("GET").arg(&key).query(c))?;
            match got {
                Some(bytes) if !bytes.is_empty() => {
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    Some(Arc::new(bytes))
                }
                _ => {
                    self.misses.fetch_add(1, Ordering::Relaxed);
                    None
                }
            }
        }

        fn put(&self, h: Hash, bytes: Arc<Vec<u8>>) {
            if bytes.len() > self.max_entry {
                return;
            }
            let key = self.key(h);
            // SET with an expiry in one command: two commands would leave a
            // key without one if the process died between them.
            self.with(|c| {
                redis::cmd("SET")
                    .arg(&key)
                    .arg(bytes.as_slice())
                    .arg("EX")
                    .arg(self.ttl_secs)
                    .exec(c)
            });
        }

        fn forget(&self, h: Hash) {
            let key = self.key(h);
            self.with(|c| redis::cmd("DEL").arg(&key).exec(c));
        }

        /// Drop this server's objects.
        ///
        /// Scans for the prefix rather than issuing `FLUSHDB`, because the
        /// instance may not be ours alone — flushing someone else's data to
        /// tidy ours is not a trade this gets to make.
        fn clear(&self) {
            let pattern = format!("{}*", self.prefix);
            self.with(|c| {
                let mut cursor: u64 = 0;
                loop {
                    let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                        .arg(cursor)
                        .arg("MATCH")
                        .arg(&pattern)
                        .arg("COUNT")
                        .arg(500)
                        .query(c)?;
                    if !keys.is_empty() {
                        redis::cmd("DEL").arg(&keys).exec(c)?;
                    }
                    cursor = next;
                    if cursor == 0 {
                        return Ok(());
                    }
                }
            });
        }

        fn stats(&self) -> CacheStats {
            CacheStats {
                // Size lives on the server and is its business, not ours.
                entries: 0,
                bytes: 0,
                capacity: 0,
                hits: self.hits.load(Ordering::Relaxed),
                misses: self.misses.load(Ordering::Relaxed),
            }
        }
    }
}

#[cfg(feature = "redis-cache")]
pub use shared::RedisCache;

#[cfg(test)]
mod tier_tests {
    use super::*;

    fn h(n: u8) -> Hash {
        Hash([n; 32])
    }
    fn bytes(n: usize) -> Arc<Vec<u8>> {
        Arc::new(vec![7u8; n])
    }

    #[test]
    fn a_write_reaches_both_tiers() {
        let near = Arc::new(MemoryCache::default());
        let far = Arc::new(MemoryCache::default());
        let t = Tiered::new(near.clone(), far.clone());

        t.put(h(1), bytes(10));
        assert!(near.get(h(1)).is_some());
        assert!(far.get(h(1)).is_some());
    }

    #[test]
    fn a_far_hit_is_promoted_so_the_next_read_is_local() {
        let near = Arc::new(MemoryCache::default());
        let far = Arc::new(MemoryCache::default());
        // Only the far tier has it — as after a restart, or for a second
        // process that never read it.
        far.put(h(1), bytes(10));

        let t = Tiered::new(near.clone(), far.clone());
        assert!(near.get(h(1)).is_none(), "not local yet");
        assert!(t.get(h(1)).is_some());
        assert!(near.get(h(1)).is_some(), "promoted on the way back");
    }

    #[test]
    fn clearing_reaches_both() {
        let near = Arc::new(MemoryCache::default());
        let far = Arc::new(MemoryCache::default());
        let t = Tiered::new(near.clone(), far.clone());
        t.put(h(1), bytes(10));
        t.clear();
        assert!(near.get(h(1)).is_none());
        assert!(far.get(h(1)).is_none(), "collection must not leave bytes behind");
    }

    #[test]
    fn a_far_tier_that_is_unreachable_only_costs_hits() {
        // NoCache stands in for a Redis that cannot be reached: every read is
        // a miss, and nothing breaks.
        let near = Arc::new(MemoryCache::default());
        let t = Tiered::new(near, Arc::new(NoCache));
        t.put(h(1), bytes(10));
        assert!(t.get(h(1)).is_some(), "the near tier still answers");
        assert!(t.get(h(2)).is_none());
    }

    /// Storing into a full cache costs the same however much it holds.
    ///
    /// Eviction used to find its victim by scanning every entry for the
    /// smallest `used`. That is cheap while a cache evicts in bursts and
    /// quadratic when it evicts on every store, which is what a full cache
    /// under a large transfer does. A hub serving a second clone of a big
    /// history spent most of its CPU in that scan, and more of it the longer
    /// the process had been up.
    #[test]
    fn storing_into_a_full_cache_does_not_slow_down_as_it_fills() {
        // Room for twenty thousand entries, then ten times that many stores,
        // every one of which has to evict.
        // Its own key function: the shared one takes a byte, and this needs
        // two hundred thousand distinct names.
        let key = |n: u32| {
            let mut b = [0u8; 32];
            b[..4].copy_from_slice(&n.to_le_bytes());
            Hash(b)
        };
        let held = 20_000;
        let c = MemoryCache::new(held * 64, Duration::ZERO);
        let started = std::time::Instant::now();
        for i in 0..(held as u32 * 10) {
            c.put(key(i), bytes(64));
        }
        let took = started.elapsed();

        assert_eq!(c.stats().entries, held, "it should be holding its capacity");
        assert!(
            // The scan takes five seconds here; the pop takes fifty
            // milliseconds. Two is clear of both.
            took < Duration::from_secs(2),
            "two hundred thousand stores took {took:?}, which is the scan coming back"
        );
    }
}
