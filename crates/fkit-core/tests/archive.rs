//! Archives are only correct if the tools people actually use can open them,
//! so these tests shell out to `tar` and `unzip` rather than round-tripping
//! through a reader of our own — which would agree with our own bugs.

use fkit_core::archive::{plan, write_tar, write_zip, EPOCH};
use fkit_core::repo::Repo;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!(
            "fkit-ar-{tag}-{}-{:?}",
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

fn write(root: &Path, rel: &str, content: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// A tree with the shapes that break archive writers: nested directories, an
/// empty file, a file that spans many chunks, and a deep path.
fn fixture(root: &Path) -> Repo {
    let repo = Repo::init(root).unwrap();
    repo.config_set("author.name", "t").unwrap();
    repo.config_set("author.email", "t@e").unwrap();

    write(root, "README.md", b"# hello\n");
    write(root, "empty.txt", b"");
    write(root, "src/main.rs", b"fn main() {}\n");
    write(root, "src/deep/nested/inner.txt", b"down here\n");
    // Larger than one chunk, so the writers see several store reads per file.
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
    write(root, "big.bin", &big);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        write(root, "run.sh", b"#!/bin/sh\necho hi\n");
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    repo.commit("everything").unwrap();
    repo
}

fn tree_of(repo: &Repo) -> fkit_core::hash::Hash {
    let tip = repo.head_commit().unwrap().unwrap();
    match repo.store.get(tip).unwrap() {
        fkit_core::object::Object::Commit(c) => c.tree,
        _ => panic!("not a commit"),
    }
}

#[test]
fn tar_opens_with_system_tar_and_the_contents_match() {
    let tmp = Tmp::new("tar");
    let src = tmp.0.join("src");
    fs::create_dir_all(&src).unwrap();
    let repo = fixture(&src);
    let p = plan(&repo.store, tree_of(&repo), "").unwrap();

    let path = tmp.0.join("out.tar");
    let mut f = fs::File::create(&path).unwrap();
    write_tar(&repo.store, &p, "proj", EPOCH, &mut f).unwrap();
    drop(f);

    // The size is predicted before anything is written; if that is wrong, a
    // Content-Length would be a lie.
    assert_eq!(fs::metadata(&path).unwrap().len(), p.tar_size(), "predicted tar size");

    let out = tmp.0.join("x");
    fs::create_dir_all(&out).unwrap();
    let st = Command::new("tar").arg("xf").arg(&path).arg("-C").arg(&out).status().unwrap();
    assert!(st.success(), "system tar refused the archive");

    assert_eq!(fs::read(out.join("proj/README.md")).unwrap(), b"# hello\n");
    assert_eq!(fs::read(out.join("proj/empty.txt")).unwrap(), b"");
    assert_eq!(fs::read(out.join("proj/src/deep/nested/inner.txt")).unwrap(), b"down here\n");
    assert_eq!(fs::read(out.join("proj/big.bin")).unwrap(), fs::read(src.join("big.bin")).unwrap());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(out.join("proj/run.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "the executable bit must survive");
    }
}

#[test]
fn zip_opens_with_system_unzip_and_the_contents_match() {
    let tmp = Tmp::new("zip");
    let src = tmp.0.join("src");
    fs::create_dir_all(&src).unwrap();
    let repo = fixture(&src);
    let p = plan(&repo.store, tree_of(&repo), "").unwrap();

    let path = tmp.0.join("out.zip");
    let mut f = fs::File::create(&path).unwrap();
    write_zip(&repo.store, &p, "proj", &mut f).unwrap();
    drop(f);

    // unzip -t verifies every CRC, which is the part a streaming writer is
    // most likely to get wrong.
    let t = Command::new("unzip").arg("-t").arg(&path).output().unwrap();
    assert!(
        t.status.success(),
        "unzip -t failed:\n{}",
        String::from_utf8_lossy(&t.stdout)
    );

    let out = tmp.0.join("x");
    fs::create_dir_all(&out).unwrap();
    let st = Command::new("unzip")
        .arg("-q")
        .arg(&path)
        .arg("-d")
        .arg(&out)
        .status()
        .unwrap();
    assert!(st.success(), "unzip refused the archive");

    assert_eq!(fs::read(out.join("proj/README.md")).unwrap(), b"# hello\n");
    assert_eq!(fs::read(out.join("proj/empty.txt")).unwrap(), b"");
    assert_eq!(fs::read(out.join("proj/src/main.rs")).unwrap(), b"fn main() {}\n");
    assert_eq!(fs::read(out.join("proj/big.bin")).unwrap(), fs::read(src.join("big.bin")).unwrap());
}

#[test]
fn the_same_tree_always_produces_the_same_bytes() {
    // This is what lets the archive be cached by tree hash: if the output
    // varied, an ETag built from the tree would be a lie.
    let tmp = Tmp::new("det");
    let src = tmp.0.join("src");
    fs::create_dir_all(&src).unwrap();
    let repo = fixture(&src);
    let p = plan(&repo.store, tree_of(&repo), "").unwrap();

    let mut a = Vec::new();
    let mut b = Vec::new();
    write_tar(&repo.store, &p, "proj", EPOCH, &mut a).unwrap();
    write_tar(&repo.store, &p, "proj", EPOCH, &mut b).unwrap();
    assert_eq!(a, b, "tar is not deterministic");

    let mut c = Vec::new();
    let mut d = Vec::new();
    write_zip(&repo.store, &p, "proj", &mut c).unwrap();
    write_zip(&repo.store, &p, "proj", &mut d).unwrap();
    assert_eq!(c, d, "zip is not deterministic");
}

#[test]
fn the_plan_knows_the_size_without_reading_any_content() {
    let tmp = Tmp::new("plan");
    let src = tmp.0.join("src");
    fs::create_dir_all(&src).unwrap();
    let repo = fixture(&src);
    let p = plan(&repo.store, tree_of(&repo), "").unwrap();

    let on_disk: u64 = ["README.md", "empty.txt", "src/main.rs", "src/deep/nested/inner.txt", "big.bin"]
        .iter()
        .map(|f| fs::metadata(src.join(f)).unwrap().len())
        .sum();
    #[cfg(unix)]
    let on_disk = on_disk + fs::metadata(src.join("run.sh")).unwrap().len();

    assert_eq!(p.bytes, on_disk, "planned size must match the real content");
    assert!(p.items.iter().any(|i| i.path == "src/deep/nested/inner.txt"));
}
