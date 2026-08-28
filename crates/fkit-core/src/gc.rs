//! Garbage collection.
//!
//! An object is garbage when nothing reachable from a ref points at it — the
//! leftovers of an amended commit, an abandoned branch, or a push that failed
//! partway. Finding them is a graph walk; the hard part is not deleting
//! something that is about to become reachable.
//!
//! # The race, and the age guard
//!
//! A push writes objects and *then* moves a ref. Between those two steps its
//! objects are unreachable by definition. A collector running in that window
//! would delete a live push out from under it.
//!
//! Nothing in a content-addressed store records intent, so the only honest
//! defence is time: objects younger than `min_age` are never collected, however
//! unreachable they look. Git uses two weeks for the same reason. `--prune-all`
//! removes the guard, and should only be used when nothing else is writing.
//!
//! # Packed objects
//!
//! A packed object cannot be unlinked individually — it is bytes inside a shared
//! segment. Segments are therefore *compacted*: live objects are copied into a
//! new segment and the old files removed. That costs a rewrite, so a segment is
//! only compacted when enough of it is actually dead to be worth it.

use crate::hash::Hash;
use crate::store::Store;
use anyhow::Result;
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

/// Default grace period for unreachable objects.
///
/// Long enough to cover any plausible in-flight push, short enough that a
/// developer running `fkit gc` after a mistake sees space returned the same day.
pub const DEFAULT_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Only rewrite a segment when at least this fraction of it is dead.
pub const COMPACT_THRESHOLD: f64 = 0.20;

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub min_age: Duration,
    /// Report what would happen without changing anything.
    pub dry_run: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { min_age: DEFAULT_MIN_AGE, dry_run: false }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Report {
    pub total: usize,
    pub reachable: usize,
    pub unreachable: usize,
    /// Unreachable but retained because they are younger than `min_age`.
    pub too_young: usize,
    pub loose_removed: usize,
    pub packed_dropped: usize,
    pub segments_compacted: usize,
    pub bytes_reclaimed: u64,
}

/// Everything reachable from `roots`, following every edge.
pub fn reachable(store: &Store, roots: &[Hash]) -> Result<HashSet<Hash>> {
    let mut seen = HashSet::new();
    let mut stack: Vec<Hash> = roots.to_vec();
    while let Some(h) = stack.pop() {
        if !seen.insert(h) {
            continue;
        }
        // A root naming something absent is not fatal here — `fsck` is the tool
        // that reports incompleteness; gc only needs to know what to keep.
        if let Ok(obj) = store.get(h) {
            stack.extend(obj.links());
        }
    }
    Ok(seen)
}

fn older_than(path: &std::path::Path, age: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|d| d >= age)
        .unwrap_or(false)
}

/// Collect unreachable objects.
pub fn collect(store: &Store, roots: &[Hash], opts: Options) -> Result<Report> {
    let live = reachable(store, roots)?;
    let all = store.iter_ids()?;

    let mut report = Report {
        total: all.len(),
        reachable: live.len(),
        ..Default::default()
    };

    // ---- loose objects: unlink individually ----
    for id in store.loose_ids()? {
        if live.contains(&id) {
            continue;
        }
        report.unreachable += 1;
        let path = store.loose_path(id);
        if opts.min_age > Duration::ZERO && !older_than(&path, opts.min_age) {
            report.too_young += 1;
            continue;
        }
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if !opts.dry_run {
            std::fs::remove_file(&path)?;
        }
        report.loose_removed += 1;
        report.bytes_reclaimed += size;
    }

    // ---- packed objects: compact whole segments ----
    let plan = store.with_pack(|pack| {
        let mut targets: Vec<u32> = Vec::new();
        let mut dropped = 0usize;

        for (id, path) in pack.segments() {
            // The age guard applies per segment: a segment written moments ago
            // may hold a push still in progress.
            if opts.min_age > Duration::ZERO && !older_than(&path, opts.min_age) {
                let young = pack.ids_in(id).into_iter().filter(|h| !live.contains(h)).count();
                return_young(&mut report, young);
                continue;
            }
            let ids = pack.ids_in(id);
            let dead = ids.iter().filter(|h| !live.contains(h)).count();
            if dead == 0 {
                continue;
            }
            // Rewriting a segment to reclaim a sliver is not worth the I/O.
            if (dead as f64) < ids.len() as f64 * COMPACT_THRESHOLD {
                continue;
            }
            targets.push(id);
            dropped += dead;
        }
        (targets, dropped)
    });

    if let Some((targets, dropped)) = plan {
        report.unreachable += dropped;
        if !targets.is_empty() && !opts.dry_run {
            let stats = store.compact_segments(&targets, &live)?;
            report.packed_dropped = stats.dropped;
            report.segments_compacted = targets.len();
            report.bytes_reclaimed += stats.reclaimed;
        } else if !targets.is_empty() {
            report.packed_dropped = dropped;
            report.segments_compacted = targets.len();
        }
    }

    Ok(report)
}

