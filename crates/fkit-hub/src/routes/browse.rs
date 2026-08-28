//! Read-only repository browsing: trees, blobs, history, and diffs.
//!
//! Every handler resolves a *ref* (branch name or commit hash) to a commit, then
//! works from that commit's tree. Resolution is centralised in [`resolve_ref`]
//! so a branch name can never be mistaken for a hash, or vice versa.

use crate::auth::Viewer;
use crate::content;
use crate::error::{AppError, AppResult};
use crate::models::RepoRow;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use fkit_core::hash::Hash;
use fkit_core::store::Store;
use serde::{Deserialize, Serialize};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/repos/{owner}/{name}/tree/{ref}", get(tree_root))
        .route("/repos/{owner}/{name}/tree/{ref}/{*path}", get(tree_path))
        .route("/repos/{owner}/{name}/blob/{ref}/{*path}", get(blob))
        .route("/repos/{owner}/{name}/commits/{ref}", get(commits))
        .route("/repos/{owner}/{name}/commit/{hash}", get(commit_detail))
        .route("/repos/{owner}/{name}/readme/{ref}", get(readme))
        .route("/repos/{owner}/{name}/lastcommits/{ref}", get(last_commits_root))
        .route("/repos/{owner}/{name}/lastcommits/{ref}/{*path}", get(last_commits_path))
        .route("/repos/{owner}/{name}/raw/{ref}/{*path}", get(raw))
        .route("/repos/{owner}/{name}/patch/{hash}", get(patch))
        .route("/repos/{owner}/{name}/compare/{base}/{head}", get(compare))
}

/// Resolve a branch name or hex commit hash to a commit hash.
///
/// Branch names are tried first: if someone names a branch after a valid hex
/// string, the branch is what they meant.
async fn resolve_ref(state: &AppState, repo: &RepoRow, spec: &str) -> AppResult<Hash> {
    let row: Option<(Vec<u8>,)> =
        sqlx::query_as("SELECT target FROM refs WHERE repo_id = $1 AND name = $2")
            .bind(repo.id)
            .bind(spec)
            .fetch_optional(&state.db)
            .await?;

    if let Some((bytes,)) = row {
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| AppError::Internal(anyhow::anyhow!("corrupt ref target")))?;
        return Ok(Hash(arr));
    }

    Hash::from_hex(spec)
        .ok_or_else(|| AppError::not_found(format!("no such branch or commit: {spec}")))
}

/// Open the store and resolve the ref — the common preamble of every handler.
async fn open(
    state: &AppState,
    viewer: &Viewer,
    owner: &str,
    name: &str,
    spec: &str,
) -> AppResult<(Store, Hash, Hash)> {
    let (repo, _, _) = super::load_repo(state, viewer, owner, name).await?;
    let commit_id = resolve_ref(state, &repo, spec).await?;
    let store = state.store_for(repo.id).map_err(AppError::Internal)?;
    let commit = content::commit_of(&store, commit_id)?;
    Ok((store, commit_id, commit.tree))
}

#[derive(Serialize)]
struct TreeResponse {
    path: String,
    commit: String,
    entries: Vec<content::EntryView>,
}

async fn tree_root(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r)): Path<(String, String, String)>,
) -> AppResult<Json<TreeResponse>> {
    let (store, commit, tree) = open(&state, &viewer, &owner, &name, &r).await?;
    Ok(Json(TreeResponse {
        entries: content::list_dir(&store, tree, "")?,
        path: String::new(),
        commit: commit.to_hex(),
    }))
}

async fn tree_path(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r, path)): Path<(String, String, String, String)>,
) -> AppResult<Json<TreeResponse>> {
    let (store, commit, tree) = open(&state, &viewer, &owner, &name, &r).await?;
    Ok(Json(TreeResponse {
        entries: content::list_dir(&store, tree, &path)?,
        path,
        commit: commit.to_hex(),
    }))
}

#[derive(Serialize)]
struct BlobResponse {
    path: String,
    hash: String,
    size: u64,
    binary: bool,
    truncated: bool,
    /// Absent for binary or oversized files.
    content: Option<String>,
    lines: usize,
}

async fn blob(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r, path)): Path<(String, String, String, String)>,
) -> AppResult<Json<BlobResponse>> {
    let (store, _, tree) = open(&state, &viewer, &owner, &name, &r).await?;
    let b = content::read_blob(&store, tree, &path)?;

    let text = if b.binary || b.truncated {
        None
    } else {
        String::from_utf8(b.bytes.clone()).ok()
    };

    Ok(Json(BlobResponse {
        path,
        hash: b.hash.to_hex(),
        size: b.size,
        binary: b.binary || text.is_none(),
        truncated: b.truncated,
        lines: text.as_deref().map(|t| t.lines().count()).unwrap_or(0),
        content: text,
    }))
}

