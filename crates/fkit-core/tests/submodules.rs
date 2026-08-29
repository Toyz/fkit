//! Submodules, tested against the failures that make git's version painful.
//!
//! Each test below names a specific thing that goes wrong elsewhere. They are
//! written against two repositories sharing one object store, because that is
//! the arrangement the design depends on: the submodule's content is the
//! parent's content, so nothing has to be fetched at checkout time and there
//! is no second repository to fall out of step with.

use fkit_core::checkout::checkout_tree;
use fkit_core::object::{EntryKind, Object};
use fkit_core::repo::Repo;
use fkit_core::submodule::{self, Mount};
use fkit_core::Hash;
use std::fs;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!(
            "fkit-sub-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn tree_of(repo: &Repo, commit: Hash) -> Hash {
    match repo.store.get(commit).unwrap() {
        Object::Commit(c) => c.tree,
        _ => panic!("not a commit"),
    }
}

/// A parent with one submodule already mounted and committed.
struct World {
    _tmp: Tmp,
    parent: Repo,
    /// Two revisions of the submodule, oldest first.
    v1: Hash,
    v2: Hash,
}

fn build(tag: &str) -> World {
    let tmp = Tmp::new(tag);
    let dep_dir = tmp.0.join("dep");
    fs::create_dir_all(&dep_dir).unwrap();
    let dep = Repo::init(&dep_dir).unwrap();
    write(&dep_dir, "lib.js", "v1\n");
    write(&dep_dir, "README.md", "dep\n");
    let v1 = dep.commit("dep v1").unwrap().commit;
    write(&dep_dir, "lib.js", "v2\n");
    write(&dep_dir, "extra.js", "added in v2\n");
    let v2 = dep.commit("dep v2").unwrap().commit;

    let parent_dir = tmp.0.join("app");
    fs::create_dir_all(&parent_dir).unwrap();
    let parent = Repo::init(&parent_dir).unwrap();
    write(&parent_dir, "main.js", "app\n");

    // Copy the dependency's objects into the parent's store, which is what a
    // fetch would have done. After this there is one store and no second
    // repository.
    for id in dep.store.iter_ids().unwrap() {
        let framed = dep.store.get_raw(id).unwrap();
        parent.store.put_raw(id, &framed).unwrap();
    }

    submodule::write(&parent, &Mount {
        path: "vendor/dep".into(),
        remote: String::new(),
        pin: v1,
    })
    .unwrap();
    fkit_core::checkout::materialize(&parent, tree_of(&parent, v1), &parent_dir.join("vendor/dep"))
        .unwrap();

    World { _tmp: tmp, parent, v1, v2 }
}

/// The submodule's files must be recorded as a pin, not ingested a second time
/// under the parent's own paths. If they were, the reference would have become
/// a fork the moment it was committed.
#[test]
fn a_submodule_is_committed_as_a_pin_and_not_as_a_copy() {
    let w = build("pin");
    // Everything the pin reaches, before the parent commits anything.
    let closure = fkit_core::gc::reachable(&w.parent.store, &[w.v1]).unwrap();
    let before: std::collections::HashSet<Hash> =
        w.parent.store.iter_ids().unwrap().into_iter().collect();
    let res = w.parent.commit("vendor dep").unwrap();
    let after: std::collections::HashSet<Hash> =
        w.parent.store.iter_ids().unwrap().into_iter().collect();

    let entries = w.parent.view().read_entries(tree_of(&w.parent, res.commit)).unwrap();
    let vendor = entries.iter().find(|e| e.name == "vendor").expect("vendor directory");
    let inner = w.parent.view().read_entries(vendor.hash).unwrap();
    let dep = inner.iter().find(|e| e.name == "dep").expect("dep entry");

    assert_eq!(dep.kind, EntryKind::Submodule, "the entry must be a pin");
    assert_eq!(dep.hash, w.v1, "the pin must name the submodule's commit");

    // The claim is not that the commit was free — the parent has its own tree
    // to write — but that not one byte of the submodule was stored a second
    // time. Content addressing makes that checkable rather than a matter of
    // trust: a duplicate would have to be a new object inside the pin's own
    // closure, and there are none.
    let written_from_closure: Vec<_> =
        after.difference(&before).filter(|h| closure.contains(h)).collect();
    assert!(
        written_from_closure.is_empty(),
        "committing a pin re-stored {} of the submodule's own objects",
        written_from_closure.len()
    );
}

/// The pin is inside the parent's commit hash, so two different pins have to
/// give two different commits. This is what makes a commit name a complete
/// state, and it is exactly what git's out-of-tree configuration cannot do.
#[test]
fn moving_the_pin_changes_the_parent_commit() {
    let w = build("hash");
    let first = w.parent.commit("at v1").unwrap();

    submodule::set_pin(&w.parent, "vendor/dep", w.v2).unwrap();
    let second = w.parent.commit("at v2").unwrap();

    assert_ne!(first.tree, second.tree, "a different pin must give a different tree");
}

