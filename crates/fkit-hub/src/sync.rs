//! The push/pull endpoint — same protocol as `fkitd`, same port as the web UI.
//!
//! The conversation itself lives in [`fkit_core::session`]; this module supplies
//! the two things the hub does differently: **who you are** (personal access
//! token, per-repo role) and **where refs live** (Postgres, transactionally).
//!
//! # Bridging async and sync
//!
//! `fkit-core`'s protocol is synchronous — it is a straight-line conversation,
//! and blocking calls express it plainly. axum's sockets are async. Rather than
//! keep a second async copy of the negotiation in step, the socket is pumped by
//! two small async tasks while the protocol runs on a `spawn_blocking` thread:
//!
//! ```text
//!   axum WebSocket
//!     ├── reader task ──> in_tx  ──> │ ChannelTransport │ ──> serve_session
//!     └── writer task <── out_rx <── │   (blocking)     │
//! ```
//!
//! `blocking_send`/`blocking_recv` exist for exactly this, and a
//! `spawn_blocking` thread is not a runtime worker, so blocking there is sound.

use crate::models::RepoRow;
use crate::perms::{resolve, Access};
use crate::state::AppState;
use anyhow::{anyhow, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use fkit_core::hash::Hash;
use fkit_core::proto::{is_ancestor, TransferStats, Transport};
use fkit_core::session::{read_hello, send_error, send_welcome, serve_session, RefUpdate, RepoHost};
use fkit_core::store::Store;
use futures::{SinkExt, StreamExt};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use uuid::Uuid;

pub async fn handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    ws.on_upgrade(move |socket| serve(socket, state, owner, name))
}

struct ChannelTransport {
    tx: mpsc::Sender<Vec<u8>>,
    rx: mpsc::Receiver<Vec<u8>>,
}

impl Transport for ChannelTransport {
    fn send_bytes(&mut self, payload: &[u8]) -> Result<()> {
        self.tx
            .blocking_send(payload.to_vec())
            .map_err(|_| anyhow!("client disconnected"))
    }
    fn recv_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(self.rx.blocking_recv())
    }
}

/// A repository whose refs live in Postgres.
struct PgHost {
    state: AppState,
    rt: Handle,
    store: Store,
    repo: RepoRow,
    access: Access,
    user_id: Option<Uuid>,
    label: String,
    actor: String,
}

impl RepoHost for PgHost {
    fn store(&self) -> &Store {
        &self.store
    }

    fn refs(&self) -> Result<Vec<(String, Hash)>> {
        let rows: Vec<(String, Vec<u8>)> = self.rt.block_on(
            sqlx::query_as("SELECT name, target FROM refs WHERE repo_id = $1")
                .bind(self.repo.id)
                .fetch_all(&self.state.db),
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|(n, t)| Some((n, Hash(t.try_into().ok()?))))
            .collect())
    }

    fn read_ref(&self, branch: &str) -> Result<Option<Hash>> {
        let row: Option<(Vec<u8>,)> = self.rt.block_on(
            sqlx::query_as("SELECT target FROM refs WHERE repo_id = $1 AND name = $2")
                .bind(self.repo.id)
                .bind(branch)
                .fetch_optional(&self.state.db),
        )?;
        Ok(row.and_then(|(t,)| Some(Hash(t.try_into().ok()?))))
    }

    fn can_write(&self) -> bool {
        self.access.can_write()
    }

    /// The fast-forward check and the write share a transaction, with the ref
    /// row locked between them. Without that lock two simultaneous pushes can
    /// both observe the old tip, both pass the check, and one silently discards
    /// the other's commits. This is the concrete thing Postgres buys the hub
    /// over the file-backed daemon.
    fn advance_ref(&self, branch: &str, tip: Hash, force: bool) -> Result<RefUpdate> {
        self.rt.block_on(async {
            let mut tx = self.state.db.begin().await?;

            let existing: Option<(Vec<u8>,)> = sqlx::query_as(
                "SELECT target FROM refs WHERE repo_id = $1 AND name = $2 FOR UPDATE",
            )
            .bind(self.repo.id)
            .bind(branch)
            .fetch_optional(&mut *tx)
            .await?;

            if let Some((bytes,)) = &existing {
                let old = Hash(
                    bytes.clone().try_into().map_err(|_| anyhow!("corrupt ref target"))?,
                );
                if old == tip {
                    return Ok(RefUpdate::AlreadyCurrent);
                }
                // A tag has no history to fast-forward along. Moving one makes
                // every checkout of that name silently mean something else, so
                // it takes an explicit force rather than passing the ancestry
                // test a later commit would happen to satisfy.
                if fkit_core::session::is_tag(branch) {
                    if !force {
                        return Ok(RefUpdate::NotFastForward);
                    }
                } else if !force && !is_ancestor(&self.store, old, tip)? {
                    // Reachability is a pure question about the object store,
                    // so it is safe to answer while holding the row lock.
                    return Ok(RefUpdate::NotFastForward);
                }
            }

            sqlx::query(
                "INSERT INTO refs (repo_id, name, target, updated_by)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (repo_id, name) DO UPDATE
                   SET target = EXCLUDED.target,
                       updated_at = now(),
                       updated_by = EXCLUDED.updated_by",
            )
            .bind(self.repo.id)
            .bind(branch)
            .bind(tip.0.to_vec())
            .bind(self.user_id)
            .execute(&mut *tx)
            .await?;

            sqlx::query("UPDATE repos SET updated_at = now() WHERE id = $1")
                .bind(self.repo.id)
                .execute(&mut *tx)
                .await?;

            sqlx::query(
                "INSERT INTO audit_log (actor_id, repo_id, action, detail)
                 VALUES ($1, $2, 'ref.update', $3)",
            )
            .bind(self.user_id)
            .bind(self.repo.id)
            .bind(serde_json::json!({
                "branch": branch, "target": tip.to_hex(), "force": force
            }))
            .execute(&mut *tx)
            .await?;

            tx.commit().await?;
            Ok(RefUpdate::Updated)
        })
    }

    fn on_push(&self, branch: &str, tip: Hash, stats: &TransferStats) {
        tracing::info!(
            "push {}/{branch} -> {} by {} ({} objects, {} bytes)",
            self.label, tip.short(), self.actor, stats.objects, stats.bytes
        );
    }

    fn on_pull(&self, branch: &str, stats: &TransferStats) {
        tracing::info!(
            "pull {}/{branch} by {} ({} objects, {} bytes)",
            self.label, self.actor, stats.objects, stats.bytes
        );
    }
}

