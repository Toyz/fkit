//! Submodules: another repository pinned into this one at an exact revision.
//!
//! The split of responsibility here is the whole design, so it is worth being
//! explicit about it.
//!
//! **The pin lives in the tree.** A [`crate::object::EntryKind::Submodule`]
//! entry names a commit, and that entry is inside the parent commit's hash. A
//! commit therefore names one complete state, submodules included, which is
//! the property the rest of fkit is built on. It also means the pin travels
//! through every mechanism that already walks the object graph: `push` sends
//! the content, `gc` keeps it, `fsck` checks it. Not one of those needed to
//! learn what a submodule is.
//!
//! **The remote lives here, next to the repository.** A URL is not content.
//! It says where bytes may be fetched from, not what they are, and two people
//! can legitimately disagree about it while working on the same commit — a
//! mirror, a proxy, a local path. Git records the URL in the tracked
//! `.gitmodules` and then needs `git submodule sync` to paper over the
//! consequences. Keeping it out of the hash removes the whole category.
//!
//! There is one wrinkle worth spelling out, because getting it wrong is what
//! makes git's version painful. Someone cloning a repository for the first
//! time has never seen its submodules and cannot be expected to know where
//! their objects live. So the project does get to *suggest* a remote, in a
//! tracked file at the repository root — ordinary content, diffable and
//! mergeable like anything else.
//!
//! The distinction git misses is between that suggestion and the URL a given
//! machine actually uses. Here the suggestion seeds the local record the first
//! time a submodule appears, and after that the local record wins and is never
//! written over. Someone behind a mirror sets it once. Nothing has to be
//! re-synchronised, because nothing ever falls out of step: there is no shared
//! value that local edits are fighting with.
//!
//! What is left in this file is deliberately small: where to fetch from, and
//! which revision is currently materialised on disk.

use crate::hash::Hash;
use crate::object::Object;
use crate::repo::Repo;
use crate::store::Store;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// One submodule, as recorded beside the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// Where it is mounted, relative to the repository root. Always uses `/`.
    pub path: String,
    /// Where its objects can be fetched from.
    pub remote: String,
    /// The revision currently materialised on disk.
    ///
    /// The tree is the authority on what *should* be there; this records what
    /// actually is, so `status` can tell the difference without hashing the
    /// whole subtree on every call.
    pub pin: Hash,
}

/// One submodule as the project declares it: where to fetch it, and at which
/// revision.
///
/// Written `wss://host/owner/repo@<hash>`. The hash makes a submodule bump a
/// legible one-line diff rather than an opaque pointer change, which is the
/// one thing git's `.gitmodules` cannot show you — there the URL sits in a
/// tracked file and the revision in a gitlink, so a review sees the move
/// without seeing what moved.
///
/// The tree remains authoritative. This hash is a declaration that travels
/// with the working tree and is rewritten whenever the pin moves, so the two
/// change in the same commit; where they somehow disagree, the tree wins and
/// `fkit submodule` says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub url: String,
    pub pin: Option<Hash>,
}

impl Suggestion {
    /// Split `url@hash`, tolerating a URL that itself contains `@`.
    ///
    /// The last `@` wins, and only if what follows is a full hash — otherwise
    /// the whole value is a URL. A userinfo `@` in a URL is therefore safe,
    /// and so is a value with no pin at all.
    pub fn parse(value: &str) -> Suggestion {
        if let Some((url, tail)) = value.rsplit_once('@')
            && let Some(pin) = Hash::from_hex(tail)
        {
            return Suggestion { url: url.to_string(), pin: Some(pin) };
        }
        Suggestion { url: value.to_string(), pin: None }
    }
}

impl std::fmt::Display for Suggestion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.pin {
            Some(p) => write!(f, "{}@{}", self.url, p),
            None => write!(f, "{}", self.url),
        }
    }
}

/// Where the project suggests each submodule's objects can be fetched from.
///
/// Tracked content, so it travels with a clone and can be reviewed in a diff.
/// It is only ever a default: see the module comment.
pub const HINTS_FILE: &str = ".fkit-submodules";

