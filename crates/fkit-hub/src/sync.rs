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
    /// Whether the credential that opened this session links what it pushes to
    /// its owner. Off for a mirror's token.
    attributes: bool,
    label: String,
    actor: String,
}

impl PgHost {
    /// The rules that bind this pusher, or `None` if none do.
    ///
    /// Read before the transaction opens, deliberately. Taking a second pooled
    /// connection while holding a row lock is a deadlock waiting for load: with
    /// sixteen connections, sixteen concurrent force-pushes would each hold a
    /// transaction while waiting for a seventeenth that never comes. Rules do
    /// not change on the timescale of a single push, so reading them a moment
    /// early costs nothing — and a push racing an administrator adding a rule
    /// has no defined order anyway.
    ///
    /// Fails closed: if they cannot be read, everything is refused. Protection
    /// that evaporates when the database is unhappy is not protection.
    /// Record which account delivered these commits.
    ///
    /// Walks back from the new tip and stops at the first commit already
    /// recorded: everything behind that one was recorded when *it* arrived, so
    /// there is nothing further to learn. After the first push this touches
    /// only what the push actually added.
    ///
    /// Deliberately outside `advance_ref`'s transaction. That one holds a lock
    /// on the ref row and every concurrent push to the branch waits behind it;
    /// attribution is not worth widening that window, and a push whose
    /// attribution failed is still a push that happened.
    fn record_authorship(&self, user_id: Uuid, tip: Hash) {
        // A first import can be enormous. The cap keeps one push from walking
        // a hundred thousand commits while the client waits for its reply; the
        // ones beyond it simply have no linked account, which is the same
        // state as a commit pushed before this existed.
        const MAX: usize = 5_000;

        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![tip];
        let mut batch: Vec<Vec<u8>> = Vec::new();

        while let Some(h) = stack.pop() {
            if batch.len() >= MAX {
                tracing::warn!(
                    "push to {} attributed the first {MAX} commits; the rest are unlinked",
                    self.label
                );
                break;
            }
            if !seen.insert(h) {
                continue;
            }
            // Already attributed means its ancestors are too — stop here
            // rather than walking history that is already answered.
            if self.already_attributed(h) {
                continue;
            }
            let Ok(fkit_core::object::Object::Commit(c)) = self.store.get(h) else { continue };
            batch.push(h.0.to_vec());
            stack.extend(c.parents);
        }

        if batch.is_empty() {
            return;
        }
        let n = batch.len();

        // First writer wins. A force-push, or a fork pushing the same commits
        // somewhere else, must not reattribute what someone else delivered.
        let done = self.rt.block_on(
            sqlx::query(
                "INSERT INTO commit_authors (commit_hash, user_id, repo_id)
                 SELECT h, $2, $3 FROM UNNEST($1::bytea[]) AS h
                 ON CONFLICT (commit_hash) DO NOTHING",
            )
            .bind(&batch)
            .bind(user_id)
            .bind(self.repo.id)
            .execute(&self.state.db),
        );

        match done {
            Ok(r) => tracing::debug!("attributed {} of {n} commit(s)", r.rows_affected()),
            // Never fatal: the push has already landed, and an unlinked commit
            // shows its author string exactly as it did before.
            Err(e) => tracing::warn!("could not attribute commits for {}: {e}", self.label),
        }
    }

    fn already_attributed(&self, h: Hash) -> bool {
        self.rt
            .block_on(
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(SELECT 1 FROM commit_authors WHERE commit_hash = $1)",
                )
                .bind(h.0.to_vec())
                .fetch_one(&self.state.db),
            )
            .unwrap_or(false)
    }
    fn rules_for(&self, branch: &str) -> Option<String> {
        if crate::rules::exempt(self.user_id, self.repo.owner_id) {
            return None;
        }
        let loaded = self
            .rt
            .block_on(crate::rules::for_repo(&self.state.db, self.repo.id));
        match loaded {
            Ok(rules) => crate::rules::deny_force(&rules, branch),
            Err(e) => {
                tracing::error!("could not read branch rules for {}: {e}", self.label);
                Some("branch protection could not be checked, so this push is refused".into())
            }
        }
    }
}

impl RepoHost for PgHost {
    // ---- stashes ----
    //
    // Parked work belongs to an account, so an anonymous session has nobody to
    // park it for and every one of these refuses. The repository is the one the
    // session opened, which is what keeps a stash from crossing repositories.

    fn put_stash(&self, tip: Hash, message: &str) -> anyhow::Result<()> {
        let Some(uid) = self.user_id else {
            anyhow::bail!("sign in to keep stashes on this server");
        };
        let Ok(fkit_core::Object::Commit(c)) = self.store.get(tip) else {
            anyhow::bail!("that hash does not name a commit");
        };
        let Some(&base) = c.parents.first() else {
            anyhow::bail!("a stash must have the commit it was taken from as a parent");
        };

        // What the closure actually occupies here, for the quota. Measured
        // rather than taken on trust from the client.
        let bytes = crate::stash::closure_bytes(&self.store, tip);

        self.rt.block_on(async {
            if let Err(e) = crate::stash::sweep(&self.state.db).await {
                tracing::warn!("sweeping expired stashes: {e}");
            }
            crate::stash::create(
                &self.state.db,
                uid,
                self.repo.id,
                tip,
                base,
                message,
                bytes,
                crate::stash::DEFAULT_DAYS,
            )
            .await
            .map(|_| ())
            .map_err(|e| anyhow!("{e}"))
        })
    }

