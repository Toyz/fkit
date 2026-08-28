//! Repository workflow tests, concentrated on `checkout` — where the two real
//! bugs in this project lived. Both came from inferring "the tree the working
//! directory currently reflects" from HEAD, which is wrong precisely when the
//! caller has already moved HEAD (clone and pull both do).

use fkit_core::checkout::checkout_tree;
use fkit_core::object::Object;
use fkit_core::repo::{diff_trees, CommitAs, Head, Repo};
use std::fs;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!(
            "fkit-wf-{tag}-{}-{:?}",
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

fn files_in(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            if e.file_name() == ".fkit" {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().to_string());
            }
        }
    }
    walk(root, root, &mut out);
    out.sort();
    out
}

fn tree_of(repo: &Repo, commit: fkit_core::Hash) -> fkit_core::Hash {
    match repo.store.get(commit).unwrap() {
        Object::Commit(c) => c.tree,
        _ => panic!("not a commit"),
    }
}

#[test]
fn commit_then_status_is_clean() {
    let t = Tmp::new("clean");
    let repo = Repo::init(&t.0).unwrap();
    write(&t.0, "a.txt", "hello");
    write(&t.0, "sub/b.txt", "world");
    repo.commit("first").unwrap();

    let snap = repo.snapshot().unwrap();
    let changes = diff_trees(&repo.view_with(&snap), repo.head_tree().unwrap(), Some(snap.hash)).unwrap();
    assert!(changes.is_empty(), "expected a clean tree, got {changes:?}");
}

/// `status` must not write to the object store — asking a question should not
/// mutate anything.
#[test]
fn snapshot_does_not_write_objects() {
    let t = Tmp::new("dry");
    let repo = Repo::init(&t.0).unwrap();
    write(&t.0, "a.txt", "hello");
    write(&t.0, "big.bin", &"x".repeat(200_000));

    let before = repo.store.iter_ids().unwrap().len();
    let snap = repo.snapshot().unwrap();
    let after = repo.store.iter_ids().unwrap().len();

    assert_eq!(before, after, "snapshot must not persist objects");
    assert!(snap.stats.objects_written > 0, "but it should report what a commit would write");

    // And the very same hash must come out of a real commit.
    let res = repo.commit("first").unwrap();
    assert_eq!(res.tree, snap.hash, "dry and writing runs must agree on the hash");
}

/// The clone bug: checking out into an empty directory after HEAD already
/// points at the target must still write every file.
#[test]
fn checkout_into_an_empty_directory_writes_everything() {
    let src = Tmp::new("cl-src");
    let repo = Repo::init(&src.0).unwrap();
    write(&src.0, "README.md", "readme");
    write(&src.0, "src/main.rs", "fn main() {}");
    write(&src.0, "deep/a/b/c.txt", "nested");
    let res = repo.commit("first").unwrap();

    // Simulate a fresh clone: same objects, empty working tree, HEAD already
    // advanced to the target commit.
    let dst = Tmp::new("cl-dst");
    let clone = Repo::init(&dst.0).unwrap();
    for id in repo.store.iter_ids().unwrap() {
        let raw = repo.store.get_raw(id).unwrap();
        clone.store.put_raw(id, &raw).unwrap();
    }
    clone.write_ref("main", res.commit).unwrap();
    clone.set_head(&Head::Branch("main".into())).unwrap();
    assert_eq!(files_in(&dst.0).len(), 0, "clone starts empty");

    let plan = checkout_tree(&clone, None, res.tree, true).unwrap();

    assert_eq!(plan.written, 3, "all three files must be written");
    assert_eq!(
        files_in(&dst.0),
        vec!["README.md", "deep/a/b/c.txt", "src/main.rs"]
    );
    assert_eq!(fs::read_to_string(dst.0.join("src/main.rs")).unwrap(), "fn main() {}");
}

