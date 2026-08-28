//! Shared application state.

use anyhow::Result;
use fkit_core::store::Store;
use std::path::PathBuf;
use uuid::Uuid;

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
    pub fn store_for(&self, repo_id: Uuid) -> Result<Store> {
        Store::open(self.data_dir.join("repos").join(repo_id.to_string()).join("objects"))
    }

    pub fn repo_path(&self, repo_id: Uuid) -> PathBuf {
        self.data_dir.join("repos").join(repo_id.to_string())
    }
}
