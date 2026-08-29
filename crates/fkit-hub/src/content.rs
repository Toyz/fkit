//! Reading repository content out of the object store for the web UI.
//!
//! Refs live in Postgres, so this layer never touches `fkit_core::Repo` (which
//! assumes on-disk refs and a working tree). It works directly against the CAS
//! plus a commit hash handed in by the caller.

use crate::error::{AppError, AppResult};
use fkit_core::hash::Hash;
use fkit_core::ingest::{read_entries, read_file};
use fkit_core::object::{EntryKind, Object};
use fkit_core::repo::{diff_trees, View};
use fkit_core::store::Store;
use serde::Serialize;

/// Files larger than this are not sent to the browser inline.
pub const MAX_INLINE_BLOB: u64 = 2 * 1024 * 1024;

fn view(store: &Store) -> View<'_> {
    View { store, overlay: Default::default() }
}

/// Read a commit by hash.
///
/// A hash that is well-formed but absent is a 404, not a 500. Any 64 hex
/// characters parse, so `resolve_ref` hands one through whenever a URL names a
/// commit this repository does not have — a link to a branch someone deleted
/// and collected, a hash from another repository, a typo that stays valid hex.
/// That is a missing page, and reporting it as an internal error says the
/// server broke when it did not.
pub fn commit_of(store: &Store, id: Hash) -> AppResult<fkit_core::object::Commit> {
    if !store.has(id) {
        return Err(AppError::not_found(format!(
            "no commit {} in this repository",
            id.short()
        )));
    }
    match store.get(id).map_err(AppError::Internal)? {
        Object::Commit(c) => Ok(c),
        other => Err(AppError::bad(format!(
            "{} is a {}, not a commit",
            id.short(),
            other.kind().name()
        ))),
    }
}

#[derive(Debug, Serialize)]
pub struct EntryView {
    pub name: String,
    pub path: String,
    pub kind: &'static str,
    pub hash: String,
    pub size: u64,
}

/// List one directory level — not the whole tree. The browser only ever renders
/// one directory at a time, and a recursive walk of a large repository would be
/// wasted work on every page view.
pub fn list_dir(store: &Store, tree: Hash, path: &str) -> AppResult<Vec<EntryView>> {
    let target = resolve_dir(store, tree, path)?;
    let entries = read_entries(store, target).map_err(AppError::Internal)?;

    let mut out: Vec<EntryView> = entries
        .into_iter()
        .map(|e| EntryView {
            path: if path.is_empty() { e.name.clone() } else { format!("{path}/{}", e.name) },
            kind: match e.kind {
                EntryKind::Dir => "dir",
                EntryKind::Symlink => "symlink",
                EntryKind::File { exec: true } => "exec",
                EntryKind::File { exec: false } => "file",
            },
            hash: e.hash.to_hex(),
            size: e.size,
            name: e.name,
        })
        .collect();

    // Directories first, then files, each alphabetical — the ordering every
    // file browser uses, and not the same as the tree's canonical hash order.
    out.sort_by(|a, b| {
        let rank = |k: &str| if k == "dir" { 0 } else { 1 };
        rank(a.kind).cmp(&rank(b.kind)).then(a.name.cmp(&b.name))
    });
    Ok(out)
}

/// Walk down a path, one component at a time, following only directories.
pub fn resolve_dir(store: &Store, root: Hash, path: &str) -> AppResult<Hash> {
    let mut current = root;
    for part in path.split('/').filter(|p| !p.is_empty() && *p != ".") {
        if part == ".." {
            return Err(AppError::bad("path may not contain '..'"));
        }
        let entries = read_entries(store, current)
            .map_err(|_| AppError::not_found(format!("{path}: not a directory")))?;
        let hit = entries
            .into_iter()
            .find(|e| e.name == part && e.kind == EntryKind::Dir)
            .ok_or_else(|| AppError::not_found(format!("no such directory: {path}")))?;
        current = hit.hash;
    }
    Ok(current)
}

pub struct Blob {
    pub bytes: Vec<u8>,
    pub size: u64,
    pub truncated: bool,
    pub binary: bool,
    /// The image type, when the bytes actually are one. `None` for everything
    /// else — including SVG, which is a document, not a picture.
    pub image: Option<&'static str>,
    pub hash: Hash,
}

