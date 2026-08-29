//! Materializing a tree back onto the filesystem.
//!
//! This is the one genuinely destructive operation in fkit, so it is written
//! defensively: it computes the exact set of changes first, refuses to run if
//! that would discard uncommitted work, and only then touches disk.
//!
//! Note what it does *not* do: it never deletes a file that is untracked. The
//! removal set comes from diffing HEAD's tree against the target tree, so files
//! fkit has never seen are left alone.

use crate::hash::Hash;
use crate::ingest::read_file;
use crate::object::EntryKind;
use crate::repo::Repo;
use anyhow::{bail, Result};
use std::fs;
use std::path::Path;

#[derive(Debug, Default)]
pub struct CheckoutPlan {
    pub written: usize,
    pub removed: usize,
}

impl CheckoutPlan {
    pub fn touched(&self) -> usize {
        self.written + self.removed
    }
}

/// Replace the working tree's tracked content with `target`.
///
/// `from` is the tree the working directory is *expected* to currently hold —
/// normally HEAD's tree, `None` for a fresh clone. It is passed explicitly
/// rather than read from HEAD because callers like `pull` and `clone` have
/// already advanced the ref by the time they check out; inferring it from HEAD
/// there silently produces an empty diff and checks out nothing.
///
/// Two distinct sets are computed, and the distinction matters:
///
/// * **Removals** come from `from` minus `target` — that is, *tracked* files
///   only. An untracked file is never deleted, even with `--force`.
/// * **Writes** come from comparing `target` against what is actually on disk,
///   so a file that already has the right content is not rewritten.
pub fn checkout_tree(
    repo: &Repo,
    from: Option<Hash>,
    target: Hash,
    force: bool,
) -> Result<CheckoutPlan> {
    let snap = repo.snapshot()?;

    if !force && Some(snap.hash) != from {
        bail!(
            "you have uncommitted changes\n\
             commit them, or re-run with --force to discard them"
        );
    }

    let from_files = match from {
        Some(t) => repo.walk_tree(t)?,
        None => Default::default(),
    };
    let target_files = repo.walk_tree(target)?;
    let mut disk_files = repo.view_with(&snap).walk_tree(snap.hash)?;

    // The snapshot describes a submodule's content from the *recorded* pin, not
    // from the files themselves — ingest deliberately does not descend into a
    // submodule, so it has nothing else to go on. Wherever that record
    // disagrees with the tree being checked out, it is not evidence about what
    // is on disk, and believing it would skip writing the very files that need
    // to change. Drop those paths so they are written afresh.
    let recorded = crate::submodule::list(repo)?;
    for (path, pin) in repo.view().submodules(target)? {
        if recorded.get(&path).map(|m| m.pin) != Some(pin) {
            let under = format!("{path}/");
            disk_files.retain(|p, _| !p.starts_with(&under));
        }
    }

    let mut plan = CheckoutPlan::default();

    // Deletions first, so a path can change from a directory into a file.
    for path in from_files.keys() {
        if target_files.contains_key(path) {
            continue;
        }
        let full = repo.root.join(path);
        if full.symlink_metadata().is_ok() {
            fs::remove_file(&full).ok();
            plan.removed += 1;
        }
    }

    for (path, entry) in &target_files {
        // Already correct on disk? Leave it alone.
        if let Some(d) = disk_files.get(path)
            && d.hash == entry.hash && d.kind == entry.kind {
                continue;
            }

        let full = repo.root.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }

        match entry.kind {
            EntryKind::Symlink => {
                let mut buf = Vec::new();
                read_file(&repo.store, entry.hash, &mut buf)?;
                let link_target = String::from_utf8(buf)?;
                let _ = fs::remove_file(&full);
                symlink(&link_target, &full)?;
            }
            EntryKind::File { exec } => {
                let _ = fs::remove_file(&full);
                let mut f = std::io::BufWriter::new(fs::File::create(&full)?);
                read_file(&repo.store, entry.hash, &mut f)?;
                std::io::Write::flush(&mut f)?;
                drop(f);
                set_exec(&full, exec)?;
            }
            EntryKind::Dir => unreachable!("walk_tree flattens directories away"),
            EntryKind::Submodule => {
                unreachable!("walk_tree expands submodules into their content")
            }
        }
        plan.written += 1;
    }

    prune_empty_dirs(&repo.root, &repo.root)?;
    reconcile_mounts(repo, target)?;
    Ok(plan)
}

