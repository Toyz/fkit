//! Work in progress, parked on the server.
//!
//! A stash is an ordinary commit holding a working tree, parented on the HEAD
//! it was taken from. Everything useful follows from that parent: restoring it
//! is a three-way merge against the exact tree the work was written on, and
//! looking at it is `base..commit` — which is what every commit page already
//! renders, because a commit page diffs a commit against its first parent. So
//! the server needs no diff machinery of its own here, only the bookkeeping.
//!
//! # Not a ref
//!
//! Refs are the namespace every listing reads: the branch picker, the compare
//! view, the refs endpoint, the greeting the sync protocol sends on every
//! connection. Putting a private thing in a shared namespace makes its privacy
//! depend on all of those remembering to filter, including the ones nobody has
//! written yet. A row in a table nothing else reads cannot leak that way.
//!
//! # Not visible to anybody else
//!
//! Including administrators. A site administrator can read every repository —
//! that is what the word means for whoever runs the server, and it is
//! disclosed — but somebody else's unfinished work is a different promise, and
//! this is where that rule stops.

use crate::error::{AppError, AppResult};
use fkit_core::Hash;
use uuid::Uuid;

/// One parked change set.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Stash {
    pub id: Uuid,
    #[serde(serialize_with = "hex")]
    pub commit_hash: Vec<u8>,
    #[serde(serialize_with = "hex")]
    pub base_hash: Vec<u8>,
    pub message: String,
    pub bytes: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

fn hex<S: serde::Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&v.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// How long a stash lives unless the server says otherwise.
///
/// Long enough to cover a weekend and a forgotten laptop; short enough that a
/// server is not quietly accumulating everybody's abandoned work forever.
pub const DEFAULT_DAYS: i64 = 30;

/// What one account may park in one repository.
///
/// A stash's base can be any commit, including history the server has never
/// seen — that is the point, since the whole use is carrying work off a branch
/// you have not pushed. Which means a stash can bring arbitrary bytes with it,
/// so the limit is a cap rather than a rule about what the base may be. A
/// reachability check would refuse exactly the case this feature exists for.
pub const MAX_PER_REPO: i64 = 20;
pub const MAX_BYTES_PER_REPO: i64 = 256 * 1024 * 1024;

/// Everything this account has parked here, newest first.
pub async fn list(db: &sqlx::PgPool, user: Uuid, repo: Uuid) -> sqlx::Result<Vec<Stash>> {
    sqlx::query_as(
        "SELECT id, commit_hash, base_hash, message, bytes, created_at, expires_at
           FROM stashes
          WHERE user_id = $1 AND repo_id = $2 AND expires_at > now()
          ORDER BY created_at DESC",
    )
    .bind(user)
    .bind(repo)
    .fetch_all(db)
    .await
}

/// One parked change set, with the repository it belongs to.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct MineRow {
    pub id: Uuid,
    pub owner: String,
    pub repo: String,
    #[serde(serialize_with = "hex")]
    pub commit_hash: Vec<u8>,
    #[serde(serialize_with = "hex")]
    pub base_hash: Vec<u8>,
    pub message: String,
    pub bytes: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// Everything this account has parked anywhere.
///
/// Across repositories on purpose: a stash belongs to one repository, but the
/// question "where did I leave that" is not one you can ask a repository,
/// because the answer is that you cannot remember which.
pub async fn mine(db: &sqlx::PgPool, user: Uuid) -> sqlx::Result<Vec<MineRow>> {
    sqlx::query_as(
        "SELECT s.id, u.username AS owner, r.name AS repo,
                s.commit_hash, s.base_hash, s.message, s.bytes,
                s.created_at, s.expires_at
           FROM stashes s
           JOIN repos r ON r.id = s.repo_id
           JOIN users u ON u.id = r.owner_id
          WHERE s.user_id = $1 AND s.expires_at > now()
          ORDER BY s.created_at DESC",
    )
    .bind(user)
    .fetch_all(db)
    .await
}

/// Park one, refusing it if this account is already over its limit.
#[allow(clippy::too_many_arguments)]
pub async fn create(
    db: &sqlx::PgPool,
    user: Uuid,
    repo: Uuid,
    commit: Hash,
    base: Hash,
    message: &str,
    bytes: i64,
    days: i64,
) -> AppResult<Stash> {
    let (count, used): (i64, i64) = sqlx::query_as(
        "SELECT count(*), COALESCE(sum(bytes), 0)::bigint
           FROM stashes WHERE user_id = $1 AND repo_id = $2 AND expires_at > now()",
    )
    .bind(user)
    .bind(repo)
    .fetch_one(db)
    .await?;

    if count >= MAX_PER_REPO {
        return Err(AppError::Conflict(format!(
            "you already have {MAX_PER_REPO} stashes on this repository — \
             drop one, or let it expire"
        )));
    }
    if used + bytes > MAX_BYTES_PER_REPO {
        return Err(AppError::Conflict(format!(
            "that would put you over {} MB of stashes on this repository",
            MAX_BYTES_PER_REPO / (1024 * 1024)
        )));
    }

    let row: Stash = sqlx::query_as(
        "INSERT INTO stashes
             (id, user_id, repo_id, commit_hash, base_hash, message, bytes, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(days => $8::int))
         ON CONFLICT (user_id, repo_id, commit_hash) DO UPDATE
           SET message    = EXCLUDED.message,
               expires_at = EXCLUDED.expires_at
         RETURNING id, commit_hash, base_hash, message, bytes, created_at, expires_at",
    )
    .bind(Uuid::new_v4())
    .bind(user)
    .bind(repo)
    .bind(commit.0.to_vec())
    .bind(base.0.to_vec())
    .bind(message)
    .bind(bytes)
    .bind(days as i32)
    .fetch_one(db)
    .await?;
    Ok(row)
}

