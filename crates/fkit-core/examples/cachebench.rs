// Measures the object cache against the real store: walk a commit's whole
// tree repeatedly, which is what a page render does.
use fkit_core::{cache::NoCache, hash::Hash, object::Object, store::Store};
use std::sync::Arc;
use std::time::Instant;

fn walk(store: &Store, root: Hash) -> usize {
    let mut n = 0usize;
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    while let Some(h) = stack.pop() {
        if !seen.insert(h) { continue; }
        if let Ok(o) = store.get(h) {
            n += 1;
            // Structure only: chunks are the bulk and are not what a page walk
            // re-reads.
            if !matches!(o, Object::Chunk(_)) {
                stack.extend(o.links());
            }
        }
    }
    n
}

fn main() {
    let dir = std::env::args().nth(1).expect("store dir");
    let tip = std::env::args().nth(2).expect("commit hash");
    let root = Hash::from_hex(&tip).expect("hash");

    let store = Store::open(&dir).expect("open");
    let cold = walk(&store, root);
    println!("objects walked: {cold}");

    // One store, cache off. Reopening it per pass would also re-read the pack
    // index, which measures the cost of opening a store rather than the cost
    // of reading objects — a different thing, and not what the cache changes.
    let mut off = Store::open(&dir).expect("open");
    off.set_cache(Arc::new(NoCache));
    walk(&off, root); // let the OS page cache settle, as it would in a server
    let t = Instant::now();
    for _ in 0..20 {
        walk(&off, root);
    }
    let uncached = t.elapsed();

    // Warm: one store, cache doing its job.
    let s = Store::open(&dir).expect("open");
    walk(&s, root); // prime
    let t = Instant::now();
    for _ in 0..20 { walk(&s, root); }
    let cached = t.elapsed();

    println!("uncached  {:>8.2?} for 20 walks", uncached);
    println!("cached    {:>8.2?} for 20 walks", cached);
    println!("speedup   {:.1}x", uncached.as_secs_f64() / cached.as_secs_f64());
    println!("{:?}", s.cache_stats());
}
