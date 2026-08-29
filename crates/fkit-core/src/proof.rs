//! Merkle inclusion proofs.
//!
//! Because every node's hash covers its children's hashes, you can prove a file
//! belongs to a commit by handing over just the *siblings* along the path from
//! that file up to the root. The verifier recomputes each parent hash in turn
//! and checks the final value against a commit hash they already trust. They
//! never need the repository.
//!
//! ```text
//!   commit  ← the only thing the verifier has to trust
//!     └─ tree /            siblings: the other entry runs
//!         └─ entries run   siblings: the other entries in the run
//!             └─ src/      …
//!                 └─ file node
//!                     └─ chunks
//! ```
//!
//! This is the capability that content addressing actually buys, and the one
//! git cannot offer: its trees are flat, so "proving" a path means shipping
//! every sibling name in every directory along the way — and its history is not
//! structured to let you skip the rest.
//!
//! A proof carries the *encoded bytes* of each node on the path, not just
//! hashes, because those bytes are what the verifier must re-hash. They are
//! small: a run of entries or one file node, never the file contents. Proving a
//! path in a 4 GB repository costs a couple of kilobytes.

use crate::hash::Hash;
use crate::ingest::read_entries;
use crate::object::{EntryKind, Object};
use crate::store::Store;
use anyhow::{bail, Context, Result};

/// One node on the path from the commit down to the proven entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The node's id.
    pub hash: Hash,
    /// Its framed bytes (tag + encoding) — exactly what hashes to `hash`.
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    /// The commit this proof is against.
    pub root: Hash,
    pub path: String,
    /// The proven entry's own hash (a file node, or a subtree for a directory).
    pub target: Hash,
    pub size: u64,
    /// Commit first, then each node down to the one naming the target.
    pub steps: Vec<Step>,
}

impl Proof {
    pub fn byte_len(&self) -> usize {
        self.steps.iter().map(|s| s.bytes.len()).sum()
    }
}

fn step(store: &Store, h: Hash) -> Result<Step> {
    Ok(Step { hash: h, bytes: store.get_raw(h)? })
}

/// Which child of a tree node leads to `name`, walking runs and levels.
fn descend_to(store: &Store, node: Hash, name: &str, steps: &mut Vec<Step>) -> Result<Option<Hash>> {
    match store.get(node)? {
        Object::Entries(entries) => {
            steps.push(step(store, node)?);
            Ok(entries.into_iter().find(|e| e.name == name).map(|e| e.hash))
        }
        Object::Tree { children, .. } => {
            steps.push(step(store, node)?);
            // The interior node does not say which run holds a name, so each
            // candidate run is inspected. Runs are small and this is bounded by
            // the fan-out, not by the size of the directory.
            for c in children {
                if let Some(found) = descend_to(store, c.hash, name, steps)? {
                    return Ok(Some(found));
                }
                // Not in this run: drop the steps it added, they are not on the
                // path and would only bloat the proof.
                steps.truncate(steps.iter().position(|s| s.hash == node).unwrap() + 1);
            }
            Ok(None)
        }
        other => bail!("expected a tree node, found a {}", other.kind().name()),
    }
}

/// Build a proof that `path` has its current content in `commit`.
pub fn prove(store: &Store, commit: Hash, path: &str) -> Result<Proof> {
    let Object::Commit(c) = store.get(commit)? else {
        bail!("{} is not a commit", commit.short());
    };

    let mut steps = vec![step(store, commit)?];
    let mut node = c.tree;
    let mut size = 0u64;
    let mut target = None;

    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        bail!("a path is required");
    }

    for (i, part) in parts.iter().enumerate() {
        let found = descend_to(store, node, part, &mut steps)?
            .with_context(|| format!("no such path: {path}"))?;

        // Record the entry's size from the run we just walked.
        if let Some(e) = read_entries(store, node)?.into_iter().find(|e| &e.name == part) {
            size = e.size;
            if i + 1 == parts.len() {
                target = Some((found, e.kind));
            }
        }
        node = found;
    }

    let (target, kind) = target.context("path did not resolve")?;
    // A directory proves as its subtree root; a file as its file-node root.
    let _ = kind;
    let _ = EntryKind::Dir;

    Ok(Proof { root: commit, path: path.to_string(), target, size, steps })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub path: String,
    pub target: Hash,
    pub size: u64,
}

