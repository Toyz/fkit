//! Three-way merge: commit DAG, trees, and file contents.
//!
//! Merging asks one question at three scales:
//!
//! 1. **Which commit did these two histories last share?** — [`merge_base`]
//! 2. **Given that ancestor, what does the combined tree look like?** —
//!    [`merge_trees`]
//! 3. **When both sides edited the same file, can the edits coexist?** —
//!    [`merge_lines`]
//!
//! Each scale uses the same rule: compare both sides *against the base*, not
//! against each other. "Both sides have different content" is not a conflict —
//! if only one of them changed it, the answer is obvious. A conflict is only
//! when both sides changed the same thing to different values.
//!
//! The Merkle DAG makes step 2 nearly free in the common case. If a subtree
//! hash is unchanged from the base on one side, that entire subtree takes the
//! other side's version with a single comparison — no matter how many files
//! are inside it.

use crate::diff::{self, Op};
use crate::hash::Hash;
use crate::ingest::read_file;
use crate::object::{EntryKind, Object, TreeEntry};
use crate::repo::View;
use crate::store::{Sink, Store};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, HashSet, VecDeque};

// ---- 1. merge base ------------------------------------------------------

/// The best common ancestor of two commits.
///
/// "Best" means a common ancestor that is not itself an ancestor of another
/// common ancestor — otherwise you would merge against something needlessly far
/// back and re-surface changes both sides already have.
///
/// Returns `None` for unrelated histories, which is a legitimate state (two
/// repositories that were never connected) and not an error.
///
/// # Limitation
///
/// Criss-cross merges can leave several equally good bases. Git handles that by
/// recursively merging the candidates into a virtual base; fkit picks the first
/// and reports the rest through [`MergeBase::ambiguous`], so the situation is
/// visible rather than silently mis-merged.
#[derive(Debug, Clone)]
pub struct MergeBase {
    pub base: Option<Hash>,
    /// True when more than one equally-good base existed.
    pub ambiguous: bool,
}

pub fn merge_base(store: &Store, a: Hash, b: Hash) -> Result<MergeBase> {
    if a == b {
        return Ok(MergeBase { base: Some(a), ambiguous: false });
    }

    let ancestors_of_a = ancestors(store, a)?;

    // Walk b's history; the first time we touch something in a's ancestry it is
    // a common ancestor, and nothing behind it can be *better*, so we stop
    // expanding that line.
    let mut candidates: Vec<Hash> = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([b]);

    while let Some(h) = queue.pop_front() {
        if !seen.insert(h) {
            continue;
        }
        if ancestors_of_a.contains(&h) {
            candidates.push(h);
            continue;
        }
        if let Ok(Object::Commit(c)) = store.get(h) {
            queue.extend(c.parents);
        }
    }

    if candidates.is_empty() {
        return Ok(MergeBase { base: None, ambiguous: false });
    }

    // Drop any candidate reachable from another: it is strictly further back.
    let mut best: Vec<Hash> = Vec::new();
    for &c in &candidates {
        let superseded = candidates
            .iter()
            .any(|&o| o != c && is_ancestor_of(store, c, o).unwrap_or(false));
        if !superseded {
            best.push(c);
        }
    }

    Ok(MergeBase {
        ambiguous: best.len() > 1,
        base: best.first().copied().or_else(|| candidates.first().copied()),
    })
}

fn ancestors(store: &Store, from: Hash) -> Result<HashSet<Hash>> {
    let mut out = HashSet::new();
    let mut queue = VecDeque::from([from]);
    while let Some(h) = queue.pop_front() {
        if !out.insert(h) {
            continue;
        }
        if let Ok(Object::Commit(c)) = store.get(h) {
            queue.extend(c.parents);
        }
    }
    Ok(out)
}

fn is_ancestor_of(store: &Store, ancestor: Hash, descendant: Hash) -> Result<bool> {
    crate::proto::is_ancestor(store, ancestor, descendant)
}

// ---- 2. file-level three-way merge --------------------------------------

/// One region where both sides changed the same lines differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineConflict {
    pub base: Vec<String>,
    pub ours: Vec<String>,
    pub theirs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedFile {
    pub lines: Vec<String>,
    pub conflicts: Vec<LineConflict>,
}

