//! Repository listing, creation, settings, collaborators, and branch refs.

use crate::auth::Viewer;
use crate::error::{AppError, AppResult};
use crate::models::*;
use crate::perms::{require_admin, require_read, require_write, resolve};
use crate::perms::Access;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use fkit_core::hash::Hash;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/repos", get(list_repos).post(create_repo))
        .route("/users/{username}", get(get_profile))
        .route("/repos/{owner}/{name}", get(get_repo))
        .route("/repos/{owner}/{name}", patch(update_repo))
        .route("/repos/{owner}/{name}", delete(delete_repo))
        .route("/repos/{owner}/{name}/refs", get(list_refs).delete(delete_ref))
        .route("/repos/{owner}/{name}/stats", get(repo_stats))
        .route("/repos/{owner}/{name}/gc", post(collect_garbage))
        .route("/repos/{owner}/{name}/fork", post(fork_repo))
        .route("/repos/{owner}/{name}/forks", get(list_forks))
        .route("/repos/{owner}/{name}/upstream", get(upstream))
        .route(
            "/repos/{owner}/{name}/collaborators",
            get(list_collaborators).post(add_collaborator),
        )
        .route(
            "/repos/{owner}/{name}/collaborators/{username}",
            delete(remove_collaborator),
        )
}

/// Repositories the viewer can see: their own, ones they collaborate on, and
/// every public repository.
async fn list_repos(State(state): State<AppState>, viewer: Viewer) -> AppResult<Json<Vec<RepoView>>> {
    let vid = viewer.id();
    let rows: Vec<RepoWithOwner> = sqlx::query_as(
        "SELECT r.*, u.username FROM repos r
         JOIN users u ON u.id = r.owner_id
         WHERE r.visibility = 'public'
            OR r.owner_id = $1
            OR EXISTS (SELECT 1 FROM collaborators c
                       WHERE c.repo_id = r.id AND c.user_id = $1)
         ORDER BY r.updated_at DESC
         LIMIT 200",
    )
    .bind(vid)
    .fetch_all(&state.db)
    .await?;

    let (uid, admin, can_write) = match &viewer.user {
        Some(u) => (Some(u.id), u.is_admin, u.can_write),
        None => (None, false, false),
    };

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let access = resolve(&state.db, &row.repo, uid, admin, can_write, state.policy().require_auth).await?;
        // The SQL pre-filters for speed; this is the authority. A repository the
        // viewer cannot read must not appear, even by name.
        if access.can_read() {
            out.push(super::repo_view(&row.repo, &row.username, access));
        }
    }
    super::attach_heads(&state, &mut out).await;
    Ok(Json(out))
}

#[derive(serde::Serialize)]
struct RepoStats {
    /// Commits reachable from the default branch.
    commits: usize,
    /// Objects in the store — chunks, files, trees, entries and commits.
    objects: usize,
    /// What the filesystem actually holds, after chunk deduplication and
    /// compression. Not the size of a checkout, which is generally larger.
    bytes: u64,
    /// Bytes an archive of the default branch would contain — the checkout
    /// size, which is what the limit below is measured against.
    archive_bytes: u64,
    /// This server's archive limit, 0 for none. Sent so the UI can decline to
    /// offer a download that the server would refuse, rather than handing
    /// someone a button whose only outcome is an error.
    archive_limit: u64,
}