/// Resolve a suggestion against the repository it was found in.
///
/// A suggestion may be absolute (`wss://host/owner/loom`) or relative
/// (`../loom`). Relative is the better default and the reason this function
/// exists: fork a repository to another host with an absolute suggestion in it
/// and every clone of the fork quietly fetches the submodule from the original
/// server — the fork is not self-contained, and nobody finds out until that
/// server is gone or private.
///
/// `..` steps back over one segment of the parent's own path, so `../loom`
/// beside `wss://host/owner/app` means `wss://host/owner/loom`. That is git's
/// convention rather than a URL's, where `..` would act on the containing
/// directory. It is worth the inconsistency: the useful question is "which
/// repository next to mine", and this is the spelling people already know.
pub fn resolve_remote(parent_remote: &str, hint: &str) -> String {
    let Some((scheme, rest)) = parent_remote.split_once("://") else {
        return hint.to_string();
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, p),
        None => (rest, ""),
    };
    match resolve_relative(path, hint) {
        Some(p) => format!("{scheme}://{authority}/{p}"),
        None => hint.to_string(),
    }
}

/// Apply a `../`-style hint to a slash-separated path.
///
/// Separate from [`resolve_remote`] because a URL is not the only thing a
/// suggestion gets resolved against. A hub serving both repositories resolves
/// `../dep` against `owner/app` to find `owner/dep`, and can then link a pin
/// straight to the repository it names — no URL involved.
///
/// `None` for an absolute hint (there is nothing to resolve) and for one that
/// climbs past the start of the path, which is a broken suggestion rather than
/// something to quietly reinterpret.
pub fn resolve_relative(base: &str, hint: &str) -> Option<String> {
    if !(hint.starts_with("../") || hint.starts_with("./")) {
        return None;
    }
    let mut segs: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    let mut rel = hint;
    loop {
        if let Some(r) = rel.strip_prefix("../") {
            segs.pop()?;
            rel = r;
        } else if let Some(r) = rel.strip_prefix("./") {
            rel = r;
        } else {
            break;
        }
    }
    for part in rel.split('/').filter(|s| !s.is_empty()) {
        segs.push(part);
    }
    Some(segs.join("/"))
}

/// Parse the suggestions file's contents, wherever they were read from.
///
/// Split out because the hub reads it out of a tree rather than off a disk:
/// it has no working directory, only objects.
pub fn parse_hints(text: &str) -> BTreeMap<String, Suggestion> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((path, value)) = line.split_once('=') {
            out.insert(path.trim().to_string(), Suggestion::parse(value.trim()));
        }
    }
    out
}

/// Express `url` relative to `parent_remote`, when they sit on the same host.
///
/// Used when recording a suggestion, so that the common case — a submodule
/// living beside its parent — produces something a fork can carry unchanged.
pub fn relative_to(parent_remote: &str, url: &str) -> Option<String> {
    let (ps, pr) = parent_remote.split_once("://")?;
    let (us, ur) = url.split_once("://")?;
    if ps != us {
        return None;
    }
    let (pa, pp) = pr.split_once('/')?;
    let (ua, up) = ur.split_once('/')?;
    if pa != ua {
        return None;
    }

    let parent: Vec<&str> = pp.split('/').filter(|s| !s.is_empty()).collect();
    let target: Vec<&str> = up.split('/').filter(|s| !s.is_empty()).collect();
    if parent.is_empty() || target.is_empty() {
        return None;
    }
    // How much of the path the two share, ignoring each one's final segment —
    // the repository name, which is what differs by definition.
    let shared = parent[..parent.len() - 1]
        .iter()
        .zip(target[..target.len() - 1].iter())
        .take_while(|(a, b)| a == b)
        .count();
    let ups = parent.len() - shared;
    let mut out = "../".repeat(ups);
    out.push_str(&target[shared..].join("/"));
    Some(out)
}

/// The suggested remotes, by submodule path.
pub fn hints(repo: &Repo) -> BTreeMap<String, Suggestion> {
    match fs::read_to_string(repo.root.join(HINTS_FILE)) {
        Ok(text) => parse_hints(&text),
        Err(_) => BTreeMap::new(),
    }
}