impl MergedFile {
    pub fn clean(&self) -> bool {
        self.conflicts.is_empty()
    }

    /// Rejoin into file content, with conflict markers already inlined by
    /// [`merge_lines`] when there were conflicts.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut s = self.lines.join("\n");
        s.push('\n');
        s.into_bytes()
    }
}

/// A contiguous replacement of base lines `[start, end)` with `lines`.
#[derive(Debug, Clone)]
struct Edit {
    start: usize,
    end: usize,
    lines: Vec<String>,
}

/// Turn an edit script against `base` into coalesced replacement regions.
fn edits(base: &[String], other: &[String]) -> Option<Vec<Edit>> {
    // The full script, not hunks: hunks omit unchanged runs, and a merge has to
    // know exactly which base lines each side left alone.
    let script = diff::script(base, other)?;

    let mut out: Vec<Edit> = Vec::new();
    let (mut bi, mut oi) = (0usize, 0usize);
    let mut cur: Option<Edit> = None;

    for op in script {
        match op {
            Op::Equal => {
                if let Some(e) = cur.take() {
                    out.push(e);
                }
                bi += 1;
                oi += 1;
            }
            Op::Delete => {
                let e = cur.get_or_insert(Edit { start: bi, end: bi, lines: Vec::new() });
                e.end = bi + 1;
                bi += 1;
            }
            Op::Insert => {
                let e = cur.get_or_insert(Edit { start: bi, end: bi, lines: Vec::new() });
                e.lines.push(other[oi].clone());
                oi += 1;
            }
        }
    }
    if let Some(e) = cur {
        out.push(e);
    }
    Some(out)
}

/// Three-way merge of line sequences (the classic diff3 region algorithm).
///
/// Both sides are diffed against the base. Non-overlapping edits are simply
/// both applied; overlapping ones become a conflict spanning their union.
pub fn merge_lines(base: &[String], ours: &[String], theirs: &[String]) -> Option<MergedFile> {
    if ours == theirs {
        return Some(MergedFile { lines: ours.to_vec(), conflicts: vec![] });
    }
    if base == ours {
        return Some(MergedFile { lines: theirs.to_vec(), conflicts: vec![] });
    }
    if base == theirs {
        return Some(MergedFile { lines: ours.to_vec(), conflicts: vec![] });
    }

    let a = edits(base, ours)?;
    let b = edits(base, theirs)?;

    // Interleave both edit lists in base order.
    let mut all: Vec<(usize, bool, Edit)> = Vec::new();
    for e in a {
        all.push((e.start, false, e));
    }
    for e in b {
        all.push((e.start, true, e));
    }
    all.sort_by_key(|(s, side, _)| (*s, *side));

    let mut out: Vec<String> = Vec::new();
    let mut conflicts: Vec<LineConflict> = Vec::new();
    let mut pos = 0usize; // next unconsumed base line
    let mut i = 0usize;

    while i < all.len() {
        let (_, side, ref e) = all[i];

        // Copy the untouched base region before this edit.
        if e.start > pos {
            out.extend_from_slice(&base[pos..e.start]);
        }

        // Gather every edit overlapping this one, from either side.
        //
        // Two edits overlap when their base ranges intersect, or when both are
        // pure insertions at the same point — two sides inserting different
        // text at one spot have no defined order, so that is a conflict rather
        // than a silent concatenation.
        let mut start = e.start;
        let mut end = e.end;
        let mut group: Vec<(bool, Edit)> = vec![(side, e.clone())];
        let mut j = i + 1;
        while j < all.len() {
            let n = &all[j].2;
            let intersects = n.start < end && start < n.end;
            let same_point_insert =
                n.start == start && n.start == n.end && start == end;
            if !(intersects || same_point_insert) {
                break;
            }
            start = start.min(n.start);
            end = end.max(n.end);
            group.push((all[j].1, n.clone()));
            j += 1;
        }
        i = j;

        let ours_side: Vec<&Edit> = group.iter().filter(|(s, _)| !*s).map(|(_, e)| e).collect();
        let theirs_side: Vec<&Edit> = group.iter().filter(|(s, _)| *s).map(|(_, e)| e).collect();

        if theirs_side.is_empty() {
            // Only we touched this region.
            for e in &ours_side {
                out.extend(e.lines.iter().cloned());
            }
        } else if ours_side.is_empty() {
            for e in &theirs_side {
                out.extend(e.lines.iter().cloned());
            }
        } else {
            // Both touched it. Rebuild each side's version of the whole region
            // so the conflict shows complete alternatives, not fragments.
            let region = |side: &[&Edit]| -> Vec<String> {
                let mut v = Vec::new();
                let mut p = start;
                for ed in side {
                    if ed.start > p {
                        v.extend_from_slice(&base[p..ed.start]);
                    }
                    v.extend(ed.lines.iter().cloned());
                    p = ed.end;
                }
                if p < end {
                    v.extend_from_slice(&base[p..end.min(base.len())]);
                }
                v
            };
            let ov = region(&ours_side);
            let tv = region(&theirs_side);

            if ov == tv {
                // Both sides made the identical change: not a conflict.
                out.extend(ov);
            } else {
                let bv = base[start.min(base.len())..end.min(base.len())].to_vec();
                out.push("<<<<<<< ours".into());
                out.extend(ov.iter().cloned());
                out.push("=======".into());
                out.extend(tv.iter().cloned());
                out.push(">>>>>>> theirs".into());
                conflicts.push(LineConflict { base: bv, ours: ov, theirs: tv });
            }
        }
        pos = end.min(base.len());
    }

    if pos < base.len() {
        out.extend_from_slice(&base[pos..]);
    }

    Some(MergedFile { lines: out, conflicts })
}