/// The pull bug: the ref has already advanced, so the "from" tree must be
/// supplied explicitly rather than read back out of HEAD.
#[test]
fn checkout_after_the_ref_already_moved() {
    let t = Tmp::new("pull");
    let repo = Repo::init(&t.0).unwrap();
    write(&t.0, "a.txt", "v1");
    let first = repo.commit("v1").unwrap();

    // Build a second commit's tree without touching the working directory,
    // exactly as a pull would.
    write(&t.0, "a.txt", "v2");
    write(&t.0, "b.txt", "new");
    let second = repo.commit("v2").unwrap();
    checkout_tree(&repo, Some(second.tree), first.tree, true).unwrap();
    repo.write_ref("main", first.commit).unwrap();
    assert_eq!(fs::read_to_string(t.0.join("a.txt")).unwrap(), "v1");

    // Now "pull": advance the ref first, then check out from the old tree.
    repo.write_ref("main", second.commit).unwrap();
    let plan = checkout_tree(&repo, Some(first.tree), second.tree, false).unwrap();

    assert_eq!(plan.written, 2, "a.txt changed and b.txt was added");
    assert_eq!(fs::read_to_string(t.0.join("a.txt")).unwrap(), "v2");
    assert_eq!(fs::read_to_string(t.0.join("b.txt")).unwrap(), "new");
}

#[test]
fn checkout_removes_tracked_files_but_never_untracked_ones() {
    let t = Tmp::new("untracked");
    let repo = Repo::init(&t.0).unwrap();
    write(&t.0, "keep.txt", "keep");
    write(&t.0, "gone.txt", "will be deleted");
    let first = repo.commit("both").unwrap();

    fs::remove_file(t.0.join("gone.txt")).unwrap();
    let second = repo.commit("removed gone.txt").unwrap();

    // Go back to the first commit, then forward again — with an untracked file
    // sitting in the working directory the whole time.
    checkout_tree(&repo, Some(second.tree), first.tree, true).unwrap();
    assert!(t.0.join("gone.txt").exists(), "checkout should restore it");

    write(&t.0, "scratch.local", "never committed");
    checkout_tree(&repo, Some(first.tree), second.tree, true).unwrap();

    assert!(!t.0.join("gone.txt").exists(), "tracked deletion must apply");
    assert!(
        t.0.join("scratch.local").exists(),
        "an untracked file must survive checkout, even with force"
    );
}

#[test]
fn checkout_refuses_to_discard_uncommitted_work_without_force() {
    let t = Tmp::new("guard");
    let repo = Repo::init(&t.0).unwrap();
    write(&t.0, "a.txt", "committed");
    let first = repo.commit("first").unwrap();
    write(&t.0, "a.txt", "UNSAVED EDIT");

    let err = checkout_tree(&repo, Some(first.tree), first.tree, false).unwrap_err();
    assert!(err.to_string().contains("uncommitted changes"), "got: {err}");
    assert_eq!(
        fs::read_to_string(t.0.join("a.txt")).unwrap(),
        "UNSAVED EDIT",
        "the edit must still be there after a refused checkout"
    );
}

#[test]
fn branches_isolate_their_working_trees() {
    let t = Tmp::new("branch");
    let repo = Repo::init(&t.0).unwrap();
    write(&t.0, "shared.txt", "base");
    let base = repo.commit("base").unwrap();

    repo.write_ref("feature", base.commit).unwrap();
    repo.set_head(&Head::Branch("feature".into())).unwrap();
    write(&t.0, "feature-only.txt", "experimental");
    let feat = repo.commit("feature work").unwrap();

    // Back to main: the feature file must disappear.
    checkout_tree(&repo, Some(feat.tree), base.tree, false).unwrap();
    repo.set_head(&Head::Branch("main".into())).unwrap();
    assert_eq!(files_in(&t.0), vec!["shared.txt"]);

    // And return when we switch back.
    checkout_tree(&repo, Some(base.tree), feat.tree, false).unwrap();
    repo.set_head(&Head::Branch("feature".into())).unwrap();
    assert_eq!(files_in(&t.0), vec!["feature-only.txt", "shared.txt"]);
}

#[test]
fn empty_commits_are_refused() {
    let t = Tmp::new("empty");
    let repo = Repo::init(&t.0).unwrap();
    write(&t.0, "a.txt", "x");
    repo.commit("first").unwrap();
    let err = repo.commit("again").unwrap_err();
    assert!(err.to_string().contains("nothing to commit"), "got: {err}");
}