/// Size and history counts for one repository.
///
/// The byte figure is a directory walk rather than a sum of object sizes: the
/// store packs and compresses, so adding up decoded objects would be wrong in
/// two directions at once. A packed store is a handful of files, so the walk
/// is cheap even for a repository of a few gigabytes.
async fn repo_stats(
    State(state): State<AppState>,
    viewer: Viewer,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
) -> AppResult<Json<RepoStats>> {
    let (repo, _, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    let store = state.store_for_network(repo.network_id).map_err(AppError::Internal)?;

    // The store lives under the fork network, so a fork asking its own id
    // measured a directory that does not exist and reported zero bytes. What
    // it reports now is what the network holds, which is the truth: a fork
    // adds nothing on disk until it is pushed to.
    let dir = state
        .data_dir
        .join("repos")
        .join(repo.network_id.to_string())
        .join("objects");
    let bytes = dir_size(&dir);
    let objects = store.iter_ids().map(|v| v.len()).unwrap_or(0);

    // Commits only, so this stays a walk of the history rather than of every
    // chunk in the repository.
    let mut commits = 0usize;
    let mut archive_bytes = 0u64;
    if let Some(tip) = ref_target(&state, &repo, &repo.default_branch).await? {
        // Directory objects only — the same cheap walk the archive route does.
        if let Ok(fkit_core::Object::Commit(c)) = store.get(tip)
            && let Ok(p) = fkit_core::archive::plan(&store, c.tree, "")
        {
            archive_bytes = p.bytes;
        }
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![tip];
        while let Some(h) = stack.pop() {
            if !seen.insert(h) {
                continue;
            }
            if let Ok(fkit_core::Object::Commit(c)) = store.get(h) {
                commits += 1;
                stack.extend(c.parents);
            }
        }
    }

    Ok(Json(RepoStats {
        commits,
        objects,
        bytes,
        archive_bytes,
        archive_limit: state.max_archive_bytes,
    }))
}

async fn ref_target(state: &AppState, repo: &RepoRow, name: &str) -> AppResult<Option<Hash>> {
    let row: Option<(Vec<u8>,)> =
        sqlx::query_as("SELECT target FROM refs WHERE repo_id = $1 AND name = $2")
            .bind(repo.id)
            .bind(name)
            .fetch_optional(&state.db)
            .await?;
    Ok(row.and_then(|(b,)| <[u8; 32]>::try_from(b.as_slice()).ok()).map(Hash))
}

/// Total size of every file beneath `dir`.
fn dir_size(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

/// A person's public page: who they are, and the repositories the viewer is
/// allowed to know exist.
#[derive(serde::Serialize)]
struct Profile {
    username: String,
    display_name: Option<String>,
    is_admin: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    repos: Vec<crate::models::RepoView>,
}

async fn get_profile(
    State(state): State<AppState>,
    viewer: Viewer,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> AppResult<Json<Profile>> {
    // An instance in `require_auth` mode should not confirm who has an account.
    if state.policy().require_auth {
        viewer.require()?;
    }

    #[derive(sqlx::FromRow)]
    struct Owner {
        id: uuid::Uuid,
        username: String,
        display_name: Option<String>,
        is_admin: bool,
        created_at: chrono::DateTime<chrono::Utc>,
    }

    let username = username.trim().to_ascii_lowercase();
    let owner: Option<Owner> = sqlx::query_as(
        "SELECT id, username, display_name, is_admin, created_at
           FROM users WHERE username = $1 AND is_active = TRUE",
    )
    .bind(&username)
    .fetch_optional(&state.db)
    .await?;

    let Some(owner) = owner else {
        return Err(AppError::NotFound(format!("no user named {username}")));
    };

    let rows: Vec<RepoWithOwner> = sqlx::query_as(
        "SELECT r.*, u.username FROM repos r
         JOIN users u ON u.id = r.owner_id
         WHERE r.owner_id = $1
         ORDER BY r.updated_at DESC
         LIMIT 200",
    )
    .bind(owner.id)
    .fetch_all(&state.db)
    .await?;

    let (uid, admin, can_write) = match &viewer.user {
        Some(u) => (Some(u.id), u.is_admin, u.can_write),
        None => (None, false, false),
    };

    // Same rule as the index: a repository the viewer cannot read must not
    // appear, even as a name.
    let mut repos = Vec::with_capacity(rows.len());
    for row in rows {
        let access =
            resolve(&state.db, &row.repo, uid, admin, can_write, state.policy().require_auth).await?;
        if access.can_read() {
            repos.push(super::repo_view(&row.repo, &row.username, access));
        }
    }
    super::attach_heads(&state, &mut repos).await;

    Ok(Json(Profile {
        username: owner.username,
        display_name: owner.display_name,
        is_admin: owner.is_admin,
        created_at: owner.created_at,
        repos,
    }))
}

async fn create_repo(
    State(state): State<AppState>,
    viewer: Viewer,
    Json(body): Json<CreateRepoReq>,
) -> AppResult<impl IntoResponse> {
    let u = viewer.require()?;
    if !u.can_write {
        return Err(AppError::Forbidden("this token is read-only".into()));
    }

    let name = body.name.trim().to_string();
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid {
        return Err(AppError::bad(
            "repository name must be 1-64 characters of A-Z, a-z, 0-9, dot, underscore or hyphen",
        ));
    }

    let policy = state.policy();
    let visibility = body
        .visibility
        .as_deref()
        .unwrap_or(&policy.default_repo_visibility);
    if !matches!(visibility, "public" | "private") {
        return Err(AppError::bad("visibility must be 'public' or 'private'"));
    }

    let id = Uuid::new_v4();
    let res = sqlx::query(
        "INSERT INTO repos (id, owner_id, name, description, visibility)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(u.id)
    .bind(&name)
    .bind(&body.description)
    .bind(visibility)
    .execute(&state.db)
    .await;

    if let Err(sqlx::Error::Database(e)) = &res
        && e.is_unique_violation()
    {
        return Err(AppError::conflict(format!("you already have a repository named '{name}'")));
    }
    res?;

    // Create the object store eagerly so a push never races directory creation.
    state.store_for(id).map_err(AppError::Internal)?;

    super::audit(&state, Some(u.id), Some(id), "repo.create",
        serde_json::json!({ "name": name, "visibility": visibility })).await;

    let repo: RepoRow = sqlx::query_as("SELECT * FROM repos WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(super::repo_view(&repo, &u.username, crate::perms::Access::Admin)),
    ))
}

async fn get_repo(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult<Json<RepoView>> {
    let (repo, access, owner_lc) = super::load_repo(&state, &viewer, &owner, &name).await?;
    // The same decoration the listing gets: the tip, the ref counts, and how
    // much is open. The page's tabs read the last of those, and were showing
    // nothing because this endpoint skipped the step that fills them in.
    let mut views = vec![super::repo_view(&repo, &owner_lc, access)];
    super::attach_heads(&state, &mut views).await;

    let (uid, is_admin) = match &viewer.user {
        Some(u) => (Some(u.id), u.is_admin),
        None => (None, false),
    };
    views[0].via_admin =
        crate::perms::only_via_site_admin(&state.db, &repo, uid, is_admin).await?;

    // An administrator reading someone's private repository is a power being
    // exercised, and `perms::resolve` has always claimed it was recorded. It
    // was not: only writes were. It is now.
    //
    // Written from the repository page rather than from every file and commit
    // request under it, and only when the same administrator has not already
    // been recorded here in the last hour — otherwise browsing a repository
    // would bury the log in one person reading it.
    if views[0].via_admin && let Some(actor) = uid {
        // `EXISTS` rather than `SELECT 1`, which is int4 and silently failed
        // to decode as i64 — the error was swallowed and every page view was
        // recorded. On failure this errs toward writing: an extra audit line
        // is noise, a missing one is a gap in a record someone may rely on.
        let (recent,): (bool,) = sqlx::query_as(
            "SELECT EXISTS(
                 SELECT 1 FROM audit_log
                  WHERE action = 'repo.read_as_admin' AND actor_id = $1 AND repo_id = $2
                    AND created_at > now() - interval '1 hour')",
        )
        .bind(actor)
        .bind(repo.id)
        .fetch_one(&state.db)
        .await
        .unwrap_or((false,));

        if !recent {
            super::audit(
                &state,
                Some(actor),
                Some(repo.id),
                "repo.read_as_admin",
                serde_json::json!({ "repo": format!("{owner_lc}/{}", repo.name) }),
            )
            .await;
        }
    }

    Ok(Json(views.remove(0)))
}

async fn update_repo(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Json(body): Json<UpdateRepoReq>,
) -> AppResult<Json<RepoView>> {
    let (repo, access, owner_lc) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_admin(access)?;

    if let Some(v) = &body.visibility
        && !matches!(v.as_str(), "public" | "private")
    {
        return Err(AppError::bad("visibility must be 'public' or 'private'"));
    }

    let homepage = match &body.homepage {
        Some(h) => Some(clean_homepage(h)?),
        None => None,
    };
    let topics = match &body.topics {
        Some(t) => Some(clean_topics(t)?),
        None => None,
    };

    sqlx::query(
        "UPDATE repos SET
            description    = COALESCE($2, description),
            visibility     = COALESCE($3, visibility),
            default_branch = COALESCE($4, default_branch),
            homepage       = COALESCE($5, homepage),
            topics         = COALESCE($6, topics),
            updated_at     = now()
         WHERE id = $1",
    )
    .bind(repo.id)
    .bind(&body.description)
    .bind(&body.visibility)
    .bind(&body.default_branch)
    .bind(&homepage)
    .bind(&topics)
    .execute(&state.db)
    .await?;

    super::audit(&state, viewer.id(), Some(repo.id), "repo.update",
        serde_json::json!({ "visibility": body.visibility })).await;

    let updated: RepoRow = sqlx::query_as("SELECT * FROM repos WHERE id = $1")
        .bind(repo.id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(super::repo_view(&updated, &owner_lc, access)))
}

/// Validate a homepage URL.
///
/// The browser renders this as a link on a page the viewer trusts, so the
/// scheme is the whole security question: `javascript:` and `data:` execute in
/// the viewer's session, and everything else is at best a broken link. Only
/// http and https are stored. An empty string clears it.
fn clean_homepage(raw: &str) -> AppResult<String> {
    let h = raw.trim();
    if h.is_empty() {
        return Ok(String::new());
    }
    let lower = h.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(AppError::bad("a website must start with http:// or https://"));
    }
    // A URL is a link, not a document: control characters in one are a way to
    // smuggle something past a naive renderer.
    if h.chars().any(|c| c.is_control()) {
        return Err(AppError::bad("that URL contains control characters"));
    }
    if h.len() > 512 {
        return Err(AppError::bad("that URL is too long"));
    }
    Ok(h.to_string())
}

/// Normalise topics: lower-case, de-duplicated, bounded.
fn clean_topics(raw: &[String]) -> AppResult<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for t in raw {
        let t = t.trim().to_ascii_lowercase();
        if t.is_empty() {
            continue;
        }
        if t.len() > 32 {
            return Err(AppError::bad(format!("topic '{t}' is longer than 32 characters")));
        }
        if !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
            return Err(AppError::bad(format!(
                "topic '{t}' may only contain letters, digits, hyphen and dot"
            )));
        }
        if !out.contains(&t) {
            out.push(t);
        }
    }
    if out.len() > 20 {
        return Err(AppError::bad("at most 20 topics"));
    }
    Ok(out)
}