/// Record, or drop, one suggestion. Rewrites the file in sorted order so that
/// two people adding different submodules do not produce a conflict over line
/// ordering.
pub fn set_hint(repo: &Repo, path: &str, what: Option<Suggestion>) -> Result<()> {
    let mut all = hints(repo);
    match what {
        Some(sug) => {
            all.insert(path.to_string(), sug);
        }
        None => {
            all.remove(path);
        }
    }
    let file = repo.root.join(HINTS_FILE);
    if all.is_empty() {
        let _ = fs::remove_file(&file);
        return Ok(());
    }
    let mut body = String::from(
        "\
# Submodules used by this project.\n\
#\n\
# One line each:\n\
#\n\
#     <path in this repository> = <url>@<revision>\n\
#\n\
# for example\n\
#\n\
#     vendor/loom = wss://example.com/alice/loom@<64 hex characters>\n\
#\n\
# The revision after the @ is the exact commit of that repository which this\n\
# one is pinned to, so a submodule bump reads as an ordinary one-line change.\n\
# The commit itself is what actually carries the pin — this line is rewritten\n\
# to match whenever it moves, and `fkit submodule` reports it if the two ever\n\
# disagree.\n\
#\n\
# The url may instead be relative to this repository's own remote, written\n\
# `../loom`, which keeps a fork on another host fetching from that host rather\n\
# than from the original. `fkit submodule set-remote <path> <url>` points one\n\
# somewhere else for this machine only, and does not touch this file.\n",
    );
    for (p, sug) in &all {
        body.push_str(&format!("{p} = {sug}\n"));
    }
    fs::write(&file, body).with_context(|| format!("writing {HINTS_FILE}"))?;
    Ok(())
}

pub fn dir(repo: &Repo) -> PathBuf {
    repo.root.join(crate::repo::META_DIR).join("submodules")
}

/// A path is stored as a filename, so the separator has to survive the trip.
///
/// `%` is escaped first, otherwise a path containing a literal `%2F` would
/// decode back into a different path than it started as.
fn encode(path: &str) -> String {
    path.replace('%', "%25").replace('/', "%2F")
}

pub fn list(repo: &Repo) -> Result<BTreeMap<String, Mount>> {
    let d = dir(repo);
    let mut out = BTreeMap::new();
    let Ok(read) = fs::read_dir(&d) else { return Ok(out) };
    for entry in read {
        let p = entry?.path();
        if !p.is_file() {
            continue;
        }
        // The filename is a convenience for finding the file; the `path` inside
        // it is what counts, so a hand-renamed file cannot silently remount a
        // submodule somewhere else.
        let text = fs::read_to_string(&p)?;
        let Some(m) = parse(&text) else { continue };
        out.insert(m.path.clone(), m);
    }
    Ok(out)
}

fn parse(text: &str) -> Option<Mount> {
    let path = crate::config::parse(text, "path")?;
    let remote = crate::config::parse(text, "remote").unwrap_or_default();
    let pin = crate::config::parse(text, "pin")?;
    Some(Mount { path, remote, pin: Hash::from_hex(&pin)? })
}

pub fn read(repo: &Repo, path: &str) -> Result<Option<Mount>> {
    let p = dir(repo).join(encode(path));
    let Ok(text) = fs::read_to_string(&p) else { return Ok(None) };
    Ok(parse(&text))
}

pub fn write(repo: &Repo, m: &Mount) -> Result<()> {
    let d = dir(repo);
    fs::create_dir_all(&d)?;
    let body =
        format!("path = {}\nremote = {}\npin = {}\n", m.path, m.remote, m.pin);
    fs::write(d.join(encode(&m.path)), body)
        .with_context(|| format!("recording submodule {}", m.path))?;
    Ok(())
}

/// Move a submodule to a different revision: files and record together.
///
/// This exists so that the two cannot be done separately. The record's whole
/// meaning is "the revision currently materialised on disk", and a caller that
/// wrote one without the other would leave that claim false — after which a
/// checkout comparing against it would skip writing the very files that needed
/// to change. Making it one operation is cheaper than defending against the
/// halfway state everywhere that reads it.
///
/// Files that the old revision had and the new one does not are removed, so
/// the result is the new revision rather than the two merged.
pub fn set_pin(repo: &Repo, path: &str, new_pin: Hash) -> Result<()> {
    let existing = read(repo, path)?;
    let dest = repo.root.join(path);

    let want = repo.view().walk_tree(repo.view().submodule_tree(new_pin, path)?)?;
    if let Some(old) = existing.as_ref().map(|m| m.pin)
        && repo.store.has(old)
        && let Ok(had) = repo.view().walk_tree(repo.view().submodule_tree(old, path)?)
    {
        for rel in had.keys() {
            if !want.contains_key(rel) {
                let _ = fs::remove_file(dest.join(rel));
            }
        }
    }
    crate::checkout::materialize(repo, repo.view().submodule_tree(new_pin, path)?, &dest)?;

    write(repo, &Mount {
        path: path.to_string(),
        remote: existing.map(|m| m.remote).unwrap_or_default(),
        pin: new_pin,
    })?;

    // Keep the declared revision in step, so that moving a pin and recording
    // that move are the same act rather than two that can be done separately.
    if let Some(sug) = hints(repo).get(path) {
        set_hint(repo, path, Some(Suggestion { url: sug.url.clone(), pin: Some(new_pin) }))?;
    }
    Ok(())
}