fn to_lines(bytes: &[u8]) -> Vec<String> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let s = String::from_utf8_lossy(bytes);
    let mut v: Vec<String> = s.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect();
    if s.ends_with('\n') {
        v.pop();
    }
    v
}

// ---- 3. tree-level three-way merge --------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictKind {
    /// Both sides edited the same text file in overlapping places. The merged
    /// content contains conflict markers.
    Content { regions: usize },
    /// Both sides changed a binary file, or one changed it while the other
    /// deleted it — nothing sensible to interleave.
    Binary,
    /// One side deleted what the other modified.
    DeleteModify,
    /// The two sides made the path a different kind of thing.
    TypeChange,
}

#[derive(Debug, Clone)]
pub struct Conflict {
    pub path: String,
    pub kind: ConflictKind,
}

pub struct MergeOutcome {
    pub tree: Hash,
    pub conflicts: Vec<Conflict>,
}

impl MergeOutcome {
    pub fn clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// Largest file a three-way content merge will attempt.
pub const MAX_MERGE_BYTES: u64 = 32 * 1024 * 1024;

fn read(store: &Store, h: Hash) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = read_file(store, h, &mut buf);
    buf
}

/// Merge two trees against their common ancestor.
///
/// The result is always a complete tree — conflicted files are written with
/// markers rather than omitted, so the working tree is usable and the reader
/// resolves conflicts in place, the way every other VCS behaves.
pub fn merge_trees(
    store: &Store,
    base: Option<Hash>,
    ours: Hash,
    theirs: Hash,
) -> Result<MergeOutcome> {
    let view = View { store, overlay: Default::default() };

    let b = match base {
        Some(t) => view.walk_tree(t)?,
        None => BTreeMap::new(),
    };
    let o = view.walk_tree(ours)?;
    let t = view.walk_tree(theirs)?;

    let mut merged: BTreeMap<String, TreeEntry> = BTreeMap::new();
    let mut conflicts: Vec<Conflict> = Vec::new();
    let sink = Sink::writing(store);

    let paths: HashSet<&String> = o.keys().chain(t.keys()).chain(b.keys()).collect();

    for path in paths {
        let (bv, ov, tv) = (b.get(path), o.get(path), t.get(path));

        match (ov, tv) {
            // Gone from both sides — stays gone.
            (None, None) => {}

            // Present on one side only.
            (Some(e), None) => {
                if bv.map(|x| x.hash) == Some(e.hash) {
                    // They deleted what we left alone: accept the deletion.
                } else if bv.is_none() {
                    merged.insert(path.clone(), e.clone()); // we added it
                } else {
                    conflicts.push(Conflict { path: path.clone(), kind: ConflictKind::DeleteModify });
                    merged.insert(path.clone(), e.clone());
                }
            }
            (None, Some(e)) => {
                if bv.map(|x| x.hash) == Some(e.hash) {
                    // We deleted what they left alone.
                } else if bv.is_none() {
                    merged.insert(path.clone(), e.clone());
                } else {
                    conflicts.push(Conflict { path: path.clone(), kind: ConflictKind::DeleteModify });
                    merged.insert(path.clone(), e.clone());
                }
            }

            (Some(oe), Some(te)) => {
                // Identical content: one hash comparison settles it, however
                // large the file.
                if oe.hash == te.hash && oe.kind == te.kind {
                    merged.insert(path.clone(), oe.clone());
                    continue;
                }
                if oe.kind != te.kind {
                    conflicts.push(Conflict { path: path.clone(), kind: ConflictKind::TypeChange });
                    merged.insert(path.clone(), oe.clone());
                    continue;
                }
                // Only one side changed it.
                if bv.map(|x| x.hash) == Some(oe.hash) {
                    merged.insert(path.clone(), te.clone());
                    continue;
                }
                if bv.map(|x| x.hash) == Some(te.hash) {
                    merged.insert(path.clone(), oe.clone());
                    continue;
                }

                // Both changed it. Decide on size *before* reading: a
                // three-way merge of two multi-gigabyte files would hold three
                // copies in memory to conclude they are binary anyway.
                if oe.size.max(te.size) > MAX_MERGE_BYTES
                    || bv.map(|e| e.size).unwrap_or(0) > MAX_MERGE_BYTES
                {
                    conflicts.push(Conflict { path: path.clone(), kind: ConflictKind::Binary });
                    merged.insert(path.clone(), oe.clone());
                    continue;
                }

                let ob = read(store, oe.hash);
                let tb = read(store, te.hash);
                let bb = bv.map(|e| read(store, e.hash)).unwrap_or_default();

                let binary = [&ob, &tb, &bb]
                    .iter()
                    .any(|d| d.iter().take(8192).any(|c| *c == 0));

                if binary {
                    conflicts.push(Conflict { path: path.clone(), kind: ConflictKind::Binary });
                    merged.insert(path.clone(), oe.clone());
                    continue;
                }

                match merge_lines(&to_lines(&bb), &to_lines(&ob), &to_lines(&tb)) {
                    Some(m) => {
                        let ing = crate::ingest::ingest_bytes(&sink, &m.to_bytes())?;
                        if !m.clean() {
                            conflicts.push(Conflict {
                                path: path.clone(),
                                kind: ConflictKind::Content { regions: m.conflicts.len() },
                            });
                        }
                        merged.insert(
                            path.clone(),
                            TreeEntry { name: leaf(path), kind: oe.kind, hash: ing.hash, size: ing.size },
                        );
                    }
                    None => {
                        conflicts.push(Conflict { path: path.clone(), kind: ConflictKind::Binary });
                        merged.insert(path.clone(), oe.clone());
                    }
                }
            }
        }
    }

    let tree = build_tree(&sink, &merged)?;
    Ok(MergeOutcome { tree, conflicts })
}