async fn delete_repo(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult<Json<serde_json::Value>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_admin(access)?;

    // Rows first: if the directory removal fails we have still revoked access,
    // and an orphaned directory is a cleanup problem rather than a leak.
    sqlx::query("DELETE FROM repos WHERE id = $1")
        .bind(repo.id)
        .execute(&state.db)
        .await?;

    // The objects belong to the fork network, not to this repository. Deleting
    // a parent that has forks must not take their commits with it — so the
    // directory goes only once nothing else in the network is left. Counted
    // after the delete above, so this repository is not counting itself.
    let (remaining,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM repos WHERE network_id = $1")
        .bind(repo.network_id)
        .fetch_one(&state.db)
        .await
        .unwrap_or((1,));

    if remaining == 0 {
        let _ = std::fs::remove_dir_all(state.repo_path(repo.network_id));
    }

    super::audit(&state, viewer.id(), None, "repo.delete",
        serde_json::json!({ "name": format!("{owner}/{name}") })).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize, Default)]
struct GcIn {
    /// Report what would go without removing anything.
    #[serde(default)]
    dry_run: bool,
}

#[derive(Serialize)]
struct GcOut {
    dry_run: bool,
    total: usize,
    reachable: usize,
    unreachable: usize,
    /// Unreachable, but held back by the age guard.
    too_young: usize,
    loose_removed: usize,
    packed_dropped: usize,
    segments_compacted: usize,
    bytes_reclaimed: u64,
}