    fn list_stashes(&self) -> anyhow::Result<Vec<(Hash, String)>> {
        let Some(uid) = self.user_id else { return Ok(Vec::new()) };
        let rows = self
            .rt
            .block_on(crate::stash::list(&self.state.db, uid, self.repo.id))?;
        Ok(rows
            .into_iter()
            .filter_map(|s| {
                let h: [u8; 32] = s.commit_hash.try_into().ok()?;
                Some((Hash(h), s.message))
            })
            .collect())
    }

    fn owns_stash(&self, commit: Hash) -> anyhow::Result<bool> {
        let Some(uid) = self.user_id else { return Ok(false) };
        Ok(self.rt.block_on(crate::stash::owned_by(
            &self.state.db,
            uid,
            self.repo.id,
            commit,
        ))?)
    }

    fn drop_stash(&self, commit: Hash) -> anyhow::Result<()> {
        let Some(uid) = self.user_id else {
            anyhow::bail!("sign in to keep stashes on this server");
        };
        self.rt
            .block_on(crate::stash::drop_by_commit(
                &self.state.db,
                uid,
                self.repo.id,
                commit,
            ))
            .map_err(|e| anyhow!("{e}"))
    }

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

    /// Move a ref, or refuse to.
    ///
    /// The fast-forward check and the write share a transaction with the ref
    /// row locked between them. Without that lock two simultaneous pushes can
    /// both observe the old tip, both pass the check, and one silently
    /// discards the other's commits. This is the concrete thing Postgres buys
    /// the hub over the file-backed daemon.
    ///
    /// Creating a branch is the case that lock cannot cover: `FOR UPDATE`
    /// locks the rows it selects, and selecting a branch that does not exist
    /// yet selects none. Two pushes creating the same name therefore both saw
    /// nothing, both skipped every check, and the second overwrote the first —
    /// an acknowledged push whose commits were no longer on the branch. So a
    /// creation is decided by the insert itself, which the unique index makes
    /// exactly one of them win, and the loser retries against the row that now
    /// exists and goes through the ordinary checks.
    fn advance_ref(&self, branch: &str, tip: Hash, force: bool) -> Result<RefUpdate> {
        // Outside the transaction: see `rules_for`.
        let denial = self.rules_for(branch);

        self.rt.block_on(async {
            // Bounded, because each turn either settles or loses a race it
            // cannot lose twice for the same reason; an unbounded loop here
            // would be a way to spin forever on a bug.
            for _ in 0..8 {
                let mut tx = self.state.db.begin().await?;

                let existing: Option<(Vec<u8>,)> = sqlx::query_as(
                    "SELECT target FROM refs WHERE repo_id = $1 AND name = $2 FOR UPDATE",
                )
                .bind(self.repo.id)
                .bind(branch)
                .fetch_optional(&mut *tx)
                .await?;

                let Some((bytes,)) = existing else {
                    // Creating. Nothing is locked, so let the unique index
                    // pick the winner rather than trusting what we just read.
                    let done = sqlx::query(
                        "INSERT INTO refs (repo_id, name, target, updated_by)
                         VALUES ($1, $2, $3, $4)
                         ON CONFLICT (repo_id, name) DO NOTHING",
                    )
                    .bind(self.repo.id)
                    .bind(branch)
                    .bind(tip.0.to_vec())
                    .bind(self.user_id)
                    .execute(&mut *tx)
                    .await?;

                    if done.rows_affected() == 1 {
                        tx.commit().await?;
                        return Ok(RefUpdate::Updated);
                    }

                    // Somebody created it between our read and our insert.
                    // Start again: the row exists now, so the next turn takes
                    // the lock and applies the checks it should have.
                    tx.rollback().await?;
                    continue;
                };

                let old =
                    Hash(bytes.try_into().map_err(|_| anyhow!("corrupt ref target"))?);
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
                } else {
                    // Reachability is a pure question about the object store,
                    // so it is safe to answer while holding the row lock.
                    let adds_only = is_ancestor(&self.store, old, tip)?;
                    if !adds_only {
                        // A rewrite: it drops commits that were already here.
                        // Protection is about exactly this, so it is checked
                        // whether or not --force was passed — the flag says
                        // the pusher meant it, not that they may.
                        if let Some(why) = denial {
                            return Ok(RefUpdate::Refused(why));
                        }
                        if !force {
                            return Ok(RefUpdate::NotFastForward);
                        }
                    }
                }

                // The row is locked, so a plain update is the whole story.
                sqlx::query(
                    "UPDATE refs SET target = $3, updated_at = now(), updated_by = $4
                      WHERE repo_id = $1 AND name = $2",
                )
                .bind(self.repo.id)
                .bind(branch)
                .bind(tip.0.to_vec())
                .bind(self.user_id)
                .execute(&mut *tx)
                .await?;

                tx.commit().await?;
                return Ok(RefUpdate::Updated);
            }

            Err(anyhow!("{branch} is being pushed to to too heavily to settle"))
        })
    }

    fn on_push(&self, branch: &str, tip: Hash, stats: &TransferStats) {
        tracing::info!(
            "push {}/{branch} -> {} by {} ({} objects, {} bytes)",
            self.label, tip.short(), self.actor, stats.objects, stats.bytes
        );
        // A mirror carries other people's history. Stamping this account on
        // all of it would be worse than leaving it flat: a wrong name with a
        // face and a profile link behind it reads as fact.
        if let Some(uid) = self.user_id
            && self.attributes
        {
            self.record_authorship(uid, tip);
        }
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

    let (uid, admin, can_write, attributes, actor) = match &viewer {
        Some(t) => (
            Some(t.user.id),
            t.user.is_admin,
            t.can_write,
            t.attributes,
            t.user.username.clone(),
        ),
        // Anonymous cannot push, so attribution never arises.
        None => (None, false, false, false, "anonymous".to_string()),
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
        attributes,
        actor,
    })
}