/// Identify an image from its leading bytes.
///
/// Sniffed from content, never from the file name. The name is part of what
/// somebody pushed, so trusting it would let a `.png` that is really HTML be
/// served as an image — and with `nosniff` set, a Content-Type that disagrees
/// with the bytes is exactly the mismatch that gets a real image blocked.
///
/// SVG is deliberately absent. It is an XML document that can carry script and
/// external references, so it stays `text/plain`: you can read the source,
/// which for an SVG is arguably the more useful view anyway.
pub fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.starts_with(PNG) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.starts_with(b"BM") {
        return Some("image/bmp");
    }
    // RIFF containers name their form at offset 8: WEBP is one of several.
    if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    // ISO-BMFF: an `ftyp` box whose brand says AVIF.
    if bytes.len() > 12 && &bytes[4..8] == b"ftyp" && &bytes[8..12] == b"avif" {
        return Some("image/avif");
    }
    None
}

pub fn read_blob(store: &Store, tree: Hash, path: &str) -> AppResult<Blob> {
    let (dir, file) = match path.rsplit_once('/') {
        Some((d, f)) => (d, f),
        None => ("", path),
    };
    let parent = resolve_dir(store, tree, dir)?;
    let entries = read_entries(store, parent).map_err(AppError::Internal)?;
    let entry = entries
        .into_iter()
        .find(|e| e.name == file && e.kind != EntryKind::Dir)
        .ok_or_else(|| AppError::not_found(format!("no such file: {path}")))?;

    let truncated = entry.size > MAX_INLINE_BLOB;
    let mut bytes = Vec::new();
    if !truncated {
        read_file(store, entry.hash, &mut bytes).map_err(AppError::Internal)?;
    }

    // The heuristic every diff tool uses: a NUL byte early in the file means
    // binary. Cheap, and wrong only for exotic text encodings.
    let binary = bytes.iter().take(8192).any(|b| *b == 0);
    let image = image_mime(&bytes);

    Ok(Blob { size: entry.size, hash: entry.hash, bytes, truncated, binary, image })
}

#[derive(Debug, Serialize)]
pub struct CommitView {
    pub hash: String,
    pub short: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub author: String,
    pub timestamp: i64,
    pub message: String,
    pub summary: String,
}

pub fn to_view(id: Hash, c: &fkit_core::object::Commit) -> CommitView {
    CommitView {
        hash: id.to_hex(),
        short: id.short(),
        tree: c.tree.to_hex(),
        parents: c.parents.iter().map(|p| p.to_hex()).collect(),
        author: c.author.clone(),
        timestamp: c.timestamp,
        summary: c.message.lines().next().unwrap_or_default().to_string(),
        message: c.message.clone(),
    }
}