/// Reclaim objects no ref can reach.
///
/// Deleting a branch removes a name, not the commits under it — which is
/// correct, since objects are shared and a delete that walked the whole store
/// would be both slow and wrong. But nothing reclaimed the leftovers either,
/// so the space was gone for good. This is the other half.
///
/// The age guard is deliberately not exposed. A push writes its objects and
/// *then* moves the ref, so in that window its objects are unreachable by
/// definition; on a server there is always potentially a push in flight, and
/// `--prune-all` is only ever safe when nothing else is writing. The default
/// grace period stands, whoever asks.
async fn collect_garbage(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    body: Option<Json<GcIn>>,
) -> AppResult<Json<GcOut>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_admin(access)?;
    let dry_run = body.map(|Json(b)| b.dry_run).unwrap_or(false);

    // One collector per repository. Two walking the same store would each
    // compute a live set blind to the other's compaction. The lock lives on a
    // dedicated connection rather than the pool at large, because a session
    // advisory lock is released by the connection that took it — handing that
    // connection back mid-collection would leak the lock.
    let mut conn = state.db.acquire().await.map_err(anyhow::Error::from)?;
    // Keyed by the network, because that is what the store is keyed by: two
    // forks collecting the same store at once is the race this prevents.
    let key = i64::from_be_bytes(repo.network_id.as_bytes()[..8].try_into().unwrap());
    let (locked,): (bool,) = sqlx::query_as("SELECT pg_try_advisory_lock($1)")
        .bind(key)
        .fetch_one(&mut *conn)
        .await?;
    if !locked {
        return Err(AppError::Conflict(
            "a collection is already running for this repository".into(),
        ));
    }

    let result = async {
        // Every repository sharing this store, not just this one. A fork's
        // branches point at objects in the same store; collecting against one
        // repository's refs alone would delete another's history.
        let roots: Vec<(Vec<u8>,)> = sqlx::query_as(
            "SELECT rf.target FROM refs rf
               JOIN repos r ON r.id = rf.repo_id
              WHERE r.network_id = $1",
        )
        .bind(repo.network_id)
        .fetch_all(&state.db)
        .await?;
        let roots: Vec<Hash> = roots
            .into_iter()
            .filter_map(|(b,)| Some(Hash(b.try_into().ok()?)))
            .collect();

        let store = state.store_for_network(repo.network_id).map_err(AppError::Internal)?;
        // A graph walk plus file IO: minutes on a large repository, and none
        // of it async. Off the runtime thread it goes.
        let opts = fkit_core::gc::Options { dry_run, ..Default::default() };
        tokio::task::spawn_blocking(move || fkit_core::gc::collect(&store, &roots, opts))
            .await
            .map_err(anyhow::Error::from)?
            .map_err(AppError::Internal)
    }
    .await;

    let (unlocked,): (bool,) = sqlx::query_as("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .fetch_one(&mut *conn)
        .await?;
    debug_assert!(unlocked, "the lock we took should still be ours");

    let r = result?;

    if !dry_run && (r.loose_removed > 0 || r.packed_dropped > 0) {
        super::audit(
            &state,
            viewer.id(),
            Some(repo.id),
            "repo.gc",
            serde_json::json!({
                "removed": r.loose_removed + r.packed_dropped,
                "bytes_reclaimed": r.bytes_reclaimed,
            }),
        )
        .await;
    }

    Ok(Json(GcOut {
        dry_run,
        total: r.total,
        reachable: r.reachable,
        unreachable: r.unreachable,
        too_young: r.too_young,
        loose_removed: r.loose_removed,
        packed_dropped: r.packed_dropped,
        segments_compacted: r.segments_compacted,
        bytes_reclaimed: r.bytes_reclaimed,
    }))
}