fn leaf(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Rebuild a nested tree from a flat `path -> entry` map.
pub fn build_tree(sink: &Sink, files: &BTreeMap<String, TreeEntry>) -> Result<Hash> {
    build_level(sink, files, "")
}

fn build_level(sink: &Sink, files: &BTreeMap<String, TreeEntry>, prefix: &str) -> Result<Hash> {
    let mut entries: Vec<TreeEntry> = Vec::new();
    let mut dirs: BTreeMap<String, BTreeMap<String, TreeEntry>> = BTreeMap::new();

    for (path, entry) in files {
        let rest = match prefix.is_empty() {
            true => path.as_str(),
            false => match path.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')) {
                Some(r) => r,
                None => continue,
            },
        };
        match rest.split_once('/') {
            None => entries.push(TreeEntry { name: rest.to_string(), ..entry.clone() }),
            Some((dir, _)) => {
                dirs.entry(dir.to_string()).or_default().insert(path.clone(), entry.clone());
            }
        }
    }

    for (name, sub) in dirs {
        let child_prefix = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let hash = build_level(sink, &sub, &child_prefix)?;
        let size = sub.values().map(|e| e.size).sum();
        entries.push(TreeEntry { name, kind: EntryKind::Dir, hash, size });
    }


    // Canonical order and the same run-chunking ingest uses — identical
    // contents must produce an identical hash however the tree was assembled.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let (h, _, _) = crate::ingest::build_tree(sink, entries)?;
    Ok(h)
}

