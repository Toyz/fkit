//! fkit-core — content-addressed storage and Merkle DAG primitives.
//!
//! Read the modules in this order to understand the system:
//!
//! 1. [`hash`]    — how things are named
//! 2. [`object`]  — the four node types and their canonical encoding
//! 3. [`chunker`] — how file bytes are split so that edits stay cheap
//! 4. [`store`]   — how objects live on disk

pub mod archive;
pub mod cache;
pub mod checkout;
pub mod chunker;
pub mod config;
pub mod diff;
pub mod fsck;
pub mod fastimport;
pub mod gc;
pub mod hash;
pub mod index;
pub mod ingest;
pub mod merge;
pub mod object;
pub mod pack;
pub mod proof;
pub mod proto;
pub mod repo;
pub mod session;
pub mod store;
pub mod submodule;
pub mod ws;

pub use hash::Hash;
pub use object::{Commit, EntryKind, Kind, Object, TreeEntry};
pub use store::Store;
pub use repo::{Change, Head, Repo, Snapshot, View};