pub fn remove(repo: &Repo, path: &str) -> Result<()> {
    let _ = fs::remove_file(dir(repo).join(encode(path)));
    Ok(())
}

/// The pins a commit's tree declares, keyed by path.
pub fn pinned(repo: &Repo, tree: Hash) -> Result<BTreeMap<String, Hash>> {
    repo.view().submodules(tree)
}

/// Content bytes underneath a pin.
///
/// Reads two objects — the commit and its tree root — because a tree node
/// already carries the size of everything beneath it.
pub fn pinned_size(store: &Store, pin: Hash) -> Result<u64> {
    let tree = match store.get(pin) {
        Ok(Object::Commit(c)) => c.tree,
        Ok(other) => bail!("pin {pin} is a {} and not a commit", other.kind().name()),
        Err(_) => bail!("pin {pin} is not in this store"),
    };
    Ok(match store.get(tree)? {
        Object::Tree { children, .. } => children.iter().map(|c| c.size).sum(),
        Object::Entries(entries) => entries.iter().map(|e| e.size).sum(),
        other => bail!("pin {pin} names a {} where a tree belongs", other.kind().name()),
    })
}

/// Reject a mount path before it is written anywhere.
///
/// A submodule path becomes a directory name on every machine that checks the
/// repository out, so the checks that matter are the ones that stop it
/// escaping the repository root.
pub fn valid_path(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("a submodule needs a path");
    }
    if path.starts_with('/') || path.ends_with('/') {
        bail!("submodule path must be relative and must not end in a slash: {path}");
    }
    if path.starts_with(crate::repo::META_DIR) {
        bail!("submodule path must not live inside {}", crate::repo::META_DIR);
    }
    for part in path.split('/') {
        if part.is_empty() {
            bail!("submodule path has an empty component: {path}");
        }
        if part == "." || part == ".." {
            bail!("submodule path must not contain `.` or `..`: {path}");
        }
    }
    if path.contains('\\') {
        bail!("submodule path must use `/` as its separator: {path}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_survives_being_used_as_a_filename() {
        for p in ["vendor/loom", "a/b/c", "plain", "odd%2Fname", "100%"] {
            let enc = encode(p);
            assert!(!enc.contains('/'), "{p} encoded to {enc}, which is still a path");
            // Two different paths must never collide on one file.
            assert_eq!(encode(p), enc);
        }
        assert_ne!(encode("a/b"), encode("a%2Fb"));
    }

    #[test]
    fn a_path_that_escapes_the_repository_is_refused() {
        for bad in ["", "/abs", "rel/", "../up", "a/../b", "a//b", ".fkit/x", "a\\b"] {
            assert!(valid_path(bad).is_err(), "{bad:?} should not be a legal submodule path");
        }
        for good in ["vendor/loom", "deps/a/b", "one"] {
            assert!(valid_path(good).is_ok(), "{good:?} should be a legal submodule path");
        }
    }

    #[test]
    fn a_record_round_trips() {
        let m = Mount {
            path: "vendor/loom".into(),
            remote: "wss://example.test/helba/loom".into(),
            pin: Hash::from_hex(&"07".repeat(32)).unwrap(),
        };
        let text = format!("path = {}\nremote = {}\npin = {}\n", m.path, m.remote, m.pin);
        assert_eq!(parse(&text), Some(m));
    }

    #[test]
    fn a_declaration_carries_its_url_and_its_revision() {
        let pin = Hash::from_hex(&"3c".repeat(32)).unwrap();
        let text = format!("wss://example.com/alice/loom@{pin}");
        let sug = Suggestion::parse(&text);
        assert_eq!(sug.url, "wss://example.com/alice/loom");
        assert_eq!(sug.pin, Some(pin));
        assert_eq!(sug.to_string(), text, "it must write back exactly as read");
    }

    #[test]
    fn a_url_containing_an_at_is_not_mistaken_for_a_revision() {
        // Userinfo in a URL, and a trailing @ with something that is not a
        // hash. Neither may eat part of the URL.
        for u in [
            "wss://user@example.com/alice/loom",
            "wss://example.com/alice/loom@main",
            "../loom",
        ] {
            let sug = Suggestion::parse(u);
            assert_eq!(sug.url, u, "{u} should be the whole url");
            assert_eq!(sug.pin, None);
        }
    }

    #[test]
    fn a_declaration_with_userinfo_still_takes_a_revision() {
        let pin = Hash::from_hex(&"9d".repeat(32)).unwrap();
        let sug = Suggestion::parse(&format!("wss://user@example.com/alice/loom@{pin}"));
        assert_eq!(sug.url, "wss://user@example.com/alice/loom");
        assert_eq!(sug.pin, Some(pin));
    }

    #[test]
    fn a_hint_resolves_against_a_plain_path_too() {
        // What the hub does: no scheme, no host, just one repository's name
        // resolved against another's.
        assert_eq!(resolve_relative("helba/app", "../dep").as_deref(), Some("helba/dep"));
        assert_eq!(resolve_relative("helba/app", "../../o/dep").as_deref(), Some("o/dep"));
        // One step up from a single-segment base is still inside the path.
        assert_eq!(resolve_relative("app", "../dep").as_deref(), Some("dep"));
        assert_eq!(resolve_relative("app", "../../dep"), None, "climbs past the start");
        assert_eq!(resolve_relative("helba/app", "wss://h/o/dep"), None, "already absolute");
    }

    #[test]
    fn a_hint_line_is_read_back() {
        // The format has to survive a human editing it, so leading space,
        // comments and blank lines are all fine.
        let text = "# comment\n\n vendor/loom = wss://h/o/loom \nb=c\n";
        let out = parse_hints(text);
        assert_eq!(out.get("vendor/loom").map(|s| s.url.as_str()), Some("wss://h/o/loom"));
        assert_eq!(out.get("b").map(|s| s.url.as_str()), Some("c"));
    }

    #[test]
    fn a_relative_suggestion_follows_the_repository_it_travels_with() {
        // The whole point: the same suggestion resolves to whichever host the
        // parent was cloned from, so a fork is self-contained.
        assert_eq!(
            resolve_remote("wss://a.test/helba/app", "../loom"),
            "wss://a.test/helba/loom"
        );
        assert_eq!(
            resolve_remote("wss://fork.test/someone/app", "../loom"),
            "wss://fork.test/someone/loom"
        );
        assert_eq!(
            resolve_remote("wss://a.test/helba/app", "../../other/loom"),
            "wss://a.test/other/loom"
        );
        assert_eq!(resolve_remote("ws://h:7433/app", "../loom"), "ws://h:7433/loom");
        assert_eq!(resolve_remote("wss://a.test/helba/app", "./nested"),
                   "wss://a.test/helba/app/nested");
    }

    #[test]
    fn an_absolute_suggestion_is_left_exactly_as_it_is() {
        for u in ["wss://other.test/x/loom", "ws://127.0.0.1:7433/loom"] {
            assert_eq!(resolve_remote("wss://a.test/helba/app", u), u);
        }
    }

    #[test]
    fn a_suggestion_that_climbs_past_the_host_is_not_invented() {
        // Better to hand back something that visibly fails than to guess at a
        // URL the author never wrote.
        assert_eq!(resolve_remote("wss://a.test/app", "../../loom"), "../../loom");
    }

    #[test]
    fn a_sibling_is_recorded_relatively_and_a_stranger_is_not() {
        assert_eq!(
            relative_to("wss://a.test/helba/app", "wss://a.test/helba/loom").as_deref(),
            Some("../loom")
        );
        assert_eq!(
            relative_to("wss://a.test/helba/app", "wss://a.test/other/loom").as_deref(),
            Some("../../other/loom")
        );
        assert_eq!(relative_to("ws://h:7433/app", "ws://h:7433/loom").as_deref(), Some("../loom"));
        // A different host has to stay absolute; there is nothing to be relative to.
        assert_eq!(relative_to("wss://a.test/helba/app", "wss://b.test/helba/loom"), None);
    }

    #[test]
    fn what_is_recorded_relatively_resolves_back_to_where_it_came_from() {
        for (parent, url) in [
            ("wss://a.test/helba/app", "wss://a.test/helba/loom"),
            ("wss://a.test/helba/app", "wss://a.test/other/loom"),
            ("ws://h:7433/app", "ws://h:7433/loom"),
        ] {
            let rel = relative_to(parent, url).expect("should be relative");
            assert_eq!(resolve_remote(parent, &rel), url, "{rel} beside {parent}");
        }
    }

    #[test]
    fn a_record_without_a_remote_is_still_usable() {
        // A submodule fetched from a path, or one whose objects simply travelled
        // with the parent, has nowhere to point at. That is not a broken record.
        let text = format!("path = v\npin = {}\n", Hash::from_hex(&"01".repeat(32)).unwrap());
        let m = parse(&text).expect("should parse");
        assert_eq!(m.remote, "");
    }
}