pub fn history(store: &Store, tip: Hash, limit: usize, skip: usize) -> AppResult<Vec<CommitView>> {
    let mut out = Vec::new();
    let mut cur = Some(tip);
    let mut seen = 0usize;
    while let Some(h) = cur {
        let c = commit_of(store, h)?;
        if seen >= skip {
            out.push(to_view(h, &c));
        }
        seen += 1;
        if out.len() >= limit {
            break;
        }
        cur = c.parents.first().copied();
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
pub struct ChangeView {
    pub status: &'static str,
    pub path: String,
    pub old_size: u64,
    pub new_size: u64,
}

/// Diff a commit against its first parent. A root commit diffs against nothing,
/// which correctly renders every file as added.
pub fn commit_diff(store: &Store, id: Hash) -> AppResult<Vec<ChangeView>> {
    let c = commit_of(store, id)?;
    let parent_tree = match c.parents.first() {
        Some(p) => Some(commit_of(store, *p)?.tree),
        None => None,
    };

    let v = view(store);
    let changes = diff_trees(&v, parent_tree, Some(c.tree)).map_err(AppError::Internal)?;

    Ok(changes
        .into_iter()
        .map(|ch| {
            use fkit_core::repo::Change::*;
            match ch {
                Added { path, size } => ChangeView { status: "added", path, old_size: 0, new_size: size },
                Removed { path, size } => ChangeView { status: "removed", path, old_size: size, new_size: 0 },
                Modified { path, old_size, new_size, .. } => {
                    ChangeView { status: "modified", path, old_size, new_size }
                }
                TypeChanged { path } => ChangeView { status: "typechanged", path, old_size: 0, new_size: 0 },
            }
        })
        .collect())
}

/// Files diffed in a single commit view before we stop and say so.
pub const MAX_DIFF_FILES: usize = 60;
/// Largest file either side may be for a line diff to be attempted.
pub const MAX_DIFF_BYTES: u64 = 512 * 1024;

#[derive(Debug, Serialize)]
pub struct DiffLineView {
    /// " ", "-" or "+", so the client does not have to map an enum.
    pub op: &'static str,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct HunkView {
    pub header: String,
    pub lines: Vec<DiffLineView>,
}

#[derive(Debug, Serialize)]
pub struct FileDiff {
    pub path: String,
    pub status: &'static str,
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
    pub truncated: bool,
    /// Skipped because the file is larger than [`MAX_DIFF_BYTES`].
    pub too_large: bool,
    pub only_line_endings: bool,
    pub hunks: Vec<HunkView>,
    /// Language hint for the client's highlighter.
    pub old_size: u64,
    pub new_size: u64,
    /// The two sides' content hashes, absent where the file did not exist.
    ///
    /// A line comment anchors to one of these rather than to a commit. The
    /// diff is recomputed live from two branches, so a comment pinned to a
    /// commit slides onto an unrelated line as soon as anyone pushes; pinned
    /// to the content it was written against, it stays put for as long as
    /// that content does, and is plainly absent once it does not.
    pub old_hash: Option<String>,
    pub new_hash: Option<String>,
}

fn read_blob_by_hash(store: &Store, h: Hash) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = read_file(store, h, &mut buf);
    buf
}

/// Full line-level patch for a commit, against its first parent.
///
/// Bounded twice over: a commit touching thousands of files renders the first
/// [`MAX_DIFF_FILES`], and an individual file past [`MAX_DIFF_BYTES`] is listed
/// but not diffed. Both limits are reported to the caller rather than silently
/// trimming, because a diff that quietly omits changes is worse than one that
/// says it did.
pub fn commit_patch(store: &Store, id: Hash) -> AppResult<(Vec<FileDiff>, bool)> {
    let c = commit_of(store, id)?;
    let parent_tree = match c.parents.first() {
        Some(p) => Some(commit_of(store, *p)?.tree),
        None => None,
    };

    let v = view(store);
    let changes = diff_trees(&v, parent_tree, Some(c.tree)).map_err(AppError::Internal)?;
    let more = changes.len() > MAX_DIFF_FILES;

    diff_file_list(store, &v, parent_tree, c.tree, changes).map(|f| (f, more))
}

fn diff_file_list(
    store: &Store,
    v: &fkit_core::repo::View<'_>,
    old_tree: Option<Hash>,
    new_tree: Hash,
    changes: Vec<fkit_core::repo::Change>,
) -> AppResult<Vec<FileDiff>> {
    let old_files = match old_tree {
        Some(t) => v.walk_tree(t).map_err(AppError::Internal)?,
        None => Default::default(),
    };
    let new_files = v.walk_tree(new_tree).map_err(AppError::Internal)?;

    let mut out = Vec::new();
    for ch in changes.into_iter().take(MAX_DIFF_FILES) {
        use fkit_core::repo::Change::*;
        let (path, status) = match &ch {
            Added { path, .. } => (path.clone(), "added"),
            Removed { path, .. } => (path.clone(), "removed"),
            Modified { path, .. } => (path.clone(), "modified"),
            TypeChanged { path } => (path.clone(), "typechanged"),
        };

        let before = old_files.get(&path);
        let after = new_files.get(&path);
        let old_size = before.map(|e| e.size).unwrap_or(0);
        let new_size = after.map(|e| e.size).unwrap_or(0);

        if old_size.max(new_size) > MAX_DIFF_BYTES {
            out.push(FileDiff {
                path, status, added: 0, removed: 0, binary: false, truncated: false,
                too_large: true, only_line_endings: false, hunks: vec![], old_size, new_size,
                old_hash: before.map(|e| e.hash.to_hex()),
                new_hash: after.map(|e| e.hash.to_hex()),
            });
            continue;
        }

        let a = before.map(|e| read_blob_by_hash(store, e.hash)).unwrap_or_default();
        let b = after.map(|e| read_blob_by_hash(store, e.hash)).unwrap_or_default();
        let d = fkit_core::diff::diff(&a, &b);

        out.push(FileDiff {
            path,
            status,
            added: d.added,
            removed: d.removed,
            binary: d.binary,
            truncated: d.truncated,
            too_large: false,
            only_line_endings: d.only_line_endings,
            old_size,
            new_size,
            old_hash: before.map(|e| e.hash.to_hex()),
            new_hash: after.map(|e| e.hash.to_hex()),
            hunks: d
                .hunks
                .into_iter()
                .map(|h| HunkView {
                    header: h.header(),
                    lines: h
                        .lines
                        .into_iter()
                        .map(|l| DiffLineView {
                            op: match l.op {
                                fkit_core::diff::Op::Equal => " ",
                                fkit_core::diff::Op::Delete => "-",
                                fkit_core::diff::Op::Insert => "+",
                            },
                            old_no: l.old_no,
                            new_no: l.new_no,
                            text: l.text,
                        })
                        .collect(),
                })
                .collect(),
        });
    }

    Ok(out)
}

#[derive(Debug, Serialize)]
pub struct ConflictView {
    pub path: String,
    pub kind: &'static str,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct Comparison {
    pub base: String,
    pub head: String,
    pub merge_base: Option<String>,
    pub merge_base_short: Option<String>,
    /// Commits on head that base does not have, newest first.
    pub commits: Vec<CommitView>,
    pub ahead: usize,
    pub behind: usize,
    /// True when head is already fully contained in base.
    pub up_to_date: bool,
    /// True when base is an ancestor of head — the merge is a pointer move.
    pub fast_forward: bool,
    pub mergeable: bool,
    pub conflicts: Vec<ConflictView>,
    pub files: Vec<FileDiff>,
    pub files_truncated: bool,
}

/// How many commits to list on a comparison before saying "and more".
const MAX_COMPARE_COMMITS: usize = 250;

/// Count commits reachable from `from` but not from `exclude`.
fn count_ahead(store: &Store, from: Hash, exclude: Hash, cap: usize) -> (Vec<CommitView>, usize) {
    use std::collections::{HashSet, VecDeque};

    let mut excluded = HashSet::new();
    let mut q = VecDeque::from([exclude]);
    while let Some(h) = q.pop_front() {
        if !excluded.insert(h) {
            continue;
        }
        if let Ok(Object::Commit(c)) = store.get(h) {
            q.extend(c.parents);
        }
    }

    let mut out = Vec::new();
    let mut total = 0usize;
    let mut seen = HashSet::new();
    let mut q = VecDeque::from([from]);
    while let Some(h) = q.pop_front() {
        if excluded.contains(&h) || !seen.insert(h) {
            continue;
        }
        if let Ok(Object::Commit(c)) = store.get(h) {
            total += 1;
            if out.len() < cap {
                out.push(to_view(h, &c));
            }
            q.extend(c.parents);
        }
    }
    // Newest first.
    out.sort_by_key(|c| std::cmp::Reverse(c.timestamp));
    (out, total)
}

/// Everything the compare / merge view needs about two refs.
///
/// Deliberately mirrors what a merge would actually do: the diff shown is from
/// the merge base to head — the changes head *introduces* — not a raw base-to-head
/// diff, which would also show everything base did independently and read as
/// though this merge were reverting them.
pub fn compare(
    store: &Store,
    base_name: &str,
    base: Hash,
    head_name: &str,
    head: Hash,
) -> AppResult<Comparison> {
    use fkit_core::merge::{merge_base as find_base, merge_trees, ConflictKind};

    let mb = find_base(store, base, head).map_err(AppError::Internal)?;
    let (commits, ahead) = match mb.base {
        Some(b) => count_ahead(store, head, b, MAX_COMPARE_COMMITS),
        None => count_ahead(store, head, head, 0),
    };
    let (_, behind) = match mb.base {
        Some(b) => count_ahead(store, base, b, 0),
        None => (vec![], 0),
    };

    let up_to_date = mb.base == Some(head);
    let fast_forward = mb.base == Some(base) && !up_to_date;

    let base_tree = match mb.base {
        Some(b) => Some(commit_of(store, b)?.tree),
        None => None,
    };
    let head_tree = commit_of(store, head)?.tree;

    // Line-level diff of what head introduces.
    let v = view(store);
    let changes = diff_trees(&v, base_tree, Some(head_tree)).map_err(AppError::Internal)?;
    let files_truncated = changes.len() > MAX_DIFF_FILES;
    let files = diff_file_list(store, &v, base_tree, head_tree, changes)?;

    // Would the merge apply cleanly?
    let mut conflicts = Vec::new();
    let mut mergeable = true;
    if !up_to_date && !fast_forward {
        let outcome = merge_trees(store, base_tree, commit_of(store, base)?.tree, head_tree)
            .map_err(AppError::Internal)?;
        mergeable = outcome.clean();
        conflicts = outcome
            .conflicts
            .into_iter()
            .map(|c| {
                let (kind, detail) = match c.kind {
                    ConflictKind::Content { regions } => {
                        ("content", format!("{regions} overlapping region(s)"))
                    }
                    ConflictKind::Binary => ("binary", "binary file changed on both sides".into()),
                    ConflictKind::DeleteModify => (
                        "delete-modify",
                        "deleted on one side, modified on the other".into(),
                    ),
                    ConflictKind::TypeChange => {
                        ("type-change", "became a different kind of entry".into())
                    }
                };
                ConflictView { path: c.path, kind, detail }
            })
            .collect();
    }

    Ok(Comparison {
        base: base_name.to_string(),
        head: head_name.to_string(),
        merge_base: mb.base.map(|h| h.to_hex()),
        merge_base_short: mb.base.map(|h| h.short()),
        ahead,
        behind,
        up_to_date,
        fast_forward,
        mergeable,
        conflicts,
        commits,
        files,
        files_truncated,
    })
}

/// Resolve one directory entry by path, if it exists.
fn entry_at(store: &Store, tree: Hash, path: &str) -> Option<fkit_core::object::TreeEntry> {
    let (dir, name) = match path.rsplit_once('/') {
        Some((d, f)) => (d, f),
        None => ("", path),
    };
    let parent = resolve_dir(store, tree, dir).ok()?;
    read_entries(store, parent).ok()?.into_iter().find(|e| e.name == name)
}

/// The tree hash of a directory inside a commit's tree, if the directory exists.
fn dir_hash(store: &Store, tree: Hash, path: &str) -> Option<Hash> {
    resolve_dir(store, tree, path).ok()
}

#[derive(Debug, Serialize)]
pub struct LastCommit {
    pub hash: String,
    pub short: String,
    pub summary: String,
    pub author: String,
    pub timestamp: i64,
}

/// For each entry in `path`, the most recent commit that changed it.
///
/// This is the column a file browser shows next to every name, and computing it
/// naively means diffing whole trees once per commit. The Merkle DAG makes it
/// cheap instead: if a directory's subtree hash is unchanged between a commit
/// and its parent, *nothing* inside it changed and the entire commit is skipped
/// with one comparison. Only commits that actually touched this directory cost
/// anything, and each entry is resolved the first time its hash differs.
///
/// `max_commits` bounds the walk so a pathological history cannot hang a page
/// view; entries still unresolved are simply omitted.
pub fn last_commits(
    store: &Store,
    tip: Hash,
    path: &str,
    max_commits: usize,
) -> AppResult<std::collections::HashMap<String, LastCommit>> {
    use std::collections::{HashMap, HashSet};

    let mut out: HashMap<String, LastCommit> = HashMap::new();

    let tip_commit = commit_of(store, tip)?;
    let Some(dir) = dir_hash(store, tip_commit.tree, path) else {
        return Ok(out);
    };
    let names: HashSet<String> = read_entries(store, dir)
        .map_err(AppError::Internal)?
        .into_iter()
        .map(|e| e.name)
        .collect();

    let mut pending: HashSet<String> = names;
    let mut cur = Some((tip, tip_commit));
    let mut seen = 0usize;

    while let Some((id, commit)) = cur {
        if pending.is_empty() || seen >= max_commits {
            break;
        }
        seen += 1;

        let this_dir = dir_hash(store, commit.tree, path);
        let parent = commit.parents.first().copied();
        let parent_commit = match parent {
            Some(p) => Some(commit_of(store, p)?),
            None => None,
        };
        let parent_dir = parent_commit
            .as_ref()
            .and_then(|c| dir_hash(store, c.tree, path));

        // One hash comparison rules out an entire commit.
        if this_dir != parent_dir {
            let here = match this_dir {
                Some(h) => read_entries(store, h).unwrap_or_default(),
                None => vec![],
            };
            let there = match parent_dir {
                Some(h) => read_entries(store, h).unwrap_or_default(),
                None => vec![],
            };

            let mut changed: Vec<String> = Vec::new();
            for e in &here {
                if !pending.contains(&e.name) {
                    continue;
                }
                let before = there.iter().find(|o| o.name == e.name);
                if before.map(|o| o.hash) != Some(e.hash) {
                    changed.push(e.name.clone());
                }
            }
            for name in changed {
                pending.remove(&name);
                out.insert(
                    name,
                    LastCommit {
                        hash: id.to_hex(),
                        short: id.short(),
                        summary: commit.message.lines().next().unwrap_or_default().to_string(),
                        author: commit.author.clone(),
                        timestamp: commit.timestamp,
                    },
                );
            }
        }

        cur = match (parent, parent_commit) {
            (Some(p), Some(c)) => Some((p, c)),
            _ => None,
        };
    }

    Ok(out)
}

/// Largest file the raw endpoint will materialise.
///
/// The response is built in memory, so without a cap a request for a
/// gigabyte-scale disk image would allocate a gigabyte of server memory — and a
/// handful of concurrent ones would take the process down. Streaming the body
/// would lift this; until then the limit is explicit rather than implicit.
pub const MAX_RAW_BYTES: u64 = 64 * 1024 * 1024;

/// Raw bytes of a file, for download and "view raw".
pub fn raw_blob(store: &Store, tree: Hash, path: &str) -> AppResult<(Vec<u8>, u64)> {
    let entry = entry_at(store, tree, path)
        .ok_or_else(|| AppError::not_found(format!("no such file: {path}")))?;
    if entry.kind == EntryKind::Dir {
        return Err(AppError::bad("that is a directory"));
    }
    if entry.size > MAX_RAW_BYTES {
        return Err(AppError::bad(format!(
            "file is {} bytes; the raw endpoint serves at most {}",
            entry.size, MAX_RAW_BYTES
        )));
    }
    let mut bytes = Vec::new();
    read_file(store, entry.hash, &mut bytes).map_err(AppError::Internal)?;
    Ok((bytes, entry.size))
}

/// Find a README at the top of a tree, for the repo landing page.
pub fn find_readme(store: &Store, tree: Hash) -> Option<(String, String)> {
    let entries = read_entries(store, tree).ok()?;
    let hit = entries.iter().find(|e| {
        matches!(e.kind, EntryKind::File { .. })
            && e.name.to_ascii_lowercase().starts_with("readme")
            && e.size <= MAX_INLINE_BLOB
    })?;
    let mut buf = Vec::new();
    read_file(store, hit.hash, &mut buf).ok()?;
    Some((hit.name.clone(), String::from_utf8(buf).ok()?))
}

#[cfg(test)]
mod image_tests {
    use super::image_mime;

    #[test]
    fn images_are_identified_from_their_bytes() {
        assert_eq!(image_mime(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]), Some("image/png"));
        assert_eq!(image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(image_mime(b"GIF89a....."), Some("image/gif"));
        assert_eq!(image_mime(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_mime(b"\0\0\0\x20ftypavif\0"), Some("image/avif"));
    }

    #[test]
    fn an_svg_is_not_served_as_an_image() {
        // It is XML that can carry script and fetch external resources. Served
        // as image/svg+xml from this origin it would run with the viewer's
        // session; as text/plain it is just readable source.
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><script>1</script></svg>"#;
        assert_eq!(image_mime(svg), None);
    }

    #[test]
    fn a_name_cannot_make_something_an_image() {
        // Only the bytes decide. HTML pushed as "logo.png" stays inert.
        assert_eq!(image_mime(b"<!doctype html><script>alert(1)</script>"), None);
        assert_eq!(image_mime(b""), None);
        assert_eq!(image_mime(b"RIFF\0\0\0\0WAVE"), None, "not every RIFF is a WEBP");
    }
}
