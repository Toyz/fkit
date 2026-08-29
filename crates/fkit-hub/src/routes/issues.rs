//! Issues, and the comments that both they and merge requests carry.
//!
//! Comments live here rather than in their own module because they are the
//! same rows either way: the only thing that differs is which column names
//! the subject, and splitting that across two files would mean two of every
//! query.

use crate::auth::Viewer;
use crate::error::{AppError, AppResult};
use crate::perms::require_read;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use fkit_core::hash::Hash;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/repos/{owner}/{name}/issues", get(list).post(create))
        .route("/repos/{owner}/{name}/issues/{number}", get(detail).patch(edit))
        .route("/repos/{owner}/{name}/issues/{number}/close", post(close))
        .route("/repos/{owner}/{name}/issues/{number}/reopen", post(reopen))
        .route(
            "/repos/{owner}/{name}/issues/{number}/comments",
            get(issue_comments).post(comment_on_issue),
        )
        .route(
            "/repos/{owner}/{name}/merges/{number}/comments",
            get(merge_comments).post(comment_on_merge),
        )
        .route(
            "/repos/{owner}/{name}/comments/{id}",
            patch(edit_comment).delete(delete_comment),
        )
}

// ---- issues -------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct IssueRow {
    number: i32,
    title: String,
    body: Option<String>,
    state: String,
    author: Option<String>,
    closed_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    comments: i64,
}

#[derive(Serialize)]
pub struct IssueView {
    pub number: i32,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub author: Option<String>,
    pub closed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Shown in the list, so it does not take a second request per row.
    pub comments: i64,
}

impl From<IssueRow> for IssueView {
    fn from(r: IssueRow) -> Self {
        IssueView {
            number: r.number,
            title: r.title,
            body: r.body,
            state: r.state,
            author: r.author,
            closed_at: r.closed_at,
            created_at: r.created_at,
            updated_at: r.updated_at,
            comments: r.comments,
        }
    }
}

/// The issue projection, plus whatever clause the caller needs.
///
/// A macro rather than a `const` joined with `format!` because sqlx wants a
/// `&'static str`: a query built at runtime is the shape SQL injection takes,
/// so the type system refuses it and the shared prefix has to be concatenated
/// at compile time instead.
macro_rules! select_issue {
    ($tail:literal) => {
        concat!(
            "SELECT i.number, i.title, i.body, i.state, u.username AS author, ",
            "       i.closed_at, i.created_at, i.updated_at, ",
            "       (SELECT COUNT(*) FROM comments c WHERE c.issue_id = i.id) AS comments ",
            "  FROM issues i ",
            "  LEFT JOIN users u ON u.id = i.author_id ",
            $tail
        )
    };
}

#[derive(Deserialize)]
struct ListQuery {
    /// "open" (default), "closed", or "all".
    state: Option<String>,
}

async fn list(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Vec<IssueView>>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_read(access, &owner, &name)?;

    let want = q.state.as_deref().unwrap_or("open");
    let rows: Vec<IssueRow> = if want == "all" {
        sqlx::query_as(select_issue!(
            "WHERE i.repo_id = $1 ORDER BY i.number DESC LIMIT 200"
        ))
        .bind(repo.id)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as(select_issue!(
            "WHERE i.repo_id = $1 AND i.state = $2 ORDER BY i.number DESC LIMIT 200"
        ))
        .bind(repo.id)
        .bind(want)
        .fetch_all(&state.db)
        .await?
    };

    Ok(Json(rows.into_iter().map(IssueView::from).collect()))
}

#[derive(Deserialize)]
struct CreateIssue {
    title: String,
    body: Option<String>,
}