/// Check a proof against a root the verifier already trusts.
///
/// Everything is recomputed: each step's bytes are hashed and must equal the
/// hash the previous step referenced. Nothing in the proof is taken on faith,
/// which is why a proof can safely come from an untrusted source.
pub fn verify(proof: &Proof, trusted_root: Hash) -> Result<Verified> {
    if proof.root != trusted_root {
        bail!(
            "proof is against {} but you asked about {}",
            proof.root.short(),
            trusted_root.short()
        );
    }
    let Some(first) = proof.steps.first() else {
        bail!("empty proof");
    };
    if first.hash != trusted_root {
        bail!("proof does not start at the root you trust");
    }

    // Every step must hash to the id it claims.
    for s in &proof.steps {
        let actual = Hash(*blake3::hash(&s.bytes).as_bytes());
        if actual != s.hash {
            bail!("step {} does not hash to its own id", s.hash.short());
        }
    }

    // And each step must be referenced by the one before it, so the chain is
    // continuous rather than a bag of individually-valid nodes.
    let mut expected: Vec<Hash> = vec![trusted_root];
    let parts: Vec<&str> = proof.path.split('/').filter(|p| !p.is_empty()).collect();
    let mut depth = 0usize;
    let mut resolved: Option<Hash> = None;

    for (i, s) in proof.steps.iter().enumerate() {
        if !expected.contains(&s.hash) {
            bail!(
                "step {} is not referenced by anything above it — the chain is broken",
                s.hash.short()
            );
        }
        let obj = Store::decode_framed(&s.bytes)?;
        expected = match &obj {
            Object::Commit(c) => vec![c.tree],
            Object::Tree { children, .. } => children.iter().map(|c| c.hash).collect(),
            Object::Entries(entries) => {
                // A run names the next path component; that is where the chain
                // steps down a directory level.
                let want = parts.get(depth).copied().unwrap_or_default();
                let hit = entries
                    .iter()
                    .find(|e| e.name == want)
                    .with_context(|| format!("run does not contain {want:?}"))?;
                depth += 1;
                if depth == parts.len() {
                    resolved = Some(hit.hash);
                    if hit.size != proof.size {
                        bail!("proof states a size the tree does not agree with");
                    }
                }
                vec![hit.hash]
            }
            other => bail!("unexpected {} in a proof", other.kind().name()),
        };
        let _ = i;
    }

    match resolved {
        Some(h) if h == proof.target => Ok(Verified {
            path: proof.path.clone(),
            target: proof.target,
            size: proof.size,
        }),
        Some(h) => bail!(
            "path resolves to {} but the proof claims {}",
            h.short(),
            proof.target.short()
        ),
        None => bail!("proof ended before resolving {}", proof.path),
    }
}

// ---- serialisation ------------------------------------------------------

/// Encode a proof for transport. Self-describing and versioned, because a proof
/// outlives the process that produced it.
pub fn encode(p: &Proof) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fkitprf1");
    out.extend_from_slice(&p.root.0);
    out.extend_from_slice(&p.target.0);
    out.extend_from_slice(&p.size.to_le_bytes());
    out.extend_from_slice(&(p.path.len() as u32).to_le_bytes());
    out.extend_from_slice(p.path.as_bytes());
    out.extend_from_slice(&(p.steps.len() as u32).to_le_bytes());
    for s in &p.steps {
        out.extend_from_slice(&s.hash.0);
        out.extend_from_slice(&(s.bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&s.bytes);
    }
    out
}