#[derive(Deserialize)]
struct Page {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    skip: usize,
}
fn default_limit() -> usize {
    50
}

async fn commits(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r)): Path<(String, String, String)>,
    Query(page): Query<Page>,
) -> AppResult<Json<Vec<content::CommitView>>> {
    let (store, commit, _) = open(&state, &viewer, &owner, &name, &r).await?;
    let limit = page.limit.clamp(1, 200);
    Ok(Json(content::history(&store, commit, limit, page.skip)?))
}

#[derive(Serialize)]
struct CommitDetail {
    #[serde(flatten)]
    commit: content::CommitView,
    changes: Vec<content::ChangeView>,
}

async fn commit_detail(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, hash)): Path<(String, String, String)>,
) -> AppResult<Json<CommitDetail>> {
    let (repo, _, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    let store = state.store_for(repo.id).map_err(AppError::Internal)?;
    let id = Hash::from_hex(&hash).ok_or_else(|| AppError::bad("not a valid commit hash"))?;
    let c = content::commit_of(&store, id)?;

    Ok(Json(CommitDetail {
        commit: content::to_view(id, &c),
        changes: content::commit_diff(&store, id)?,
    }))
}

/// How far back to look for the commit that last touched each entry.
const LAST_COMMIT_SCAN: usize = 500;

async fn last_commits_root(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r)): Path<(String, String, String)>,
) -> AppResult<Json<std::collections::HashMap<String, content::LastCommit>>> {
    let (store, commit, _) = open(&state, &viewer, &owner, &name, &r).await?;
    Ok(Json(content::last_commits(&store, commit, "", LAST_COMMIT_SCAN)?))
}

async fn last_commits_path(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r, path)): Path<(String, String, String, String)>,
) -> AppResult<Json<std::collections::HashMap<String, content::LastCommit>>> {
    let (store, commit, _) = open(&state, &viewer, &owner, &name, &r).await?;
    Ok(Json(content::last_commits(&store, commit, &path, LAST_COMMIT_SCAN)?))
}

/// Raw file bytes.
///
/// Always served as `text/plain` (or an opaque octet-stream) with `nosniff` and
/// a restrictive CSP, **never** with the file's apparent type. Repository
/// content is attacker-controlled: handing back a pushed `.html` as
/// `text/html` on this origin would execute it with the viewer's session
/// cookie. GitHub serves raw content from a separate domain for exactly this
/// reason; we do not have one, so the headers have to carry the weight.
async fn raw(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r, path)): Path<(String, String, String, String)>,
) -> AppResult<axum::response::Response> {
    use axum::http::header;
    use axum::response::IntoResponse;

    let (store, _, tree) = open(&state, &viewer, &owner, &name, &r).await?;
    let (bytes, _size) = content::raw_blob(&store, tree, &path)?;

    let is_text = std::str::from_utf8(&bytes).is_ok()
        && !bytes.iter().take(8192).any(|b| *b == 0);
    let ctype = if is_text {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    };

    Ok((
        [
            (header::CONTENT_TYPE, ctype),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; sandbox; style-src 'unsafe-inline'",
            ),
            (header::CACHE_CONTROL, "max-age=300, private"),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Serialize)]
struct PatchResponse {
    files: Vec<content::FileDiff>,
    /// More files changed than were diffed.
    truncated: bool,
}

async fn patch(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, hash)): Path<(String, String, String)>,
) -> AppResult<Json<PatchResponse>> {
    let (repo, _, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    let store = state.store_for(repo.id).map_err(AppError::Internal)?;
    let id = Hash::from_hex(&hash).ok_or_else(|| AppError::bad("not a valid commit hash"))?;
    let (files, truncated) = content::commit_patch(&store, id)?;
    Ok(Json(PatchResponse { files, truncated }))
}

/// Compare two refs — the merge preview.
async fn compare(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, base, head)): Path<(String, String, String, String)>,
) -> AppResult<Json<content::Comparison>> {
    let (repo, _, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    let store = state.store_for(repo.id).map_err(AppError::Internal)?;

    let base_id = resolve_ref(&state, &repo, &base).await?;
    let head_id = resolve_ref(&state, &repo, &head).await?;

    Ok(Json(content::compare(&store, &base, base_id, &head, head_id)?))
}

#[derive(Serialize)]
struct ReadmeResponse {
    name: String,
    content: String,
}

async fn readme(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r)): Path<(String, String, String)>,
) -> AppResult<Json<Option<ReadmeResponse>>> {
    let (store, _, tree) = open(&state, &viewer, &owner, &name, &r).await?;
    Ok(Json(
        content::find_readme(&store, tree).map(|(n, c)| ReadmeResponse { name: n, content: c }),
    ))
}