#[test]
fn executable_bit_and_symlinks_survive_a_round_trip() {
    let t = Tmp::new("modes");
    let repo = Repo::init(&t.0).unwrap();
    write(&t.0, "script.sh", "#!/bin/sh\necho hi\n");
    write(&t.0, "plain.txt", "data");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(t.0.join("script.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("plain.txt", t.0.join("link.txt")).unwrap();
    }

    let first = repo.commit("with modes").unwrap();
    write(&t.0, "plain.txt", "changed");
    let second = repo.commit("changed").unwrap();
    checkout_tree(&repo, Some(second.tree), first.tree, true).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(t.0.join("script.sh")).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "executable bit lost (mode {mode:o})");
        let meta = fs::symlink_metadata(t.0.join("link.txt")).unwrap();
        assert!(meta.file_type().is_symlink(), "symlink became a regular file");
        assert_eq!(fs::read_link(t.0.join("link.txt")).unwrap().to_str().unwrap(), "plain.txt");
    }
    let _ = tree_of(&repo, first.commit);
}

/// A commit kept alive only by a tag must survive garbage collection.
///
/// Roots came from branches alone, so tagging a release and then deleting the
/// branch left the tag pointing at objects the very next `gc` would delete.
#[test]
fn gc_keeps_a_commit_that_only_a_tag_points_at() {
    let tmp = Tmp::new("gc-tag");
    let repo = Repo::init(&tmp.0).unwrap();
    repo.config_set("author.name", "tester").unwrap();
    repo.config_set("author.email", "t@e").unwrap();

    write(&tmp.0, "a.txt", "release one\n");
    repo.commit("first").unwrap();
    let released = repo.head_commit().unwrap().unwrap();
    repo.write_tag("v1.0", released, false).unwrap();

    // Move the branch on, so nothing but the tag reaches the first commit.
    write(&tmp.0, "a.txt", "release two\n");
    repo.commit("second").unwrap();

    let roots: Vec<_> = repo.all_refs().unwrap().into_values().collect();
    let live = fkit_core::gc::reachable(&repo.store, &roots).unwrap();
    assert!(
        live.contains(&released),
        "the tagged commit must be reachable, or gc would delete the release"
    );

    // And the tag still resolves to it afterwards.
    assert_eq!(repo.read_tag("v1.0").unwrap(), Some(released));
}

/// An importer replaying a history from elsewhere has to preserve who wrote
/// each commit and when. Without that, a mirrored project becomes one author
/// committing everything in the same second, which resembles a history without
/// being one.
#[test]
fn an_import_records_another_author_and_time() {
    let t = Tmp::new("commit-as");
    let repo = Repo::init(&t.0).unwrap();
    write(&t.0, "a.txt", "one");

    let who = CommitAs {
        author: Some("Ada Lovelace <ada@example.com>".into()),
        timestamp: Some(1_234_567_890),
    };
    let res = repo.commit_as("imported", &who).unwrap();

    let Object::Commit(c) = repo.store.get(res.commit).unwrap() else {
        panic!("expected a commit");
    };
    assert_eq!(c.author, "Ada Lovelace <ada@example.com>");
    assert_eq!(c.timestamp, 1_234_567_890);
}

/// Replaying the same import twice must land on the same commit, or a mirror
/// would rewrite its own history on every run and never fast-forward.
#[test]
fn an_import_is_reproducible() {
    let a = Tmp::new("commit-as-a");
    let b = Tmp::new("commit-as-b");
    let who = CommitAs {
        author: Some("Ada Lovelace <ada@example.com>".into()),
        timestamp: Some(1_234_567_890),
    };

    let ra = Repo::init(&a.0).unwrap();
    write(&a.0, "a.txt", "one");
    let first = ra.commit_as("imported", &who).unwrap().commit;

    let rb = Repo::init(&b.0).unwrap();
    write(&b.0, "a.txt", "one");
    let second = rb.commit_as("imported", &who).unwrap().commit;

    assert_eq!(first, second, "same tree, author, time and message");
}

/// The ordinary path must be untouched: no overrides means the configured
/// author and the current clock.
#[test]
fn an_unset_override_falls_back_to_the_configured_author() {
    let t = Tmp::new("commit-as-default");
    let repo = Repo::init(&t.0).unwrap();
    write(&t.0, "a.txt", "one");

    let res = repo.commit_as("plain", &CommitAs::default()).unwrap();
    let Object::Commit(c) = repo.store.get(res.commit).unwrap() else {
        panic!("expected a commit");
    };
    assert_eq!(c.author, repo.author());
    assert!(c.timestamp > 0, "an unset date means now, not zero");
}