#[derive(Deserialize, Default)]
struct ForkIn {
    /// A different name, when one is already taken in your account.
    name: Option<String>,
}

/// Fork a repository into the signed-in account.
///
/// The objects are not copied. A fork joins its parent's *network* and reads
/// the same store, which is safe because an object's name is a digest of its
/// bytes — two repositories cannot disagree about what a hash means. So this
/// is O(1) on disk however large the repository, and a merge request between
/// two forks needs no transfer at all: both sides already resolve.
///
/// What is copied is the refs, because those are the fork's own to move.
async fn fork_repo(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    body: Option<Json<ForkIn>>,
) -> AppResult<impl IntoResponse> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_read(access, &owner, &name)?;
    let u = viewer.require()?;
    if !u.can_write {
        return Err(AppError::Forbidden("this token is read-only".into()));
    }

    let wanted = body
        .and_then(|Json(b)| b.name)
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| repo.name.clone());

    if repo.owner_id == u.id && wanted == repo.name {
        return Err(AppError::conflict(
            "this is already yours — fork it under a different name if you want a second copy",
        ));
    }

    let id = Uuid::new_v4();
    let res = sqlx::query(
        "INSERT INTO repos
            (id, owner_id, name, description, visibility, default_branch,
             homepage, topics, forked_from, network_id)
         SELECT $1, $2, $3, r.description, $4, r.default_branch,
                r.homepage, r.topics, r.id, r.network_id
           FROM repos r WHERE r.id = $5",
    )
    .bind(id)
    .bind(u.id)
    .bind(&wanted)
    // A fork of a public repository starts private: the person forking has
    // not said they want to publish anything, and making it public by default
    // decides that for them.
    .bind("private")
    .bind(repo.id)
    .execute(&state.db)
    .await;

    if let Err(sqlx::Error::Database(e)) = &res
        && e.is_unique_violation()
    {
        return Err(AppError::conflict(format!("you already have a repository called {wanted}")));
    }
    res?;

    // The refs are the fork's own from here: it can move them without moving
    // anything in the parent.
    sqlx::query(
        "INSERT INTO refs (repo_id, name, target, updated_by)
         SELECT $1, name, target, updated_by FROM refs WHERE repo_id = $2",
    )
    .bind(id)
    .bind(repo.id)
    .execute(&state.db)
    .await?;

    super::audit(&state, Some(u.id), Some(id), "repo.fork",
        serde_json::json!({ "from": format!("{owner}/{name}"), "name": wanted })).await;

    let row: RepoWithOwner = sqlx::query_as(
        "SELECT r.*, u.username FROM repos r JOIN users u ON u.id = r.owner_id WHERE r.id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    let view = super::repo_view(&row.repo, &row.username, Access::Admin);
    Ok((StatusCode::CREATED, Json(view)))
}