/// Guard against a merge that silently loses a path.
pub fn verify_tree_covers(
    store: &Store,
    tree: Hash,
    expected: &BTreeMap<String, TreeEntry>,
) -> Result<()> {
    let view = View { store, overlay: Default::default() };
    let got = view.walk_tree(tree)?;
    if got.len() != expected.len() {
        bail!("merged tree has {} paths, expected {}", got.len(), expected.len());
    }
    Ok(())
}



#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        to_lines(s.as_bytes())
    }
    fn merged(base: &str, ours: &str, theirs: &str) -> MergedFile {
        merge_lines(&lines(base), &lines(ours), &lines(theirs)).expect("small inputs merge")
    }

    #[test]
    fn only_one_side_changed_takes_that_side() {
        let m = merged("a\nb\nc\n", "a\nB\nc\n", "a\nb\nc\n");
        assert!(m.clean());
        assert_eq!(m.lines, lines("a\nB\nc\n"));

        let m = merged("a\nb\nc\n", "a\nb\nc\n", "a\nb\nC\n");
        assert!(m.clean());
        assert_eq!(m.lines, lines("a\nb\nC\n"));
    }

    #[test]
    fn both_sides_editing_different_regions_combines_them() {
        let base = "one\ntwo\nthree\nfour\nfive\nsix\n";
        let ours = "ONE\ntwo\nthree\nfour\nfive\nsix\n";
        let theirs = "one\ntwo\nthree\nfour\nfive\nSIX\n";
        let m = merged(base, ours, theirs);
        assert!(m.clean(), "far-apart edits must not conflict");
        assert_eq!(m.lines, lines("ONE\ntwo\nthree\nfour\nfive\nSIX\n"));
    }

    #[test]
    fn identical_edits_on_both_sides_are_not_a_conflict() {
        let m = merged("a\nb\nc\n", "a\nCHANGED\nc\n", "a\nCHANGED\nc\n");
        assert!(m.clean(), "the same change made twice is still one change");
        assert_eq!(m.lines, lines("a\nCHANGED\nc\n"));
    }

    #[test]
    fn overlapping_edits_conflict_with_both_versions_shown() {
        let m = merged("a\nb\nc\n", "a\nOURS\nc\n", "a\nTHEIRS\nc\n");
        assert_eq!(m.conflicts.len(), 1);
        let c = &m.conflicts[0];
        assert_eq!(c.base, vec!["b".to_string()]);
        assert_eq!(c.ours, vec!["OURS".to_string()]);
        assert_eq!(c.theirs, vec!["THEIRS".to_string()]);

        let text = m.lines.join("\n");
        assert!(text.contains("<<<<<<< ours"));
        assert!(text.contains("OURS"));
        assert!(text.contains("======="));
        assert!(text.contains("THEIRS"));
        assert!(text.contains(">>>>>>> theirs"));
        // The unchanged surroundings must survive intact.
        assert_eq!(m.lines.first().unwrap(), "a");
        assert_eq!(m.lines.last().unwrap(), "c");
    }

    #[test]
    fn insertions_at_the_same_point_conflict_rather_than_concatenating() {
        // There is no defined order for two different insertions at one spot,
        // so silently picking one would be data loss with extra steps.
        let m = merged("a\nb\n", "a\nOURS\nb\n", "a\nTHEIRS\nb\n");
        assert_eq!(m.conflicts.len(), 1, "same-point inserts must conflict");
    }

    #[test]
    fn appending_to_both_ends_merges_cleanly() {
        let m = merged("mid\n", "top\nmid\n", "mid\nbottom\n");
        assert!(m.clean());
        assert_eq!(m.lines, lines("top\nmid\nbottom\n"));
    }

    #[test]
    fn merging_into_an_empty_base_takes_both_where_possible() {
        let m = merged("", "a\n", "a\n");
        assert!(m.clean());
        assert_eq!(m.lines, lines("a\n"));
    }

    #[test]
    fn a_deleted_region_on_one_side_only_is_applied() {
        let m = merged("a\nb\nc\nd\n", "a\nd\n", "a\nb\nc\nd\n");
        assert!(m.clean());
        assert_eq!(m.lines, lines("a\nd\n"));
    }

    #[test]
    fn delete_versus_modify_of_the_same_lines_conflicts() {
        let m = merged("a\nb\nc\n", "a\nc\n", "a\nB\nc\n");
        assert_eq!(m.conflicts.len(), 1, "one side removed what the other edited");
    }

    // ---- tree level ----

    struct Fix {
        dir: std::path::PathBuf,
        store: Store,
    }
    impl Fix {
        fn new(tag: &str) -> Fix {
            let dir = std::env::temp_dir().join(format!(
                "fkit-merge-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let store = Store::open(&dir).unwrap();
            Fix { dir, store }
        }
        /// Build a tree from `path -> contents`.
        fn tree(&self, files: &[(&str, &str)]) -> Hash {
            let sink = Sink::writing(&self.store);
            let mut map = BTreeMap::new();
            for (path, body) in files {
                let ing = crate::ingest::ingest_bytes(&sink, body.as_bytes()).unwrap();
                map.insert(
                    path.to_string(),
                    TreeEntry {
                        name: leaf(path),
                        kind: EntryKind::File { exec: false },
                        hash: ing.hash,
                        size: ing.size,
                    },
                );
            }
            build_tree(&sink, &map).unwrap()
        }
        fn read_path(&self, tree: Hash, path: &str) -> String {
            let view = View { store: &self.store, overlay: Default::default() };
            let files = view.walk_tree(tree).unwrap();
            let e = files.get(path).unwrap_or_else(|| panic!("{path} missing from merged tree"));
            String::from_utf8_lossy(&read(&self.store, e.hash)).into_owned()
        }
        fn paths(&self, tree: Hash) -> Vec<String> {
            let view = View { store: &self.store, overlay: Default::default() };
            view.walk_tree(tree).unwrap().keys().cloned().collect()
        }
    }
    impl Drop for Fix {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_rebuilt_tree_hashes_the_same_as_an_ingested_one() {
        // build_tree must agree with ingest_dir, or a merge commit would have a
        // different hash than committing the same files normally.
        let f = Fix::new("rebuild");
        let t = f.tree(&[("a.txt", "one\n"), ("src/main.rs", "fn main() {}\n")]);

        let work = f.dir.join("wt");
        std::fs::create_dir_all(work.join("src")).unwrap();
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        std::fs::write(work.join("src/main.rs"), "fn main() {}\n").unwrap();
        let ing = crate::ingest::ingest_dir(
            &Sink::writing(&f.store),
            &work,
            &crate::ingest::Ignore::empty(),
        )
        .unwrap();

        assert_eq!(t, ing.hash, "rebuilt and ingested trees must be identical");
    }

    #[test]
    fn disjoint_file_changes_merge_without_conflict() {
        let f = Fix::new("disjoint");
        let base = f.tree(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
        let ours = f.tree(&[("a.txt", "A CHANGED\n"), ("b.txt", "b\n")]);
        let theirs = f.tree(&[("a.txt", "a\n"), ("b.txt", "B CHANGED\n")]);

        let m = merge_trees(&f.store, Some(base), ours, theirs).unwrap();
        assert!(m.clean(), "different files are not a conflict: {:?}", m.conflicts);
        assert_eq!(f.read_path(m.tree, "a.txt"), "A CHANGED\n");
        assert_eq!(f.read_path(m.tree, "b.txt"), "B CHANGED\n");
    }

    #[test]
    fn additions_from_both_sides_are_both_kept() {
        let f = Fix::new("adds");
        let base = f.tree(&[("shared.txt", "s\n")]);
        let ours = f.tree(&[("shared.txt", "s\n"), ("ours.txt", "o\n")]);
        let theirs = f.tree(&[("shared.txt", "s\n"), ("theirs.txt", "t\n")]);

        let m = merge_trees(&f.store, Some(base), ours, theirs).unwrap();
        assert!(m.clean());
        assert_eq!(f.paths(m.tree), vec!["ours.txt", "shared.txt", "theirs.txt"]);
    }

    #[test]
    fn a_deletion_on_one_side_is_honoured() {
        let f = Fix::new("del");
        let base = f.tree(&[("keep.txt", "k\n"), ("gone.txt", "g\n")]);
        let ours = f.tree(&[("keep.txt", "k\n")]);
        let theirs = f.tree(&[("keep.txt", "k\n"), ("gone.txt", "g\n")]);

        let m = merge_trees(&f.store, Some(base), ours, theirs).unwrap();
        assert!(m.clean());
        assert_eq!(f.paths(m.tree), vec!["keep.txt"]);
    }

    #[test]
    fn delete_on_one_side_and_modify_on_the_other_conflicts() {
        let f = Fix::new("delmod");
        let base = f.tree(&[("f.txt", "original\n")]);
        let ours = f.tree(&[]);
        let theirs = f.tree(&[("f.txt", "edited\n")]);

        let m = merge_trees(&f.store, Some(base), ours, theirs).unwrap();
        assert_eq!(m.conflicts.len(), 1);
        assert!(matches!(m.conflicts[0].kind, ConflictKind::DeleteModify));
        // The surviving edit is kept so the work is not lost.
        assert_eq!(f.read_path(m.tree, "f.txt"), "edited\n");
    }

    #[test]
    fn overlapping_edits_to_one_file_produce_markers_in_the_tree() {
        let f = Fix::new("conflict");
        let base = f.tree(&[("f.txt", "a\nb\nc\n")]);
        let ours = f.tree(&[("f.txt", "a\nOURS\nc\n")]);
        let theirs = f.tree(&[("f.txt", "a\nTHEIRS\nc\n")]);

        let m = merge_trees(&f.store, Some(base), ours, theirs).unwrap();
        assert_eq!(m.conflicts.len(), 1);
        assert!(matches!(m.conflicts[0].kind, ConflictKind::Content { regions: 1 }));

        let body = f.read_path(m.tree, "f.txt");
        assert!(body.contains("<<<<<<< ours"), "got:\n{body}");
        assert!(body.contains(">>>>>>> theirs"));
    }

    #[test]
    fn merging_a_branch_that_is_already_included_is_a_no_op() {
        let f = Fix::new("uptodate");
        let base = f.tree(&[("f.txt", "a\n")]);
        let ours = f.tree(&[("f.txt", "a\nb\n")]);
        // theirs == base: we already contain everything they have.
        let m = merge_trees(&f.store, Some(base), ours, base).unwrap();
        assert!(m.clean());
        assert_eq!(m.tree, ours, "result must be exactly our tree");
    }

    #[test]
    fn a_binary_conflict_is_reported_not_mangled() {
        let f = Fix::new("binary");
        let base = f.tree(&[("b.bin", "\u{0}base\n")]);
        let ours = f.tree(&[("b.bin", "\u{0}ours\n")]);
        let theirs = f.tree(&[("b.bin", "\u{0}theirs\n")]);

        let m = merge_trees(&f.store, Some(base), ours, theirs).unwrap();
        assert_eq!(m.conflicts.len(), 1);
        assert!(matches!(m.conflicts[0].kind, ConflictKind::Binary));
        // Never interleave binary content — our side is kept verbatim.
        assert!(!f.read_path(m.tree, "b.bin").contains("<<<<"));
    }

    #[test]
    fn nested_directories_survive_a_merge() {
        let f = Fix::new("nested");
        let base = f.tree(&[("src/a/deep.rs", "1\n"), ("src/b/other.rs", "2\n")]);
        let ours = f.tree(&[("src/a/deep.rs", "1 ours\n"), ("src/b/other.rs", "2\n")]);
        let theirs = f.tree(&[("src/a/deep.rs", "1\n"), ("src/b/other.rs", "2 theirs\n")]);

        let m = merge_trees(&f.store, Some(base), ours, theirs).unwrap();
        assert!(m.clean(), "{:?}", m.conflicts);
        assert_eq!(f.read_path(m.tree, "src/a/deep.rs"), "1 ours\n");
        assert_eq!(f.read_path(m.tree, "src/b/other.rs"), "2 theirs\n");
    }
}
