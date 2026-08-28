//! Whole-store integrity checking.
//!
//! Because every object's name is the hash of its own bytes, verifying the
//! entire repository needs no external checksum file, no signature, and no
//! trusted third party. Re-hash each object; if it matches its name, it is
//! exactly the data that was originally written. Then check that every hash it
//! references actually exists — that gives you completeness on top of integrity.

use crate::hash::Hash;
use crate::repo::Repo;
use anyhow::Result;
use std::collections::HashSet;

#[derive(Debug, Default)]
pub struct FsckReport {
    pub checked: usize,
    pub corrupt: Vec<(Hash, String)>,
    /// Referenced but absent — the repo is incomplete.
    pub missing: Vec<(Hash, Hash)>, // (referrer, missing target)
    /// Present but unreachable from any ref — garbage collectable.
    pub unreachable: Vec<Hash>,
    pub total_bytes: u64,
}

impl FsckReport {
    pub fn is_healthy(&self) -> bool {
        self.corrupt.is_empty() && self.missing.is_empty()
    }
}

pub fn fsck(repo: &Repo) -> Result<FsckReport> {
    let mut report = FsckReport::default();
    let all: Vec<Hash> = repo.store.iter_ids()?;
    let present: HashSet<Hash> = all.iter().copied().collect();

    for id in &all {
        report.checked += 1;
        match repo.store.get_raw(*id) {
            Ok(bytes) => {
                report.total_bytes += bytes.len() as u64;
                let actual = Hash(*blake3::hash(&bytes).as_bytes());
                if actual != *id {
                    report.corrupt.push((*id, format!("hashes to {}", actual.short())));
                    continue;
                }
                match crate::store::Store::decode_framed(&bytes) {
                    Ok(obj) => {
                        for link in obj.links() {
                            if !present.contains(&link) {
                                report.missing.push((*id, link));
                            }
                        }
                    }
                    Err(e) => report.corrupt.push((*id, format!("undecodable: {e}"))),
                }
            }
            Err(e) => report.corrupt.push((*id, e.to_string())),
        }
    }

    // Reachability: everything findable from any branch or from HEAD.
    let mut reachable = HashSet::new();
    // Tags are roots too, or a commit kept alive only by a tag is reported as
    // unreachable — and the advice fsck gives for that is to garbage collect.
    let mut stack: Vec<Hash> = repo.all_refs()?.values().copied().collect();
    if let Some(h) = repo.head_commit()? {
        stack.push(h);
    }
    while let Some(h) = stack.pop() {
        if !reachable.insert(h) || !present.contains(&h) {
            continue;
        }
        if let Ok(obj) = repo.store.get(h) {
            stack.extend(obj.links());
        }
    }
    report.unreachable = all.into_iter().filter(|h| !reachable.contains(h)).collect();

    Ok(report)
}