/// Everything forked from this repository, directly.
async fn list_forks(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult<Json<Vec<RepoView>>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_read(access, &owner, &name)?;

    let rows: Vec<RepoWithOwner> = sqlx::query_as(
        "SELECT r.*, u.username FROM repos r JOIN users u ON u.id = r.owner_id
          WHERE r.forked_from = $1 ORDER BY r.created_at DESC LIMIT 200",
    )
    .bind(repo.id)
    .fetch_all(&state.db)
    .await?;

    let (uid, admin, can_write) = match &viewer.user {
        Some(u) => (Some(u.id), u.is_admin, u.can_write),
        None => (None, false, false),
    };

    // A private fork of a public repository must not be listed to someone who
    // cannot read it — the fork's visibility is its own, not its parent's.
    let mut out = Vec::new();
    for row in rows {
        let a = resolve(&state.db, &row.repo, uid, admin, can_write, state.policy().require_auth)
            .await?;
        if a.can_read() {
            out.push(super::repo_view(&row.repo, &row.username, a));
        }
    }
    Ok(Json(out))
}

#[derive(Serialize)]
struct UpstreamView {
    /// `owner/name` of what this was forked from.
    parent: String,
    /// The branch compared on each side — each repository's own default.
    branch: String,
    parent_branch: String,
    /// Commits this fork has that the parent does not, and the reverse.
    ahead: usize,
    behind: usize,
    /// True when the two point at the same commit.
    level: bool,
}

/// How far a fork has drifted from what it was forked from.
///
/// Its own endpoint rather than a field on the repository, because answering
/// it is a graph walk: every page that shows a repository would pay for it,
/// and only a fork's page has anything to say. Compares each side's default
/// branch, which is the comparison people mean when they ask.
async fn upstream(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult<Json<Option<UpstreamView>>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_read(access, &owner, &name)?;

    let Some(parent_id) = repo.forked_from else {
        return Ok(Json(None));
    };

    let parent: Option<RepoWithOwner> = sqlx::query_as(
        "SELECT r.*, u.username FROM repos r JOIN users u ON u.id = r.owner_id WHERE r.id = $1",
    )
    .bind(parent_id)
    .fetch_optional(&state.db)
    .await?;
    // A parent that was deleted, or that this viewer cannot see, is simply no
    // upstream as far as this page is concerned.
    let Some(parent) = parent else { return Ok(Json(None)) };

    let (uid, admin, can_write) = match &viewer.user {
        Some(u) => (Some(u.id), u.is_admin, u.can_write),
        None => (None, false, false),
    };
    let pa = resolve(&state.db, &parent.repo, uid, admin, can_write, state.policy().require_auth)
        .await?;
    if !pa.can_read() {
        return Ok(Json(None));
    }

    let mine = ref_target(&state, &repo, &repo.default_branch).await?;
    let theirs = ref_target(&state, &parent.repo, &parent.repo.default_branch).await?;
    let (Some(mine), Some(theirs)) = (mine, theirs) else {
        return Ok(Json(None));
    };

    if mine == theirs {
        return Ok(Json(Some(UpstreamView {
            parent: format!("{}/{}", parent.username, parent.repo.name),
            branch: repo.default_branch.clone(),
            parent_branch: parent.repo.default_branch.clone(),
            ahead: 0,
            behind: 0,
            level: true,
        })));
    }

    // One store holds both sides, which is what makes this a local walk rather
    // than a fetch.
    let store = state
        .store_for_network(repo.network_id)
        .map_err(AppError::Internal)?;
    let c = crate::content::compare(
        &store,
        &parent.repo.default_branch,
        theirs,
        &repo.default_branch,
        mine,
    )?;

    Ok(Json(Some(UpstreamView {
        parent: format!("{}/{}", parent.username, parent.repo.name),
        branch: repo.default_branch.clone(),
        parent_branch: parent.repo.default_branch.clone(),
        ahead: c.ahead,
        behind: c.behind,
        level: false,
    })))
}