/// Git's worst submodule behaviour: `git checkout` of the superproject leaves
/// submodules at whatever revision they happened to be at, silently, unless
/// you remember `--recurse-submodules`. Here one checkout moves everything.
#[test]
fn checking_out_an_older_commit_moves_the_submodule_back() {
    let w = build("atomic");
    let at_v1 = w.parent.commit("at v1").unwrap();

    submodule::set_pin(&w.parent, "vendor/dep", w.v2).unwrap();
    let at_v2 = w.parent.commit("at v2").unwrap();
    checkout_tree(&w.parent, Some(at_v1.tree), at_v2.tree, true).unwrap();

    let dep = w.parent.root.join("vendor/dep");
    assert_eq!(fs::read_to_string(dep.join("lib.js")).unwrap(), "v2\n");
    assert!(dep.join("extra.js").exists(), "v2 adds a file");

    // Back to the older commit, with no flag and no second command.
    checkout_tree(&w.parent, Some(at_v2.tree), at_v1.tree, false).unwrap();

    assert_eq!(
        fs::read_to_string(dep.join("lib.js")).unwrap(),
        "v1\n",
        "the submodule's content must follow the commit"
    );
    assert!(
        !dep.join("extra.js").exists(),
        "a file that only exists in v2 must be gone again"
    );
    assert_eq!(
        submodule::read(&w.parent, "vendor/dep").unwrap().unwrap().pin,
        w.v1,
        "the record on disk must agree with the tree that was checked out"
    );
}

/// A checkout that adds or drops a submodule has to maintain the mount table,
/// or the next commit would re-pin whatever was there before. A fresh clone is
/// the same code path, which is why cloning needs no `--recursive`.
#[test]
fn a_checkout_that_introduces_a_submodule_records_it() {
    let w = build("reconcile");
    let with = w.parent.commit("with dep").unwrap();

    // A tree that predates the submodule.
    submodule::remove(&w.parent, "vendor/dep").unwrap();
    let _ = fs::remove_dir_all(w.parent.root.join("vendor"));
    let without = w.parent.commit("without dep").unwrap();

    checkout_tree(&w.parent, Some(without.tree), with.tree, true).unwrap();
    assert_eq!(
        submodule::read(&w.parent, "vendor/dep").unwrap().map(|m| m.pin),
        Some(w.v1),
        "checking out a tree that has a submodule must record the mount"
    );

    checkout_tree(&w.parent, Some(with.tree), without.tree, true).unwrap();
    assert!(
        submodule::read(&w.parent, "vendor/dep").unwrap().is_none(),
        "checking out a tree without the submodule must drop the mount"
    );
}

/// A pin is an ordinary link in the object graph, so the submodule's content is
/// reachable and `gc` must not touch it. Nothing taught `gc` about submodules;
/// this asserts that not teaching it was actually correct.
#[test]
fn gc_keeps_what_a_pin_points_at() {
    let w = build("gc");
    let res = w.parent.commit("vendor dep").unwrap();

    // v2 is in this store but nothing pins it, so it is genuinely garbage and
    // gc is right to say so. What must survive is everything v1 reaches.
    let pinned = fkit_core::gc::reachable(&w.parent.store, &[w.v1]).unwrap();

    fkit_core::gc::collect(
        &w.parent.store,
        &[res.commit],
        fkit_core::gc::Options { dry_run: false, min_age: std::time::Duration::ZERO },
    )
    .unwrap();

    for h in &pinned {
        assert!(w.parent.store.has(*h), "gc removed {h}, which the pin reaches");
    }
    // The proof that matters: it still checks out afterwards.
    let files = w.parent.view().walk_tree(res.tree).unwrap();
    assert!(files.contains_key("vendor/dep/lib.js"), "submodule content survived gc");
    for entry in files.values() {
        assert!(w.parent.store.has(entry.hash), "every object is still present");
    }
}

/// Committing a pin whose objects are absent would produce a commit nobody —
/// including its author — could check out. Refuse it at the point it is made,
/// rather than at the point someone else discovers it.
#[test]
fn a_pin_the_store_cannot_resolve_is_refused() {
    let w = build("missing");
    let absent = Hash::from_hex(&"ab".repeat(32)).unwrap();
    submodule::write(&w.parent, &Mount {
        path: "vendor/dep".into(),
        remote: String::new(),
        pin: absent,
    })
    .unwrap();

    let err = w.parent.commit("pin into the void").unwrap_err().to_string();
    assert!(
        err.contains("vendor/dep") && err.contains("not in this store"),
        "the error should name the submodule and the reason, got: {err}"
    );
}

/// `walk_tree` expands a pin so that callers see content, while `submodules`
/// reports the boundary. Both views are needed and they must agree.
#[test]
fn a_tree_reads_as_content_or_as_a_boundary() {
    let w = build("views");
    let res = w.parent.commit("vendor dep").unwrap();
    let view = w.parent.view();

    let files = view.walk_tree(res.tree).unwrap();
    assert!(files.contains_key("vendor/dep/lib.js"), "expanded view sees through the pin");
    assert!(files.contains_key("main.js"));
    assert!(!files.contains_key("vendor/dep"), "the pin itself is not a file");

    let subs = view.submodules(res.tree).unwrap();
    assert_eq!(subs.get("vendor/dep"), Some(&w.v1), "boundary view sees the pin");
    assert_eq!(subs.len(), 1);
}
