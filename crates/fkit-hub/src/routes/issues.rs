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
        .route(
            "/repos/{owner}/{name}/merges/{number}/resolve",
            post(resolve_thread),
        )
        .route("/repos/{owner}/{name}/labels", get(list_labels).post(create_label))
        .route(
            "/repos/{owner}/{name}/labels/{id}",
            patch(edit_label).delete(delete_label),
        )
        .route(
            "/repos/{owner}/{name}/issues/{number}/labels",
            post(set_issue_labels),
        )
        .route(
            "/repos/{owner}/{name}/merges/{number}/labels",
            post(set_merge_labels),
        )
        .route("/repos/{owner}/{name}/n/{number}", get(what_is))
        .route("/repos/{owner}/{name}/issues/{number}/refs", get(issue_refs))
}

// ---- labels -------------------------------------------------------------

#[derive(sqlx::FromRow, Serialize)]
pub struct LabelView {
    pub id: Uuid,
    pub name: String,
    /// 0-359. The palette is derived from this against whichever theme is in
    /// use, so a label picked in the dark theme stays readable in the light.
    pub hue: i32,
    pub description: Option<String>,
}

async fn list_labels(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult<Json<Vec<LabelView>>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_read(access, &owner, &name)?;
    let rows: Vec<LabelView> = sqlx::query_as(
        "SELECT id, name, hue, description FROM labels WHERE repo_id = $1 ORDER BY lower(name)",
    )
    .bind(repo.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct LabelInput {
    name: Option<String>,
    hue: Option<i32>,
    description: Option<String>,
}

fn clean_label(name: &str) -> AppResult<String> {
    let n = name.trim();
    if n.is_empty() {
        return Err(AppError::BadRequest("a label needs a name".into()));
    }
    if n.chars().count() > 40 {
        return Err(AppError::BadRequest("that label name is too long".into()));
    }
    Ok(n.to_string())
}

/// Defining the vocabulary is an administrative act; applying it is not.
///
/// Anyone who can write may label an issue, because that is triage. Only an
/// administrator may invent a label, because a shared vocabulary stops being
/// one the moment everybody can add to it — which is how repositories end up
/// with "bug", "Bug", "bugs" and "defect".
async fn create_label(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Json(input): Json<LabelInput>,
) -> AppResult<impl IntoResponse> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    crate::perms::require_admin(access)?;
    let u = viewer.require()?;

    let label = clean_label(input.name.as_deref().unwrap_or(""))?;
    let hue = input.hue.unwrap_or(0).rem_euclid(360);

    let id = Uuid::new_v4();
    let res = sqlx::query(
        "INSERT INTO labels (id, repo_id, name, hue, description) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(repo.id)
    .bind(&label)
    .bind(hue)
    .bind(input.description.as_deref().map(str::trim).filter(|d| !d.is_empty()))
    .execute(&state.db)
    .await;

    if let Err(sqlx::Error::Database(e)) = &res
        && e.is_unique_violation()
    {
        return Err(AppError::conflict(format!("there is already a \"{label}\" label")));
    }
    res?;

    super::audit(&state, Some(u.id), Some(repo.id), "label.create",
        serde_json::json!({ "name": label })).await;

    let row: LabelView = sqlx::query_as("SELECT id, name, hue, description FROM labels WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok((StatusCode::CREATED, Json(row)))
}

async fn edit_label(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, id)): Path<(String, String, Uuid)>,
    Json(input): Json<LabelInput>,
) -> AppResult<Json<LabelView>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    crate::perms::require_admin(access)?;

    if let Some(n) = input.name.as_deref() {
        let n = clean_label(n)?;
        sqlx::query("UPDATE labels SET name = $3 WHERE id = $1 AND repo_id = $2")
            .bind(id)
            .bind(repo.id)
            .bind(n)
            .execute(&state.db)
            .await?;
    }
    if let Some(h) = input.hue {
        sqlx::query("UPDATE labels SET hue = $3 WHERE id = $1 AND repo_id = $2")
            .bind(id)
            .bind(repo.id)
            .bind(h.rem_euclid(360))
            .execute(&state.db)
            .await?;
    }
    if let Some(d) = input.description.as_deref() {
        sqlx::query("UPDATE labels SET description = $3 WHERE id = $1 AND repo_id = $2")
            .bind(id)
            .bind(repo.id)
            .bind(d.trim())
            .execute(&state.db)
            .await?;
    }

    let row: Option<LabelView> =
        sqlx::query_as("SELECT id, name, hue, description FROM labels WHERE id = $1 AND repo_id = $2")
            .bind(id)
            .bind(repo.id)
            .fetch_optional(&state.db)
            .await?;
    row.map(Json).ok_or_else(|| AppError::not_found("no such label"))
}