fn return_young(report: &mut Report, n: usize) {
    report.unreachable += n;
    report.too_young += n;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Commit, Object};

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "fkit-gc-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// A commit whose tree is a run holding one file.
    fn seed(store: &Store, body: &str) -> Hash {
        let sink = crate::store::Sink::writing(store);
        let f = crate::ingest::ingest_bytes(&sink, body.as_bytes()).unwrap();
        let (tree, _, _) = crate::ingest::build_tree(
            &sink,
            vec![crate::object::TreeEntry {
                name: "f.txt".into(),
                kind: crate::object::EntryKind::File { exec: false },
                hash: f.hash,
                size: f.size,
            }],
        )
        .unwrap();
        let (c, _) = store
            .put(&Object::Commit(Commit {
                tree,
                parents: vec![],
                author: "t".into(),
                timestamp: 1,
                message: body.into(),
            }))
            .unwrap();
        c
    }

    #[test]
    fn unreachable_objects_are_collected_and_reachable_ones_are_not() {
        let dir = tmp("basic");
        let store = Store::open(&dir).unwrap();
        let keep = seed(&store, "keep me");
        let drop = seed(&store, "abandoned");

        let before = store.iter_ids().unwrap().len();
        // min_age zero: this test is about reachability, not the race guard.
        let r = collect(&store, &[keep], Options { min_age: Duration::ZERO, dry_run: false })
            .unwrap();

        // Assert the outcome, not the mechanism — objects are packed by
        // default now, so collection happens through segment compaction.
        assert!(
            r.loose_removed + r.packed_dropped > 0,
            "the abandoned commit should be collected"
        );
        assert!(store.has(keep), "a reachable commit must survive");
        assert!(!store.has(drop), "an unreachable commit must be gone");
        assert!(store.iter_ids().unwrap().len() < before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn young_objects_are_spared_even_when_unreachable() {
        let dir = tmp("young");
        let store = Store::open(&dir).unwrap();
        let keep = seed(&store, "keep");
        let fresh = seed(&store, "just written");

        // Everything here was created milliseconds ago, so a one-hour guard
        // must protect all of it — this is the in-flight-push case.
        let r = collect(
            &store,
            &[keep],
            Options { min_age: Duration::from_secs(3600), dry_run: false },
        )
        .unwrap();

        assert_eq!(r.loose_removed + r.packed_dropped, 0, "nothing old enough to collect");
        assert!(r.too_young > 0);
        assert!(store.has(fresh), "a freshly written object must not be collected");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_dry_run_changes_nothing() {
        let dir = tmp("dry");
        let store = Store::open(&dir).unwrap();
        let keep = seed(&store, "keep");
        let drop = seed(&store, "garbage");

        let before = store.iter_ids().unwrap().len();
        let r = collect(&store, &[keep], Options { min_age: Duration::ZERO, dry_run: true })
            .unwrap();

        assert!(
            r.loose_removed + r.packed_dropped > 0,
            "it should still report what it would remove"
        );
        assert_eq!(store.iter_ids().unwrap().len(), before, "but remove nothing");
        assert!(store.has(drop));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn packed_segments_are_compacted_and_survivors_still_read() {
        let dir = tmp("packed");
        let store = Store::open(&dir).unwrap();

        let keep = seed(&store, "the surviving commit");
        for i in 0..40 {
            seed(&store, &format!("garbage number {i}"));
        }
        store.pack_loose().unwrap();
        let packed_before = store.packed_count();
        assert!(packed_before > 40);

        let r = collect(&store, &[keep], Options { min_age: Duration::ZERO, dry_run: false })
            .unwrap();

        assert!(r.segments_compacted > 0, "the segment was mostly dead and should compact");
        assert!(r.packed_dropped > 0);
        assert!(store.packed_count() < packed_before);

        // The point of the whole exercise: what survived is still readable.
        assert!(store.has(keep));
        let obj = store.get_verified(keep).unwrap();
        assert!(matches!(obj, Object::Commit(_)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_mostly_live_segment_is_left_alone() {
        let dir = tmp("mostly-live");
        let store = Store::open(&dir).unwrap();

        let roots: Vec<Hash> = (0..20).map(|i| seed(&store, &format!("live {i}"))).collect();
        seed(&store, "one piece of garbage");
        store.pack_loose().unwrap();
        let before = store.packed_count();

        let r = collect(&store, &roots, Options { min_age: Duration::ZERO, dry_run: false })
            .unwrap();

        assert_eq!(r.segments_compacted, 0, "rewriting a live segment for one object is waste");
        assert_eq!(store.packed_count(), before);
        let _ = std::fs::remove_dir_all(dir);
    }
}