pub fn decode(buf: &[u8]) -> Result<Proof> {
    let mut i = 0usize;
    let mut take = |n: usize| -> Result<&[u8]> {
        if i + n > buf.len() {
            bail!("truncated proof");
        }
        let s = &buf[i..i + n];
        i += n;
        Ok(s)
    };
    if take(8)? != b"fkitprf1" {
        bail!("not an fkit proof");
    }
    let root = Hash(take(32)?.try_into().unwrap());
    let target = Hash(take(32)?.try_into().unwrap());
    let size = u64::from_le_bytes(take(8)?.try_into().unwrap());
    let plen = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
    let path = String::from_utf8(take(plen)?.to_vec())?;
    let n = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;

    let mut steps = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        let hash = Hash(take(32)?.try_into().unwrap());
        let blen = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
        steps.push(Step { hash, bytes: take(blen)?.to_vec() });
    }
    Ok(Proof { root, path, target, size, steps })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{ingest_dir, Ignore};
    use crate::object::Commit;
    use crate::store::Sink;

    struct Fix {
        dir: std::path::PathBuf,
        store: Store,
        commit: Hash,
    }

    fn build(tag: &str, files: &[(&str, &str)]) -> Fix {
        let dir = std::env::temp_dir().join(format!("fkit-proof-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let work = dir.join("wt");
        for (path, body) in files {
            let full = work.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        let store = Store::open(dir.join("objects")).unwrap();
        let sink = Sink::writing(&store);
        let ing = ingest_dir(&sink, &work, &Ignore::empty(), &Default::default()).unwrap();
        let (commit, _) = store
            .put(&Object::Commit(Commit {
                tree: ing.hash,
                parents: vec![],
                author: "tester".into(),
                timestamp: 1_700_000_000,
                message: "root".into(),
            }))
            .unwrap();
        Fix { dir, store, commit }
    }
    impl Drop for Fix {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_valid_proof_verifies_against_the_root() {
        let f = build("ok", &[("a.txt", "hello\n"), ("src/lib.rs", "fn x() {}\n")]);
        let p = prove(&f.store, f.commit, "src/lib.rs").unwrap();
        let v = verify(&p, f.commit).unwrap();
        assert_eq!(v.path, "src/lib.rs");
        assert_eq!(v.size, 10);
    }

    #[test]
    fn a_proof_is_small_even_for_a_large_repository() {
        let mut files: Vec<(String, String)> = (0..2000)
            .map(|i| (format!("data/f{i:05}.txt"), format!("contents number {i}\n")))
            .collect();
        files.push(("target.txt".into(), "the one we prove\n".into()));
        let refs: Vec<(&str, &str)> =
            files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();

        let f = build("small", &refs);
        let p = prove(&f.store, f.commit, "target.txt").unwrap();
        verify(&p, f.commit).unwrap();

        // The whole point: proving one path does not cost the directory.
        assert!(
            p.byte_len() < 64 * 1024,
            "proof was {} bytes for a 2001-file repo",
            p.byte_len()
        );
    }

    #[test]
    fn a_proof_against_the_wrong_root_is_rejected() {
        let f = build("wrongroot", &[("a.txt", "hello\n")]);
        let p = prove(&f.store, f.commit, "a.txt").unwrap();
        let err = verify(&p, Hash([9u8; 32])).unwrap_err();
        assert!(err.to_string().contains("proof is against"), "got: {err}");
    }

    #[test]
    fn tampering_with_a_step_is_detected() {
        let f = build("tamper", &[("a.txt", "hello\n"), ("b.txt", "world\n")]);
        let mut p = prove(&f.store, f.commit, "a.txt").unwrap();

        // Flip a byte inside one of the node encodings.
        let last = p.steps.len() - 1;
        p.steps[last].bytes[3] ^= 0xFF;

        let err = verify(&p, f.commit).unwrap_err();
        assert!(
            err.to_string().contains("does not hash to its own id"),
            "got: {err}"
        );
    }

    #[test]
    fn a_step_from_a_different_tree_cannot_be_spliced_in() {
        let a = build("splice-a", &[("secret.txt", "real\n")]);
        let b = build("splice-b", &[("secret.txt", "forged\n")]);

        let mut p = prove(&a.store, a.commit, "secret.txt").unwrap();
        let forged = prove(&b.store, b.commit, "secret.txt").unwrap();

        // Replace the final run with a valid-but-unrelated one: each step still
        // hashes correctly, so only the chain check catches this.
        let last = p.steps.len() - 1;
        p.steps[last] = forged.steps.last().unwrap().clone();

        let err = verify(&p, a.commit).unwrap_err();
        assert!(err.to_string().contains("chain is broken"), "got: {err}");
    }

    #[test]
    fn claiming_the_wrong_target_is_rejected() {
        let f = build("target", &[("a.txt", "hello\n")]);
        let mut p = prove(&f.store, f.commit, "a.txt").unwrap();
        p.target = Hash([7u8; 32]);
        let err = verify(&p, f.commit).unwrap_err();
        assert!(err.to_string().contains("proof claims"), "got: {err}");
    }

    #[test]
    fn proofs_round_trip_through_their_encoding() {
        let f = build("codec", &[("deep/nested/file.txt", "x\n")]);
        let p = prove(&f.store, f.commit, "deep/nested/file.txt").unwrap();
        let back = decode(&encode(&p)).unwrap();
        assert_eq!(back, p);
        verify(&back, f.commit).unwrap();
    }

    #[test]
    fn a_missing_path_cannot_be_proven() {
        let f = build("missing", &[("a.txt", "hello\n")]);
        assert!(prove(&f.store, f.commit, "nope.txt").is_err());
    }
}