async fn delete_label(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, id)): Path<(String, String, Uuid)>,
) -> AppResult<Json<serde_json::Value>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    crate::perms::require_admin(access)?;

    // The rows on issues go with it: a label nobody can see is not a label.
    let done = sqlx::query("DELETE FROM labels WHERE id = $1 AND repo_id = $2")
        .bind(id)
        .bind(repo.id)
        .execute(&state.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::not_found("no such label"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct IssueLabels {
    /// The complete set, not a delta. A caller that sends what it believes the
    /// labels to be cannot half-apply a change it thought it made.
    labels: Vec<Uuid>,
}

async fn set_issue_labels(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
    Json(input): Json<IssueLabels>,
) -> AppResult<Json<Vec<LabelView>>> {
    let (repo_id, id, _) = load_issue(&state, &viewer, &owner, &name, number).await?;
    // Triage, not vocabulary: write access is enough.
    writer(&viewer)?;

    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM issue_labels WHERE issue_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;

    for label in &input.labels {
        // Scoped to the repository, so an id from elsewhere cannot be pasted
        // onto an issue here.
        sqlx::query(
            "INSERT INTO issue_labels (issue_id, label_id)
             SELECT $1, id FROM labels WHERE id = $2 AND repo_id = $3
             ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(label)
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE issues SET updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(Json(labels_of(&state, id).await?))
}

async fn set_merge_labels(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
    Json(input): Json<IssueLabels>,
) -> AppResult<Json<Vec<LabelView>>> {
    let (repo_id, id) = load_merge(&state, &viewer, &owner, &name, number).await?;
    writer(&viewer)?;

    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM merge_labels WHERE merge_request_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for label in &input.labels {
        sqlx::query(
            "INSERT INTO merge_labels (merge_request_id, label_id)
             SELECT $1, id FROM labels WHERE id = $2 AND repo_id = $3
             ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(label)
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE merge_requests SET updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(Json(merge_labels_of(&state, id).await?))
}

/// The labels on one merge request.
pub async fn merge_labels_of(state: &AppState, mr: Uuid) -> AppResult<Vec<LabelView>> {
    Ok(sqlx::query_as(
        "SELECT l.id, l.name, l.hue, l.description
           FROM merge_labels ml JOIN labels l ON l.id = ml.label_id
          WHERE ml.merge_request_id = $1
          ORDER BY lower(l.name)",
    )
    .bind(mr)
    .fetch_all(&state.db)
    .await?)
}

/// The labels on one issue.
async fn labels_of(state: &AppState, issue: Uuid) -> AppResult<Vec<LabelView>> {
    Ok(sqlx::query_as(
        "SELECT l.id, l.name, l.hue, l.description
           FROM issue_labels il JOIN labels l ON l.id = il.label_id
          WHERE il.issue_id = $1
          ORDER BY lower(l.name)",
    )
    .bind(issue)
    .fetch_all(&state.db)
    .await?)
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
    file_path: Option<String>,
    line_start: Option<i32>,
    line_end: Option<i32>,
    blob: Option<Vec<u8>>,
    ref_name: Option<String>,
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
    /// Filled in bulk after the rows are fetched, for the same reason.
    pub labels: Vec<LabelView>,
    /// The lines this issue was opened about, if it was opened from code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<CodeAnchor>,
}

/// Where an issue came from: an exact range of an exact file's content.
#[derive(Serialize)]
pub struct CodeAnchor {
    /// Where the file was when the issue was opened. Display only — content
    /// that moves keeps its hash, so the anchor survives a rename.
    pub file_path: String,
    pub line_start: i32,
    pub line_end: i32,
    /// The anchor proper. Names one byte sequence, forever.
    pub blob: String,
    /// What the author was reading. Display only; a branch moves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_name: Option<String>,
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
            labels: Vec::new(),
            // All four parts or none: the column constraint guarantees it, and
            // zipping them here means a half-anchor can never reach a client.
            anchor: match (r.file_path, r.line_start, r.line_end, r.blob) {
                (Some(file_path), Some(line_start), Some(line_end), Some(blob)) => {
                    Some(CodeAnchor {
                        file_path,
                        line_start,
                        line_end,
                        blob: hex(&blob),
                        ref_name: r.ref_name,
                    })
                }
                _ => None,
            },
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One row of the bulk label join: which issue, and the label's own columns.
type LabelRow = (i32, Uuid, String, i32, Option<String>);

/// The same, keyed by merge request id rather than issue number.
pub type MergeLabelRow = (Uuid, Uuid, String, i32, Option<String>);

/// Attach each issue's labels in one query rather than one per row.
async fn attach_labels(state: &AppState, views: &mut [IssueView], repo: Uuid) {
    if views.is_empty() {
        return;
    }
    let numbers: Vec<i32> = views.iter().map(|v| v.number).collect();
    let rows: Result<Vec<LabelRow>, _> = sqlx::query_as(
        "SELECT i.number, l.id, l.name, l.hue, l.description
           FROM issues i
           JOIN issue_labels il ON il.issue_id = i.id
           JOIN labels l ON l.id = il.label_id
          WHERE i.repo_id = $1 AND i.number = ANY($2)
          ORDER BY lower(l.name)",
    )
    .bind(repo)
    .bind(&numbers)
    .fetch_all(&state.db)
    .await;

    let Ok(rows) = rows else { return };
    let mut by: std::collections::HashMap<i32, Vec<LabelView>> = std::collections::HashMap::new();
    for (number, id, name, hue, description) in rows {
        by.entry(number).or_default().push(LabelView { id, name, hue, description });
    }
    for v in views.iter_mut() {
        if let Some(ls) = by.remove(&v.number) {
            v.labels = ls;
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
            "       i.file_path, i.line_start, i.line_end, i.blob, i.ref_name, ",
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
    /// Show only issues carrying this label, by name.
    label: Option<String>,
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
    let label = q.label.as_deref().map(str::trim).filter(|l| !l.is_empty());

    // A label filter is a different query rather than a clause appended to a
    // string: sqlx wants the SQL known at compile time, and four spelled-out
    // queries are easier to read than one built by concatenation anyway.
    if let Some(label) = label {
        let mut rows: Vec<IssueRow> = if want == "all" {
            sqlx::query_as(select_issue!(
                "WHERE i.repo_id = $1 AND EXISTS (
                     SELECT 1 FROM issue_labels il JOIN labels l ON l.id = il.label_id
                      WHERE il.issue_id = i.id AND lower(l.name) = lower($2))
                 ORDER BY i.number DESC LIMIT 200"
            ))
            .bind(repo.id)
            .bind(label)
            .fetch_all(&state.db)
            .await?
        } else {
            sqlx::query_as(select_issue!(
                "WHERE i.repo_id = $1 AND i.state = $3 AND EXISTS (
                     SELECT 1 FROM issue_labels il JOIN labels l ON l.id = il.label_id
                      WHERE il.issue_id = i.id AND lower(l.name) = lower($2))
                 ORDER BY i.number DESC LIMIT 200"
            ))
            .bind(repo.id)
            .bind(label)
            .bind(want)
            .fetch_all(&state.db)
            .await?
        };
        let mut views: Vec<IssueView> =
            rows.drain(..).map(IssueView::from).collect();
        attach_labels(&state, &mut views, repo.id).await;
        return Ok(Json(views));
    }

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

    let mut views: Vec<IssueView> = rows.into_iter().map(IssueView::from).collect();
    attach_labels(&state, &mut views, repo.id).await;
    Ok(Json(views))
}

#[derive(Deserialize)]
struct CreateIssue {
    title: String,
    body: Option<String>,
    /// Opened from a selection while reading a file.
    #[serde(default)]
    anchor: Option<NewAnchor>,
}

#[derive(Deserialize)]
struct NewAnchor {
    file_path: String,
    line_start: i32,
    line_end: i32,
    /// The blob the lines were read from, as hex.
    blob: String,
    #[serde(default)]
    ref_name: Option<String>,
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

    // Validated before anything is written, so a bad anchor cannot leave an
    // issue that exists but cannot be rendered.
    let anchor = match &input.anchor {
        None => None,
        Some(a) => {
            let path = a.file_path.trim();
            if path.is_empty() {
                return Err(AppError::BadRequest("an anchor needs a file".into()));
            }
            if a.line_start < 1 || a.line_end < a.line_start {
                return Err(AppError::BadRequest(
                    "an anchor's lines must run forwards from one".into(),
                ));
            }
            // The hash is the anchor, so it has to be one.
            let blob = fkit_core::Hash::from_hex(a.blob.trim())
                .ok_or_else(|| AppError::BadRequest("that is not a blob hash".into()))?;

            // And it has to be content this repository actually holds —
            // otherwise the issue points at bytes nobody can show.
            let store =
                state.store_for_network(repo.network_id).map_err(AppError::Internal)?;
            if !store.has(blob) {
                return Err(AppError::BadRequest(
                    "that blob is not in this repository".into(),
                ));
            }
            Some((
                path.to_string(),
                a.line_start,
                a.line_end,
                blob.0.to_vec(),
                a.ref_name.as_deref().map(str::trim).filter(|r| !r.is_empty()).map(String::from),
            ))
        }
    };

    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT id FROM repos WHERE id = $1 FOR UPDATE")
        .bind(repo.id)
        .fetch_one(&mut *tx)
        .await?;
    let number = super::next_number(&mut tx, repo.id).await?;

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO issues
             (id, repo_id, number, title, body, author_id,
              file_path, line_start, line_end, blob, ref_name)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(id)
    .bind(repo.id)
    .bind(number)
    .bind(title)
    .bind(input.body.as_deref().map(str::trim).filter(|b| !b.is_empty()))
    .bind(u.id)
    .bind(anchor.as_ref().map(|a| a.0.as_str()))
    .bind(anchor.as_ref().map(|a| a.1))
    .bind(anchor.as_ref().map(|a| a.2))
    .bind(anchor.as_ref().map(|a| a.3.clone()))
    .bind(anchor.as_ref().and_then(|a| a.4.clone()))
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
    let (repo_id, id, row) = load_issue(&state, &viewer, &owner, &name, number).await?;
    let mut view = IssueView::from(row);
    view.labels = labels_of(&state, id).await?;
    let _ = repo_id;
    Ok(Json(view))
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

/// What `#4` is.
///
/// Issues and merge requests share one counter so that a number names one
/// thing; this is the lookup that turns the number back into which thing. It
/// exists so a `#4` written in a comment can be a link without whoever wrote
/// it having to know or say which kind they meant.
async fn what_is(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
) -> AppResult<Json<serde_json::Value>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_read(access, &owner, &name)?;

    let issue: Option<(String,)> =
        sqlx::query_as("SELECT title FROM issues WHERE repo_id = $1 AND number = $2")
            .bind(repo.id)
            .bind(number)
            .fetch_optional(&state.db)
            .await?;
    if let Some((title,)) = issue {
        return Ok(Json(
            serde_json::json!({ "kind": "issue", "number": number, "title": title }),
        ));
    }

    let mr: Option<(String,)> =
        sqlx::query_as("SELECT title FROM merge_requests WHERE repo_id = $1 AND number = $2")
            .bind(repo.id)
            .bind(number)
            .fetch_optional(&state.db)
            .await?;
    if let Some((title,)) = mr {
        return Ok(Json(
            serde_json::json!({ "kind": "merge", "number": number, "title": title }),
        ));
    }

    Err(AppError::not_found(format!("nothing is #{number} here")))
}

#[derive(Serialize)]
pub struct RefView {
    pub kind: &'static str,
    pub number: i32,
    pub title: String,
    pub state: String,
    pub author: Option<String>,
}

/// What mentions this issue.
///
/// The link a person cares about most is the merge request that will close
/// it, but any mention is worth showing: reading an issue and not knowing a
/// change already proposes to fix it is how the same work gets done twice.
///
/// Matched with a word-boundary regex rather than LIKE, so `#4` does not also
/// match `#41`.
async fn issue_refs(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
) -> AppResult<Json<Vec<RefView>>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_read(access, &owner, &name)?;

    let pattern = format!("(^|[^0-9a-zA-Z_])#{number}([^0-9]|$)");

    let rows: Vec<(String, i32, String, String, Option<String>)> = sqlx::query_as(
        "SELECT 'merge', m.number, m.title, m.state, u.username
           FROM merge_requests m
           LEFT JOIN users u ON u.id = m.author_id
          WHERE m.repo_id = $1
            AND (m.title ~ $2 OR COALESCE(m.description, '') ~ $2
                 OR EXISTS (SELECT 1 FROM comments c
                             WHERE c.merge_request_id = m.id AND c.body ~ $2))
          UNION ALL
         SELECT 'issue', i.number, i.title, i.state, u.username
           FROM issues i
           LEFT JOIN users u ON u.id = i.author_id
          WHERE i.repo_id = $1 AND i.number <> $3
            AND (i.title ~ $2 OR COALESCE(i.body, '') ~ $2
                 OR EXISTS (SELECT 1 FROM comments c
                             WHERE c.issue_id = i.id AND c.body ~ $2))
          ORDER BY 2",
    )
    .bind(repo.id)
    .bind(&pattern)
    .bind(number)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|(kind, number, title, state, author)| RefView {
                kind: if kind == "merge" { "merge" } else { "issue" },
                number,
                title,
                state,
                author,
            })
            .collect(),
    ))
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
    resolved_at: Option<DateTime<Utc>>,
    resolver: Option<String>,
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
    /// Set once someone has said this has been dealt with. An open merge
    /// request will not merge while any line comment is still unresolved.
    pub resolved_at: Option<DateTime<Utc>>,
    pub resolver: Option<String>,
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
            resolved_at: r.resolved_at,
            resolver: r.resolver,
        }
    }
}

macro_rules! select_comment {
    ($tail:literal) => {
        concat!(
            "SELECT c.id, u.username AS author, c.body, c.file_path, c.line, c.side, ",
            "       c.blob, c.created_at, c.edited_at, c.resolved_at, ",
            "       ru.username AS resolver ",
            "  FROM comments c ",
            "  LEFT JOIN users u ON u.id = c.author_id ",
            "  LEFT JOIN users ru ON ru.id = c.resolved_by ",
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
struct ResolveThread {
    file_path: String,
    line: i32,
    side: String,
    /// Hex hash identifying which version of the file the thread is on.
    blob: String,
    /// False to reopen it.
    resolved: bool,
}

/// Mark every comment on one line of one version of one file resolved.
///
/// A thread is not a row here — it is every comment sharing an anchor — so
/// this updates the set rather than a parent. Anyone who can read the
/// repository and is signed in may resolve: in practice the person who
/// answered a question is as often the one who closes it as the one who asked.
async fn resolve_thread(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
    Json(input): Json<ResolveThread>,
) -> AppResult<Json<serde_json::Value>> {
    let (_, mr) = load_merge(&state, &viewer, &owner, &name, number).await?;
    let u = writer(&viewer)?;

    let blob = Hash::from_hex(&input.blob)
        .ok_or_else(|| AppError::BadRequest("blob is not a hash".into()))?;
    if input.side != "old" && input.side != "new" {
        return Err(AppError::BadRequest("side must be \"old\" or \"new\"".into()));
    }

    let done = sqlx::query(
        "UPDATE comments
            SET resolved_at = CASE WHEN $5 THEN now() ELSE NULL END,
                resolved_by = CASE WHEN $5 THEN $6 ELSE NULL END,
                updated_at = now()
          WHERE merge_request_id = $1
            AND file_path = $2 AND line = $3 AND side = $4
            AND blob = $7",
    )
    .bind(mr)
    .bind(&input.file_path)
    .bind(input.line)
    .bind(&input.side)
    .bind(input.resolved)
    .bind(u.id)
    .bind(blob.0.to_vec())
    .execute(&state.db)
    .await?;

    if done.rows_affected() == 0 {
        return Err(AppError::not_found("no thread there"));
    }
    Ok(Json(serde_json::json!({ "ok": true, "resolved": input.resolved })))
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