/// Which ref to remove.
///
/// The name travels in the body rather than the path because a branch may be
/// called `feature/thing`, and a slash in a path segment is a routing problem
/// nobody needs to have.
#[derive(Deserialize)]
struct DeleteRefIn {
    /// "branch" or "tag".
    kind: String,
    /// The bare name, exactly as [`RefView`] reports it.
    name: String,
}

/// Delete a branch or a tag.
///
/// Only the ref goes. The commits it pointed at are still in the store and
/// still reachable by hash — a name is a pointer here, and removing a pointer
/// is not a way to destroy history. Reclaiming unreferenced objects is a
/// separate, deliberate operation.
///
/// This is write access, not admin: someone who can create a branch by pushing
/// it should be able to tidy it up again.
async fn delete_ref(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<DeleteRefIn>,
) -> AppResult<Json<serde_json::Value>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_write(access)?;

    let bare = input.name.trim();
    if bare.is_empty() {
        return Err(AppError::BadRequest("no ref name given".into()));
    }

    let is_tag = match input.kind.as_str() {
        "tag" => true,
        "branch" => false,
        _ => return Err(AppError::BadRequest("kind must be \"branch\" or \"tag\"".into())),
    };
    let stored = if is_tag {
        format!("{}{bare}", fkit_core::session::TAG_PREFIX)
    } else {
        bare.to_string()
    };

    // The default branch is what a clone checks out and what every URL naming
    // no ref resolves to. Removing it leaves the repository pointing at
    // nothing, so it has to be reassigned first.
    if !is_tag && bare == repo.default_branch {
        return Err(AppError::Conflict(format!(
            "{bare} is the default branch — choose a different default first"
        )));
    }

    // A merge request stores its branches by name, not by foreign key, so
    // deleting one out from under an open request would leave a proposal whose
    // diff cannot be computed and whose merge cannot run.
    if !is_tag {
        let open: Option<(i32,)> = sqlx::query_as(
            "SELECT number FROM merge_requests
             WHERE repo_id = $1 AND state = 'open'
               AND (source_branch = $2 OR target_branch = $2)
             ORDER BY number
             LIMIT 1",
        )
        .bind(repo.id)
        .bind(bare)
        .fetch_optional(&state.db)
        .await?;

        if let Some((number,)) = open {
            return Err(AppError::Conflict(format!(
                "merge request #{number} is open on {bare} — merge or close it first"
            )));
        }
    }

    let done = sqlx::query("DELETE FROM refs WHERE repo_id = $1 AND name = $2")
        .bind(repo.id)
        .bind(&stored)
        .execute(&state.db)
        .await?;

    if done.rows_affected() == 0 {
        return Err(AppError::not_found(format!("no such {}: {bare}", input.kind)));
    }

    super::audit(
        &state,
        viewer.id(),
        Some(repo.id),
        "ref.delete",
        serde_json::json!({ "kind": input.kind, "name": bare }),
    )
    .await;

    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Serialize)]