/// Bring `.fkit/submodules/` into line with the tree that was just written.
///
/// This lives inside `checkout_tree` rather than in its callers on purpose. It
/// is the single reason `clone`, `pull`, `switch`, `checkout` and `merge` all
/// handle submodules correctly without any of them mentioning submodules: by
/// the time any of them returns, what is on disk and what is recorded agree.
///
/// Git makes this the caller's job — `--recurse-submodules`, and a separate
/// `git submodule update` when you forget — which is why a checkout there can
/// leave you with the superproject at one revision and its submodules at
/// another, with nothing saying so.
///
/// The remote is deliberately carried over from any existing record. It is not
/// part of the tree, so a checkout has nothing to say about it, and discarding
/// it here would break the next fetch.
fn reconcile_mounts(repo: &Repo, tree: Hash) -> Result<()> {
    use crate::submodule::{self, Mount};

    let want = repo.view().submodules(tree)?;
    let have = submodule::list(repo)?;
    // Read after the files are written, so a clone picks up the suggestions
    // that arrived with this very checkout.
    // Resolved against *this* clone's own remote, so a fork picks up the
    // fork's host rather than whichever one the suggestion was written beside.
    let parent_remote = repo.config_get("remote").unwrap_or_default();
    let hints = submodule::hints(repo);

    for (path, pin) in &want {
        // An existing local remote is never written over: it may be a mirror,
        // and the project's suggestion has no business overruling it.
        let remote = match have.get(path) {
            Some(m) if !m.remote.is_empty() => m.remote.clone(),
            _ => hints
                .get(path)
                .map(|sug| submodule::resolve_remote(&parent_remote, &sug.url))
                .unwrap_or_default(),
        };
        // Compare the whole record, not just the pin: a suggestion that only
        // became available with this checkout should be picked up even when
        // the revision itself did not move.
        let next = Mount { path: path.clone(), remote, pin: *pin };
        if have.get(path) != Some(&next) {
            submodule::write(repo, &next)?;
        }
    }
    for path in have.keys() {
        if !want.contains_key(path) {
            submodule::remove(repo, path)?;
        }
    }
    Ok(())
}

/// Write a tree's content into `dest`, which need not be the repository root.
///
/// Used when a submodule is first mounted, before it is part of any commit and
/// so before `checkout_tree` would know to write it.
pub fn materialize(repo: &Repo, tree: Hash, dest: &Path) -> Result<usize> {
    let files = repo.view().walk_tree(tree)?;
    let mut n = 0;
    for (path, entry) in &files {
        let full = dest.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }
        match entry.kind {
            EntryKind::Symlink => {
                let mut buf = Vec::new();
                read_file(&repo.store, entry.hash, &mut buf)?;
                let target = String::from_utf8(buf)?;
                let _ = fs::remove_file(&full);
                symlink(&target, &full)?;
            }
            EntryKind::File { exec } => {
                let _ = fs::remove_file(&full);
                let mut f = std::io::BufWriter::new(fs::File::create(&full)?);
                read_file(&repo.store, entry.hash, &mut f)?;
                std::io::Write::flush(&mut f)?;
                drop(f);
                set_exec(&full, exec)?;
            }
            EntryKind::Dir | EntryKind::Submodule => {
                unreachable!("walk_tree flattens directories and expands submodules")
            }
        }
        n += 1;
    }
    Ok(n)
}

/// Remove directories left empty by deletions. Never removes the repo root or
/// anything named `.fkit`.
fn prune_empty_dirs(root: &Path, dir: &Path) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for e in fs::read_dir(dir)? {
        let e = e?;
        if e.file_type()?.is_dir() && e.file_name() != crate::repo::META_DIR {
            prune_empty_dirs(root, &e.path())?;
        }
    }
    if dir != root && fs::read_dir(dir)?.next().is_none() {
        fs::remove_dir(dir).ok();
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &str, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(not(unix))]
fn symlink(target: &str, link: &Path) -> Result<()> {
    fs::write(link, target)?;
    Ok(())
}

#[cfg(unix)]
fn set_exec(path: &Path, exec: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if exec { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_exec(_path: &Path, _exec: bool) -> Result<()> {
    Ok(())
}
