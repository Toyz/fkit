//! Stamp the build with the commit it was made from.
//!
//! A self-hosted server is deployed from whatever was on the branch that day,
//! so "0.1.0" answers nothing anybody actually asks. The question is which
//! build is running, and for this program in particular the honest answer is
//! a hash -- it is the whole premise of the thing that a digest names one
//! exact state.
//!
//! Three sources, in order, because the answer has to survive being built
//! somewhere without a checkout:
//!
//!   1. `FKIT_COMMIT`, for image builds, where the workflow knows the commit
//!      and the build context often has no `.git` at all.
//!   2. `git rev-parse`, for anybody building from a clone.
//!   3. Nothing, and the footer simply omits it rather than inventing one.

use std::process::Command;

fn main() {
    // Only these should re-run it. Without this the script runs on every
    // build, and with it the stamp can go stale between commits -- which is
    // why HEAD is watched too.
    println!("cargo:rerun-if-env-changed=FKIT_COMMIT");
    for p in ["../../.git/HEAD", "../../.git/refs/heads"] {
        if std::path::Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }

    let commit = std::env::var("FKIT_COMMIT").ok().filter(|s| !s.trim().is_empty());
    let commit = commit.or_else(|| {
        let out = Command::new("git").args(["rev-parse", "--short=10", "HEAD"]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (!s.is_empty()).then_some(s)
    });

    println!("cargo:rustc-env=FKIT_COMMIT={}", commit.unwrap_or_default());
}