pub struct RefView {
    /// The bare name: the `tags/` prefix is a namespace marker on the wire,
    /// not part of what the tag is called.
    pub name: String,
    pub target: String,
    pub short: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub is_default: bool,
    /// "branch" or "tag".
    pub kind: &'static str,
    /// The commit this ref points at, so a listing can say what was released
    /// without a request per row.
    pub head: Option<crate::models::HeadView>,
}

async fn list_refs(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult<Json<Vec<RefView>>> {
    let (repo, _, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    Ok(Json(refs_of(&state, &repo).await?))
}

pub async fn refs_of(state: &AppState, repo: &RepoRow) -> AppResult<Vec<RefView>> {
    let rows: Vec<(String, Vec<u8>, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT name, target, updated_at FROM refs WHERE repo_id = $1 ORDER BY name",
    )
    .bind(repo.id)
    .fetch_all(&state.db)
    .await?;

    let store = state.store_for_network(repo.network_id).ok();

    Ok(rows
        .into_iter()
        .filter_map(|(n, t, u)| {
            let h = Hash(t.try_into().ok()?);
            let (kind, name) = match n.strip_prefix(fkit_core::session::TAG_PREFIX) {
                Some(bare) => ("tag", bare.to_string()),
                None => ("branch", n),
            };
            Some(RefView {
                is_default: kind == "branch" && name == repo.default_branch,
                target: h.to_hex(),
                short: h.short(),
                updated_at: u,
                kind,
                head: store.as_ref().and_then(|s| head_view(s, h)),
                name,
            })
        })
        .collect())
}

/// Summarise the commit a ref points at. `None` if the object is missing or is
/// not a commit, which a listing should render as a ref with no detail rather
/// than as an error.
pub fn head_view(store: &fkit_core::Store, hash: Hash) -> Option<crate::models::HeadView> {
    let fkit_core::Object::Commit(c) = store.get(hash).ok()? else {
        return None;
    };
    let hex = hash.to_hex();
    Some(crate::models::HeadView {
        short: hex[..10].to_string(),
        commit: hex,
        summary: c.message.lines().next().unwrap_or_default().to_string(),
        author: c.author,
        timestamp: c.timestamp,
    })
}

async fn list_collaborators(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult<Json<Vec<CollaboratorView>>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_admin(access)?;

    let rows: Vec<CollaboratorView> = sqlx::query_as(
        "SELECT c.user_id, u.username, c.role, c.granted_at
         FROM collaborators c JOIN users u ON u.id = c.user_id
         WHERE c.repo_id = $1 ORDER BY u.username",
    )
    .bind(repo.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

async fn add_collaborator(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Json(body): Json<AddCollaboratorReq>,
) -> AppResult<impl IntoResponse> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_admin(access)?;

    if !matches!(body.role.as_str(), "read" | "write" | "admin") {
        return Err(AppError::bad("role must be 'read', 'write' or 'admin'"));
    }
    let target = body.username.trim().to_ascii_lowercase();

    let user: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM users WHERE username = $1")
        .bind(&target)
        .fetch_optional(&state.db)
        .await?;
    let (user_id,) = user.ok_or_else(|| AppError::not_found(format!("no such user: {target}")))?;

    if user_id == repo.owner_id {
        return Err(AppError::bad("the owner already has full access"));
    }

    sqlx::query(
        "INSERT INTO collaborators (repo_id, user_id, role, granted_by)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (repo_id, user_id) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind(repo.id)
    .bind(user_id)
    .bind(&body.role)
    .bind(viewer.id())
    .execute(&state.db)
    .await?;

    super::audit(&state, viewer.id(), Some(repo.id), "collaborator.add",
        serde_json::json!({ "username": target, "role": body.role })).await;

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "ok": true }))))
}

async fn remove_collaborator(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, username)): Path<(String, String, String)>,
) -> AppResult<Json<serde_json::Value>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_admin(access)?;

    let done = sqlx::query(
        "DELETE FROM collaborators c USING users u
         WHERE c.user_id = u.id AND c.repo_id = $1 AND u.username = $2",
    )
    .bind(repo.id)
    .bind(username.to_ascii_lowercase())
    .execute(&state.db)
    .await?;

    if done.rows_affected() == 0 {
        return Err(AppError::not_found("that user is not a collaborator"));
    }
    super::audit(&state, viewer.id(), Some(repo.id), "collaborator.remove",
        serde_json::json!({ "username": username })).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}
