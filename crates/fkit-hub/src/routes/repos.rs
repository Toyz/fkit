//! Repository listing, creation, settings, collaborators, and branch refs.

use crate::auth::Viewer;
use crate::error::{AppError, AppResult};
use crate::models::*;
use crate::perms::{require_admin, resolve};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, patch};
use axum::{Json, Router};
use fkit_core::hash::Hash;
use serde::Serialize;
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/repos", get(list_repos).post(create_repo))
        .route("/users/{username}", get(get_profile))
        .route("/repos/{owner}/{name}", get(get_repo))
        .route("/repos/{owner}/{name}", patch(update_repo))
        .route("/repos/{owner}/{name}", delete(delete_repo))
        .route("/repos/{owner}/{name}/refs", get(list_refs))
        .route("/repos/{owner}/{name}/stats", get(repo_stats))
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
    let store = state.store_for(repo.id).map_err(AppError::Internal)?;

    let dir = state.data_dir.join("repos").join(repo.id.to_string()).join("objects");
    let bytes = dir_size(&dir);
    let objects = store.iter_ids().map(|v| v.len()).unwrap_or(0);

    // Commits only, so this stays a walk of the history rather than of every
    // chunk in the repository.
    let mut commits = 0usize;
    if let Some(tip) = ref_target(&state, &repo, &repo.default_branch).await? {
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

    Ok(Json(RepoStats { commits, objects, bytes }))
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
    Ok(Json(super::repo_view(&repo, &owner_lc, access)))
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
    let _ = std::fs::remove_dir_all(state.repo_path(repo.id));

    super::audit(&state, viewer.id(), None, "repo.delete",
        serde_json::json!({ "name": format!("{owner}/{name}") })).await;
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

    let store = state.store_for(repo.id).ok();

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