/// Remove one. Scoped to its owner, so somebody else's id is simply absent.
pub async fn drop_one(db: &sqlx::PgPool, user: Uuid, repo: Uuid, id: Uuid) -> AppResult<()> {
    let done = sqlx::query("DELETE FROM stashes WHERE id = $1 AND user_id = $2 AND repo_id = $3")
        .bind(id)
        .bind(user)
        .bind(repo)
        .execute(db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::not_found("no such stash"));
    }
    Ok(())
}

/// Is this commit a stash of `user` in this repository?
pub async fn owned_by(
    db: &sqlx::PgPool,
    user: Uuid,
    repo: Uuid,
    commit: Hash,
) -> sqlx::Result<bool> {
    let hit: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM stashes
          WHERE user_id = $1 AND repo_id = $2 AND commit_hash = $3 AND expires_at > now()",
    )
    .bind(user)
    .bind(repo)
    .bind(commit.0.to_vec())
    .fetch_optional(db)
    .await?;
    Ok(hit.is_some())
}

/// Drop one by the commit it holds, which is what a client knows it by.
pub async fn drop_by_commit(
    db: &sqlx::PgPool,
    user: Uuid,
    repo: Uuid,
    commit: Hash,
) -> AppResult<()> {
    let done = sqlx::query(
        "DELETE FROM stashes WHERE user_id = $1 AND repo_id = $2 AND commit_hash = $3",
    )
    .bind(user)
    .bind(repo)
    .bind(commit.0.to_vec())
    .execute(db)
    .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::not_found("no such stash"));
    }
    Ok(())
}

/// What a stash's closure occupies here.
///
/// Walked rather than taken from the client: the quota is the server's to
/// enforce, and a figure the pusher supplies is a figure the pusher chooses.
/// Objects shared with history already on the server still count — measuring
/// what is *new* would mean asking what else references them, which is the
/// question collection exists to answer and not one worth answering per push.
pub fn closure_bytes(store: &fkit_core::Store, tip: Hash) -> i64 {
    match fkit_core::gc::reachable(store, &[tip]) {
        Ok(live) => live
            .iter()
            .filter_map(|h| store.get_raw(*h).ok().map(|b| b.len() as i64))
            .sum(),
        Err(_) => 0,
    }
}

/// Commits that must survive collection: every live stash anywhere in this
/// fork network, whoever it belongs to.
///
/// Scoped to the network rather than the repository for the same reason refs
/// are — a fork's objects live in the same store, so collecting against one
/// repository's roots alone would delete another's. A stash is set-aside work
/// that nothing else points at, which makes it exactly what this walk would
/// otherwise decide is garbage.
pub async fn roots(db: &sqlx::PgPool, network: Uuid) -> sqlx::Result<Vec<Hash>> {
    let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
        "SELECT s.commit_hash FROM stashes s
           JOIN repos r ON r.id = s.repo_id
          WHERE r.network_id = $1 AND s.expires_at > now()",
    )
    .bind(network)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(b,)| b.try_into().ok().map(Hash))
        .collect())
}

/// May `viewer` be shown this commit?
///
/// Every other object in a store belongs to the repository's published
/// history, so the commit route never had to ask. A stash is the first thing
/// that is present but not public, and the route resolves any hash it is
/// given — so without this, a stash on a public repository would render for
/// anyone who had its hash.
///
/// Unguessable hashes would make that theoretical, but "nobody can guess it"
/// is a decision to take deliberately rather than to inherit.
pub async fn may_view(
    db: &sqlx::PgPool,
    viewer: Option<Uuid>,
    commit: Hash,
) -> sqlx::Result<bool> {
    let owner: Option<(Uuid,)> =
        sqlx::query_as("SELECT user_id FROM stashes WHERE commit_hash = $1 LIMIT 1")
            .bind(commit.0.to_vec())
            .fetch_optional(db)
            .await?;
    Ok(match owner {
        // Not a stash: an ordinary commit, judged by the repository's own
        // visibility like everything else.
        None => true,
        Some((who,)) => viewer == Some(who),
    })
}

/// Delete what has expired. Returns how many rows went.
///
/// Correctness does not depend on this: every read filters on `expires_at`, so
/// an expired stash is invisible and has stopped being a collection root the
/// moment it lapses. This is tidying, which is why it rides along with pushing
/// one rather than needing a scheduler of its own — the same bargain `commit`
/// makes when it folds segments while it is already there.
///
/// Only the rows: the objects stop being roots the moment the row is gone, and
/// collection reclaims them on its own schedule. Deleting objects here would
/// mean deciding whether anything else still needs them, which is the question
/// collection exists to answer.
pub async fn sweep(db: &sqlx::PgPool) -> sqlx::Result<u64> {
    let done = sqlx::query("DELETE FROM stashes WHERE expires_at <= now()")
        .execute(db)
        .await?;
    Ok(done.rows_affected())
}
