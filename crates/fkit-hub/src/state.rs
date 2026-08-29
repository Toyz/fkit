//! Shared application state.

use anyhow::Result;
use fkit_core::store::Store;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

use crate::ratelimit::RateLimiter;

#[derive(Clone)]
pub struct AppState {
    pub db: sqlx::PgPool,
    pub data_dir: PathBuf,
    /// Set when served over TLS, so session cookies get the `Secure` attribute.
    pub secure_cookies: bool,
    /// Built frontend directory. Held in state so every handler resolves it the
    /// same way, rather than some reading a flag and others an env var.
    pub web_dir: PathBuf,
    /// Live instance policy, changeable by an administrator at runtime.
    pub settings: crate::settings::Settings,
    /// Largest archive this server will build, in bytes of content. 0 = no
    /// limit. Checked against the tree before any file is read.
    pub max_archive_bytes: u64,
    /// Ceiling on the cheap-to-ask, expensive-to-answer endpoints. Behind a
    /// trait so the counters can move out of this process without any route
    /// changing — see [`crate::ratelimit`].
    pub limiter: Arc<dyn RateLimiter>,
    /// Whether `X-Forwarded-For` may be believed when identifying a client.
    pub trust_proxy: bool,
    /// One object cache for the whole server, handed to every `Store` it
    /// opens. Behind a trait for the same reason the rate limiter is: it can
    /// move out of this process without any read path changing — see
    /// [`fkit_core::cache`].
    pub object_cache: Arc<dyn fkit_core::cache::ObjectCache>,
}

impl AppState {
    pub fn policy(&self) -> crate::settings::Instance {
        self.settings.get()
    }
}

impl AppState {
    /// Each repository gets its own object store.
    ///
    /// A single shared store across all repositories would deduplicate more —
    /// the same dependency vendored into two projects would be stored once — but
    /// it entangles private data: an object written by a private repo would be
    /// retrievable by anyone who could name its hash, and deleting a repository
    /// could no longer be a directory removal. Per-repo isolation is the
    /// conservative default; a shared pool is a later optimisation with a
    /// reference-counting design behind it.
    /// Open the object store a repository reads and writes.
    ///
    /// Keyed by the fork *network*, not the repository: forks share one store.
    /// An object's name is a digest of its bytes, so two repositories in a
    /// network cannot disagree about what a hash means, which makes sharing
    /// safe by construction — and makes a fork free, and a merge request
    /// between two forks need no transfer at all.
    ///
    /// Callers pass `repo.network_id`. The parameter is named for what it is
    /// so that passing a plain repo id is a visible mistake rather than a
    /// silent one.
    pub fn store_for_network(&self, network_id: Uuid) -> Result<Store> {
        let mut store = Store::open(
            self.data_dir
                .join("repos")
                .join(network_id.to_string())
                .join("objects"),
        )?;
        // One cache for the whole server rather than one per `Store`. A store
        // is opened per request, so a per-store cache would be born empty and
        // die at the end of the handler — which is every miss and no hits.
        //
        // Sharing it across repositories is safe for the same reason sharing a
        // store between forks is: the key is a digest of the value, so two
        // repositories cannot mean different things by one hash.
        store.set_cache(std::sync::Arc::clone(&self.object_cache));
        Ok(store)
    }

    /// The store for a repository that is its own network.
    ///
    /// Kept for the paths that create a repository, where the network is the
    /// repository itself and there is nothing yet to look up.
    pub fn store_for(&self, repo_id: Uuid) -> Result<Store> {
        self.store_for_network(repo_id)
    }

    pub fn repo_path(&self, repo_id: Uuid) -> PathBuf {
        self.data_dir.join("repos").join(repo_id.to_string())
    }
}
