//! `fkit-server` as a library.
//!
//! The standalone daemon and any embedding application share these pieces. The
//! protocol conversation itself lives in [`fkit_core::session`]; what is here is
//! the *disk-backed* host — refs as files, one shared token, no user model.
//!
//! For per-user auth, per-repo roles, and transactional refs, see `fkit-hub`,
//! which implements the same [`RepoHost`] trait against Postgres and serves the
//! identical protocol on the same port as its web UI.

use anyhow::{bail, Context, Result};
use fkit_core::hash::Hash;
use fkit_core::proto::{is_ancestor, TransferStats};
use fkit_core::repo::Repo;
use fkit_core::session::{RefUpdate, RepoHost};
use fkit_core::store::Store;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// One lock per repository name, guarding ref updates.
///
/// This makes the check-and-set atomic *within one process*. It is the honest
/// limit of a file-backed server: two `fkitd` processes over the same data
/// directory can still race. `fkit-hub` does not have this problem because the
/// check and the write share a database transaction.
pub fn ref_lock(repo: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = map.lock().unwrap();
    map.entry(repo.to_string()).or_default().clone()
}

/// A repository whose refs are files on disk.
pub struct DiskHost {
    pub repo: Repo,
    pub name: String,
    pub peer: String,
    pub writable: bool,
}

impl RepoHost for DiskHost {
    fn store(&self) -> &Store {
        &self.repo.store
    }

    fn refs(&self) -> Result<Vec<(String, Hash)>> {
        Ok(self.repo.list_refs()?.into_iter().collect())
    }

    fn read_ref(&self, branch: &str) -> Result<Option<Hash>> {
        self.repo.read_ref(branch)
    }

    fn can_write(&self) -> bool {
        self.writable
    }

    fn advance_ref(&self, branch: &str, tip: Hash, force: bool) -> Result<RefUpdate> {
        let lock = ref_lock(&self.name);
        let _guard = lock.lock().unwrap();

        if let Some(old) = self.repo.read_ref(branch)? {
            if old == tip {
                return Ok(RefUpdate::AlreadyCurrent);
            }
            if !force && !is_ancestor(&self.repo.store, old, tip)? {
                return Ok(RefUpdate::NotFastForward);
            }
        }
        self.repo.write_ref(branch, tip)?;
        Ok(RefUpdate::Updated)
    }

    fn on_push(&self, branch: &str, tip: Hash, stats: &TransferStats) {
        println!(
            "[{}] push '{}/{branch}' -> {} ({} objects, {} bytes)",
            self.peer, self.name, tip.short(), stats.objects, stats.bytes
        );
    }

    fn on_pull(&self, branch: &str, stats: &TransferStats) {
        println!(
            "[{}] pull '{}/{branch}' ({} objects, {} bytes)",
            self.peer, self.name, stats.objects, stats.bytes
        );
    }
}

/// Repository and branch names become path components, so they must not be able
/// to escape the data directory.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("empty name");
    }
    if name.len() > 128 {
        bail!("name too long");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("name '{name}' contains characters other than [A-Za-z0-9._-]");
    }
    if name.starts_with('.') || name.contains("..") {
        bail!("name '{name}' is not allowed");
    }
    Ok(())
}

pub fn open_or_create(dir: &Path, allow_create: bool) -> Result<Repo> {
    if dir.join(fkit_core::repo::META_DIR).is_dir() {
        return Repo::open(dir);
    }
    if !allow_create {
        bail!("no such repository (server is running with --no-create)");
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    Repo::init(dir)
}

pub struct Config {
    pub data_dir: PathBuf,
    pub listen: String,
    pub token: Option<String>,
    pub allow_create: bool,
    /// Explicit opt-in to running open on a non-loopback address.
    pub insecure_no_auth: bool,
}

/// Is this address reachable only from the local machine?
pub fn is_loopback_addr(listen: &str) -> bool {
    let host = match listen.rsplit_once(':') {
        // Strip an IPv6 bracket form like [::1]:7420
        Some((h, _)) => h.trim_start_matches('[').trim_end_matches(']'),
        None => listen,
    };
    host == "127.0.0.1" || host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// Refuse to serve an unauthenticated daemon to the network by accident.
///
/// The old behaviour printed a warning and carried on, which is exactly the
/// kind of notice that scrolls past in a container log. Binding `0.0.0.0` with
/// no token now fails to start unless it was asked for explicitly.
pub fn check_exposure(cfg: &Config) -> Result<()> {
    if cfg.token.is_some() || cfg.insecure_no_auth || is_loopback_addr(&cfg.listen) {
        return Ok(());
    }
    bail!(
        "refusing to listen on {} with no authentication.\n\
         \n\
         Anyone who can reach this port could read and overwrite every repository.\n\
         Either set FKIT_TOKEN, or pass --insecure-no-auth if the network is\n\
         genuinely trusted. For user accounts and per-repo permissions, run\n\
         fkit-hub instead.",
        cfg.listen
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(listen: &str, token: Option<&str>, insecure: bool) -> Config {
        Config {
            data_dir: PathBuf::from("/tmp"),
            listen: listen.to_string(),
            token: token.map(str::to_string),
            allow_create: true,
            insecure_no_auth: insecure,
        }
    }

    #[test]
    fn loopback_detection() {
        for a in ["127.0.0.1:7420", "localhost:7420", "[::1]:7420", "127.0.0.5:1"] {
            assert!(is_loopback_addr(a), "{a} should be loopback");
        }
        for a in ["0.0.0.0:7420", "192.168.1.10:7420", "[::]:7420"] {
            assert!(!is_loopback_addr(a), "{a} should not be loopback");
        }
    }

    #[test]
    fn refuses_to_expose_an_unauthenticated_daemon() {
        assert!(check_exposure(&cfg("0.0.0.0:7420", None, false)).is_err());
        // ...but every explicit path is allowed.
        assert!(check_exposure(&cfg("0.0.0.0:7420", Some("s3cret"), false)).is_ok());
        assert!(check_exposure(&cfg("0.0.0.0:7420", None, true)).is_ok());
        assert!(check_exposure(&cfg("127.0.0.1:7420", None, false)).is_ok());
    }

    #[test]
    fn rejects_path_traversal() {
        for bad in ["..", "../etc", "a/../../b", ".hidden", "", "a/b", "a\\b", "a b"] {
            assert!(validate_name(bad).is_err(), "should reject {bad:?}");
        }
        for good in ["myrepo", "my-repo", "my_repo", "repo.v2", "main"] {
            assert!(validate_name(good).is_ok(), "should accept {good:?}");
        }
    }
}