async fn create(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<CreateIssue>,
) -> AppResult<impl IntoResponse> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_read(access, &owner, &name)?;
    // Reading is not enough to write something down; anyone who can see the
    // repository and is signed in may open an issue, which is the point of
    // issues.
    let u = viewer.require()?;
    if !u.can_write {
        return Err(AppError::Forbidden("this token is read-only".into()));
    }

    let title = input.title.trim();
    if title.is_empty() {
        return Err(AppError::BadRequest("an issue needs a title".into()));
    }
    if title.chars().count() > 300 {
        return Err(AppError::BadRequest("that title is too long".into()));
    }

    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT id FROM repos WHERE id = $1 FOR UPDATE")
        .bind(repo.id)
        .fetch_one(&mut *tx)
        .await?;
    let number = super::next_number(&mut tx, repo.id).await?;

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO issues (id, repo_id, number, title, body, author_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(repo.id)
    .bind(number)
    .bind(title)
    .bind(input.body.as_deref().map(str::trim).filter(|b| !b.is_empty()))
    .bind(u.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    super::audit(&state, Some(u.id), Some(repo.id), "issue.create",
        serde_json::json!({ "number": number })).await;

    let row: IssueRow = sqlx::query_as(select_issue!("WHERE i.id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok((StatusCode::CREATED, Json(IssueView::from(row))))
}

/// Load one issue, checking the viewer may see the repository at all.
async fn load_issue(
    state: &AppState,
    viewer: &Viewer,
    owner: &str,
    name: &str,
    number: i32,
) -> AppResult<(Uuid, Uuid, IssueRow)> {
    let (repo, access, _) = super::load_repo(state, viewer, owner, name).await?;
    require_read(access, owner, name)?;

    let id: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM issues WHERE repo_id = $1 AND number = $2")
            .bind(repo.id)
            .bind(number)
            .fetch_optional(&state.db)
            .await?;
    let Some((id,)) = id else {
        return Err(AppError::not_found(format!("no issue #{number}")));
    };

    let row: IssueRow = sqlx::query_as(select_issue!("WHERE i.id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok((repo.id, id, row))
}

async fn detail(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
) -> AppResult<Json<IssueView>> {
    let (_, _, row) = load_issue(&state, &viewer, &owner, &name, number).await?;
    Ok(Json(IssueView::from(row)))
}

#[derive(Deserialize)]
struct EditIssue {
    title: Option<String>,
    body: Option<String>,
}

async fn edit(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
    Json(input): Json<EditIssue>,
) -> AppResult<Json<IssueView>> {
    let (repo_id, id, _) = load_issue(&state, &viewer, &owner, &name, number).await?;
    let u = viewer.require()?;
    can_change_issue(&state, id, repo_id, u.id, u.is_admin).await?;

    if let Some(t) = input.title.as_deref() {
        let t = t.trim();
        if t.is_empty() {
            return Err(AppError::BadRequest("an issue needs a title".into()));
        }
        sqlx::query("UPDATE issues SET title = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(t)
            .execute(&state.db)
            .await?;
    }
    if let Some(b) = input.body.as_deref() {
        sqlx::query("UPDATE issues SET body = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(b.trim())
            .execute(&state.db)
            .await?;
    }

    let row: IssueRow = sqlx::query_as(select_issue!("WHERE i.id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(IssueView::from(row)))
}

/// Whoever wrote it, or whoever runs the repository.
async fn can_change_issue(
    state: &AppState,
    issue_id: Uuid,
    repo_id: Uuid,
    user: Uuid,
    site_admin: bool,
) -> AppResult<()> {
    let author: Option<(Option<Uuid>,)> =
        sqlx::query_as("SELECT author_id FROM issues WHERE id = $1")
            .bind(issue_id)
            .fetch_optional(&state.db)
            .await?;
    if author.and_then(|(a,)| a) == Some(user) || site_admin {
        return Ok(());
    }
    // Otherwise it takes write access to the repository.
    let can: Option<(String,)> = sqlx::query_as(
        "SELECT role FROM collaborators WHERE repo_id = $1 AND user_id = $2",
    )
    .bind(repo_id)
    .bind(user)
    .fetch_optional(&state.db)
    .await?;
    let owner: Option<(Uuid,)> = sqlx::query_as("SELECT owner_id FROM repos WHERE id = $1")
        .bind(repo_id)
        .fetch_optional(&state.db)
        .await?;

    let is_owner = owner.map(|(o,)| o) == Some(user);
    let has_write = matches!(can.as_deref_role(), Some("write") | Some("admin"));
    if is_owner || has_write {
        Ok(())
    } else {
        Err(AppError::Forbidden("that is not yours to change".into()))
    }
}

/// Small helper so the match above reads as one line.
trait RoleOpt {
    fn as_deref_role(&self) -> Option<&str>;
}
impl RoleOpt for Option<(String,)> {
    fn as_deref_role(&self) -> Option<&str> {
        self.as_ref().map(|(r,)| r.as_str())
    }
}

async fn set_state(
    state: &AppState,
    viewer: &Viewer,
    owner: &str,
    name: &str,
    number: i32,
    open: bool,
) -> AppResult<Json<IssueView>> {
    let (repo_id, id, _) = load_issue(state, viewer, owner, name, number).await?;
    let u = viewer.require()?;
    can_change_issue(state, id, repo_id, u.id, u.is_admin).await?;

    sqlx::query(
        "UPDATE issues
            SET state = $2,
                closed_at = CASE WHEN $2 = 'closed' THEN now() ELSE NULL END,
                closed_by = CASE WHEN $2 = 'closed' THEN $3 ELSE NULL END,
                updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(if open { "open" } else { "closed" })
    .bind(u.id)
    .execute(&state.db)
    .await?;

    let row: IssueRow = sqlx::query_as(select_issue!("WHERE i.id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(IssueView::from(row)))
}

async fn close(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
) -> AppResult<Json<IssueView>> {
    set_state(&state, &viewer, &owner, &name, number, false).await
}

async fn reopen(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
) -> AppResult<Json<IssueView>> {
    set_state(&state, &viewer, &owner, &name, number, true).await
}

// ---- comments -----------------------------------------------------------

#[derive(sqlx::FromRow)]
struct CommentRow {
    id: Uuid,
    author: Option<String>,
    body: String,
    file_path: Option<String>,
    line: Option<i32>,
    side: Option<String>,
    blob: Option<Vec<u8>>,
    created_at: DateTime<Utc>,
    edited_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct CommentView {
    pub id: Uuid,
    pub author: Option<String>,
    pub body: String,
    /// All four present together, or all absent on a conversation comment.
    pub file_path: Option<String>,
    pub line: Option<i32>,
    pub side: Option<String>,
    /// The hash of the file the comment was written against. The diff view
    /// matches this against what it is rendering; no match means the file has
    /// changed since, and the comment is shown as outdated rather than moved.
    pub blob: Option<String>,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
}

impl From<CommentRow> for CommentView {
    fn from(r: CommentRow) -> Self {
        CommentView {
            id: r.id,
            author: r.author,
            body: r.body,
            file_path: r.file_path,
            line: r.line,
            side: r.side,
            blob: r.blob.and_then(|b| <[u8; 32]>::try_from(b).ok()).map(|a| Hash(a).to_hex()),
            created_at: r.created_at,
            edited_at: r.edited_at,
        }
    }
}

macro_rules! select_comment {
    ($tail:literal) => {
        concat!(
            "SELECT c.id, u.username AS author, c.body, c.file_path, c.line, c.side, ",
            "       c.blob, c.created_at, c.edited_at ",
            "  FROM comments c ",
            "  LEFT JOIN users u ON u.id = c.author_id ",
            $tail
        )
    };
}

async fn issue_comments(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
) -> AppResult<Json<Vec<CommentView>>> {
    let (_, id, _) = load_issue(&state, &viewer, &owner, &name, number).await?;
    let rows: Vec<CommentRow> =
        sqlx::query_as(select_comment!("WHERE c.issue_id = $1 ORDER BY c.created_at"))
            .bind(id)
            .fetch_all(&state.db)
            .await?;
    Ok(Json(rows.into_iter().map(CommentView::from).collect()))
}

/// Resolve a merge request number to its id, checking read access.
async fn load_merge(
    state: &AppState,
    viewer: &Viewer,
    owner: &str,
    name: &str,
    number: i32,
) -> AppResult<(Uuid, Uuid)> {
    let (repo, access, _) = super::load_repo(state, viewer, owner, name).await?;
    require_read(access, owner, name)?;
    let id: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM merge_requests WHERE repo_id = $1 AND number = $2")
            .bind(repo.id)
            .bind(number)
            .fetch_optional(&state.db)
            .await?;
    let Some((id,)) = id else {
        return Err(AppError::not_found(format!("no merge request #{number}")));
    };
    Ok((repo.id, id))
}

async fn merge_comments(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
) -> AppResult<Json<Vec<CommentView>>> {
    let (_, id) = load_merge(&state, &viewer, &owner, &name, number).await?;
    let rows: Vec<CommentRow> = sqlx::query_as(select_comment!(
        "WHERE c.merge_request_id = $1 ORDER BY c.created_at"
    ))
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows.into_iter().map(CommentView::from).collect()))
}

#[derive(Deserialize)]
struct NewComment {
    body: String,
    /// A line anchor. All four together, or none of them.
    file_path: Option<String>,
    line: Option<i32>,
    /// "old" or "new".
    side: Option<String>,
    /// Hex hash of the file the line belongs to.
    blob: Option<String>,
    /// Hex hash of the commit the author was looking at. Display only.
    commit: Option<String>,
}

/// The parts of an anchor, once they have been checked for completeness.
struct Anchor {
    file_path: String,
    line: i32,
    side: String,
    blob: Vec<u8>,
    commit: Option<Vec<u8>>,
}

fn check_anchor(input: &NewComment) -> AppResult<Option<Anchor>> {
    let parts = [
        input.file_path.is_some(),
        input.line.is_some(),
        input.side.is_some(),
        input.blob.is_some(),
    ];
    if parts.iter().all(|p| !p) {
        return Ok(None);
    }
    if !parts.iter().all(|p| *p) {
        return Err(AppError::BadRequest(
            "a line comment needs file_path, line, side and blob together".into(),
        ));
    }

    let side = input.side.clone().unwrap();
    if side != "old" && side != "new" {
        return Err(AppError::BadRequest("side must be \"old\" or \"new\"".into()));
    }
    let line = input.line.unwrap();
    if line < 1 {
        return Err(AppError::BadRequest("line numbers start at 1".into()));
    }
    let blob = Hash::from_hex(input.blob.as_deref().unwrap())
        .ok_or_else(|| AppError::BadRequest("blob is not a hash".into()))?;

    Ok(Some(Anchor {
        file_path: input.file_path.clone().unwrap(),
        line,
        side,
        blob: blob.0.to_vec(),
        commit: input
            .commit
            .as_deref()
            .and_then(Hash::from_hex)
            .map(|h| h.0.to_vec()),
    }))
}

async fn insert_comment(
    state: &AppState,
    repo_id: Uuid,
    issue: Option<Uuid>,
    merge: Option<Uuid>,
    author: Uuid,
    input: NewComment,
) -> AppResult<Json<CommentView>> {
    let body = input.body.trim().to_string();
    if body.is_empty() {
        return Err(AppError::BadRequest("a comment needs something in it".into()));
    }
    if body.len() > 64 * 1024 {
        return Err(AppError::BadRequest("that comment is too long".into()));
    }

    let anchor = check_anchor(&input)?;
    if anchor.is_some() && merge.is_none() {
        return Err(AppError::BadRequest(
            "only a merge request has code to comment on".into(),
        ));
    }

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO comments
            (id, repo_id, issue_id, merge_request_id, author_id, body,
             file_path, line, side, blob, commit_hash)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(id)
    .bind(repo_id)
    .bind(issue)
    .bind(merge)
    .bind(author)
    .bind(&body)
    .bind(anchor.as_ref().map(|a| a.file_path.as_str()))
    .bind(anchor.as_ref().map(|a| a.line))
    .bind(anchor.as_ref().map(|a| a.side.as_str()))
    .bind(anchor.as_ref().map(|a| a.blob.clone()))
    .bind(anchor.as_ref().and_then(|a| a.commit.clone()))
    .execute(&state.db)
    .await?;

    // The subject's own timestamp moves, so a list ordered by activity is
    // ordered by activity rather than by when someone last edited the title.
    if let Some(i) = issue {
        let _ = sqlx::query("UPDATE issues SET updated_at = now() WHERE id = $1")
            .bind(i)
            .execute(&state.db)
            .await;
    }
    if let Some(m) = merge {
        let _ = sqlx::query("UPDATE merge_requests SET updated_at = now() WHERE id = $1")
            .bind(m)
            .execute(&state.db)
            .await;
    }

    let row: CommentRow = sqlx::query_as(select_comment!("WHERE c.id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(CommentView::from(row)))
}

/// Signed in, not read-only, and able to see the repository.
fn writer(viewer: &Viewer) -> AppResult<&crate::auth::ViewerUser> {
    let u = viewer.require()?;
    if !u.can_write {
        return Err(AppError::Forbidden("this token is read-only".into()));
    }
    Ok(u)
}

async fn comment_on_issue(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
    Json(input): Json<NewComment>,
) -> AppResult<impl IntoResponse> {
    let (repo_id, id, _) = load_issue(&state, &viewer, &owner, &name, number).await?;
    let u = writer(&viewer)?;
    let out = insert_comment(&state, repo_id, Some(id), None, u.id, input).await?;
    Ok((StatusCode::CREATED, out))
}

async fn comment_on_merge(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
    Json(input): Json<NewComment>,
) -> AppResult<impl IntoResponse> {
    let (repo_id, id) = load_merge(&state, &viewer, &owner, &name, number).await?;
    let u = writer(&viewer)?;
    let out = insert_comment(&state, repo_id, None, Some(id), u.id, input).await?;
    Ok((StatusCode::CREATED, out))
}

#[derive(Deserialize)]
struct EditComment {
    body: String,
}

/// A comment is its author's. An administrator may remove one, but not rewrite
/// it: editing someone's words under their name is not a moderation power.
async fn own_comment(
    state: &AppState,
    viewer: &Viewer,
    owner: &str,
    name: &str,
    id: Uuid,
    allow_admin: bool,
) -> AppResult<Uuid> {
    let (repo, access, _) = super::load_repo(state, viewer, owner, name).await?;
    require_read(access, owner, name)?;
    let u = viewer.require()?;

    let row: Option<(Option<Uuid>,)> =
        sqlx::query_as("SELECT author_id FROM comments WHERE id = $1 AND repo_id = $2")
            .bind(id)
            .bind(repo.id)
            .fetch_optional(&state.db)
            .await?;
    let Some((author,)) = row else {
        return Err(AppError::not_found("no such comment"));
    };

    if author == Some(u.id) || (allow_admin && (u.is_admin || access.can_admin())) {
        Ok(id)
    } else {
        Err(AppError::Forbidden("that comment is not yours".into()))
    }
}

async fn edit_comment(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, id)): Path<(String, String, Uuid)>,
    Json(input): Json<EditComment>,
) -> AppResult<Json<CommentView>> {
    let id = own_comment(&state, &viewer, &owner, &name, id, false).await?;
    let body = input.body.trim();
    if body.is_empty() {
        return Err(AppError::BadRequest("a comment needs something in it".into()));
    }

    sqlx::query("UPDATE comments SET body = $2, edited_at = now(), updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(body)
        .execute(&state.db)
        .await?;

    let row: CommentRow = sqlx::query_as(select_comment!("WHERE c.id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(CommentView::from(row)))
}

async fn delete_comment(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, id)): Path<(String, String, Uuid)>,
) -> AppResult<Json<serde_json::Value>> {
    let id = own_comment(&state, &viewer, &owner, &name, id, true).await?;
    sqlx::query("DELETE FROM comments WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