async fn serve(socket: WebSocket, state: AppState, owner: String, name: String) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(16);
    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(16);

    let writer = tokio::spawn(async move {
        while let Some(bytes) = out_rx.recv().await {
            if sink.send(Message::Binary(bytes.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Binary(b) => {
                    if in_tx.send(b.to_vec()).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    let rt = Handle::current();
    let outcome = tokio::task::spawn_blocking(move || {
        let mut t = ChannelTransport { tx: out_tx, rx: in_rx };
        run(&mut t, state, rt, &owner, &name)
    })
    .await;

    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::info!("sync session ended: {e:#}"),
        Err(e) => tracing::error!("sync task panicked: {e}"),
    }
    reader.abort();
    let _ = writer.await;
}

fn run(t: &mut ChannelTransport, state: AppState, rt: Handle, owner: &str, name: &str) -> Result<()> {
    let (_, token) = read_hello(t)?;

    let host = match rt.block_on(authorise(&state, &rt, &token, owner, name)) {
        Ok(h) => h,
        Err(message) => {
            // "No such repository" and "no access" are deliberately the same
            // message, so the sync endpoint is not an existence oracle either.
            send_error(t, message)?;
            return Ok(());
        }
    };

    send_welcome(t, host.refs()?)?;
    serve_session(t, &host)?;
    Ok(())
}

async fn authorise(
    state: &AppState,
    rt: &Handle,
    token: &str,
    owner: &str,
    name: &str,
) -> std::result::Result<PgHost, String> {
    let hidden = || format!("no such repository: {owner}/{name}");

    let viewer = if token.is_empty() {
        None
    } else {
        crate::auth::lookup_token(&state.db, token).await
    };

    let repo: Option<RepoRow> = sqlx::query_as(
        "SELECT r.* FROM repos r JOIN users u ON u.id = r.owner_id
         WHERE u.username = $1 AND r.name = $2",
    )
    .bind(owner.to_ascii_lowercase())
    .bind(name)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| "internal error".to_string())?;

    let repo = repo.ok_or_else(hidden)?;

    let (uid, admin, can_write, actor) = match &viewer {
        Some((u, w)) => (Some(u.id), u.is_admin, *w, u.username.clone()),
        None => (None, false, false, "anonymous".to_string()),
    };

    let access = resolve(&state.db, &repo, uid, admin, can_write, state.policy().require_auth)
        .await
        .map_err(|_| "internal error".to_string())?;

    if !access.can_read() {
        return Err(hidden());
    }

    let store = state.store_for_network(repo.network_id).map_err(|_| "internal error".to_string())?;

    Ok(PgHost {
        label: format!("{owner}/{name}"),
        state: state.clone(),
        rt: rt.clone(),
        store,
        repo,
        access,
        user_id: uid,
        actor,
    })
}
