//! Stamp the build with the commit it was made from.
//!
//! A self-hosted server is deployed from whatever was on the branch that day,
//! so "0.1.0" answers nothing anybody actually asks. The question is which
//! build is running, and for this program in particular the honest answer is
//! a hash -- it is the whole premise of the thing that a digest names one
//! exact state.
//!
//! Two sources, in order, because the answer has to survive being built
//! somewhere with no repository in reach:
//!
//!   1. `FKIT_COMMIT`, which is how anything published gets it. The image
//!      build carries no repository in its context at all, by design, so the
//!      workflow hands the commit in as an argument.
//!   2. Asking the checkout, as a convenience for somebody building from a
//!      clone, so a local build still names itself without being told to.
//!
//! Failing both, nothing. The server then declines to name its build rather
//! than inventing an answer for it.

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
