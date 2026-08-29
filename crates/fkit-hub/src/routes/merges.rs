//! Merge requests: propose a branch, review the diff, merge it server-side.
//!
//! The request stores only the *proposal* — title, branches, state. Its diff and
//! mergeability are recomputed from the live refs on every view, so a request
//! can never show a stale picture of branches that have since moved.
//!
//! Merging happens on the server rather than by telling the user to run the CLI,
//! which means the ref update and the merge-commit creation share the same
//! transaction and the same fast-forward reasoning as a push.

use crate::auth::Viewer;
use crate::content;
use crate::error::{AppError, AppResult};
use crate::models::RepoRow;
use crate::perms::require_write;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use fkit_core::hash::Hash;
use fkit_core::object::{Commit, Object};
use fkit_core::store::Store;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/repos/{owner}/{name}/merges", get(list).post(create))
        .route("/repos/{owner}/{name}/merges/{number}", get(detail))
        .route("/repos/{owner}/{name}/merges/{number}/merge", post(do_merge))
        .route("/repos/{owner}/{name}/merges/{number}/close", post(close))
        .route("/repos/{owner}/{name}/merges/{number}/reopen", post(reopen))
}

/// Issue numbers a merge request says it closes.
///
/// GitHub's spelling, because it is the one people already type: "closes #4",
/// "fixes #12", "resolved #7", anywhere in the title or the description.
///
/// Hand-scanned rather than with a regex, to avoid a dependency for one
/// pattern this small. The rules are deliberately strict — the keyword must be
/// a whole word, the `#` must follow it within a space or two, and the number
/// must be digits to the end of the word — so that prose like "this does not
/// fix #4" is the only kind of false positive left, and that one needs a human
/// to read it anyway.
pub fn closes_issues(text: &str) -> Vec<i32> {
    const WORDS: [&str; 9] = [
        "close", "closes", "closed",
        "fix", "fixes", "fixed",
        "resolve", "resolves", "resolved",
    ];

    let lower = text.to_ascii_lowercase();
    let mut out: Vec<i32> = Vec::new();
    let mut words = lower.split_whitespace().peekable();

    while let Some(w) = words.next() {
        // Trim punctuation that commonly sits either side of the keyword.
        let key = w.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if !WORDS.contains(&key) {
            continue;
        }
        let Some(next) = words.peek() else { continue };
        let Some(digits) = next.trim_start_matches(['(', '[']).strip_prefix('#')
        else {
            continue;
        };
        let digits: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            continue;
        }
        if let Ok(n) = digits.parse::<i32>()
            && n > 0
            && !out.contains(&n)
        {
            out.push(n);
        }
    }
    out
}

#[derive(Debug, sqlx::FromRow)]
struct MrRow {
    id: Uuid,
    /// Set when the source branch lives in another repository of the network.
    source_repo: Option<String>,
    /// Which repository to read `source_branch` from. Equal to `repo_id` for
    /// an ordinary request.
    source_repo_id: Option<Uuid>,
    number: i32,
    title: String,
    description: Option<String>,
    source_branch: String,
    target_branch: String,
    state: String,
    merge_commit: Option<Vec<u8>>,
    merged_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    author: Option<String>,
}

#[derive(Serialize)]
pub struct MrView {
    pub number: i32,
    pub title: String,
    pub description: Option<String>,
    pub source_branch: String,
    /// `owner/name` when the source branch is in another fork; absent when it
    /// is in this repository, which is the ordinary case.
    pub source_repo: Option<String>,
    pub target_branch: String,
    pub state: String,
    pub author: Option<String>,
    pub merge_commit: Option<String>,
    pub merged_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Issues this request says it closes, which merging it will.
    pub closes: Vec<i32>,
    /// Filled after the row is fetched, the same way an issue's are.
    pub labels: Vec<super::issues::LabelView>,
}

impl From<MrRow> for MrView {
    fn from(r: MrRow) -> Self {
        // Computed before the fields are moved into the view below.
        let closes = closes_issues(&format!(
            "{} {}",
            r.title,
            r.description.as_deref().unwrap_or("")
        ));
        MrView {
            number: r.number,
            title: r.title,
            description: r.description,
            source_branch: r.source_branch,
            source_repo: r.source_repo,
            target_branch: r.target_branch,
            state: r.state,
            author: r.author,
            merge_commit: r
                .merge_commit
                .and_then(|b| b.try_into().ok())
                .map(|a: [u8; 32]| Hash(a).to_hex()),
            merged_at: r.merged_at,
            closes,
            labels: Vec::new(),
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

// sqlx 0.9 refuses SQL assembled with `format!` (its `SqlSafeStr` bound), which
// is the right default: it makes "just interpolate this bit" impossible to do
// by accident. Every query below is therefore a complete literal, and the one
// place that needs a variable filter expresses it as a bind parameter instead
// of string surgery.
const SELECT_BY_ID: &str = "\
    SELECT m.id, m.number, m.title, m.description, m.source_branch, m.target_branch,
           m.state, m.merge_commit, m.merged_at, m.created_at, m.updated_at,
           u.username AS author, m.source_repo_id,
           CASE WHEN m.source_repo_id IS NULL OR m.source_repo_id = m.repo_id
                THEN NULL ELSE su.username || '/' || sr.name END AS source_repo
      FROM merge_requests m
      LEFT JOIN users u ON u.id = m.author_id
      LEFT JOIN repos sr ON sr.id = m.source_repo_id
      LEFT JOIN users su ON su.id = sr.owner_id
     WHERE m.id = $1";

const SELECT_BY_NUMBER: &str = "\
    SELECT m.id, m.number, m.title, m.description, m.source_branch, m.target_branch,
           m.state, m.merge_commit, m.merged_at, m.created_at, m.updated_at,
           u.username AS author, m.source_repo_id,
           CASE WHEN m.source_repo_id IS NULL OR m.source_repo_id = m.repo_id
                THEN NULL ELSE su.username || '/' || sr.name END AS source_repo
      FROM merge_requests m
      LEFT JOIN users u ON u.id = m.author_id
      LEFT JOIN repos sr ON sr.id = m.source_repo_id
      LEFT JOIN users su ON su.id = sr.owner_id
     WHERE m.repo_id = $1 AND m.number = $2";

/// `$2 = 'all'` selects every state; anything else filters to it. One literal
/// query covers both cases.
const SELECT_LIST: &str = "\
    SELECT m.id, m.number, m.title, m.description, m.source_branch, m.target_branch,
           m.state, m.merge_commit, m.merged_at, m.created_at, m.updated_at,
           u.username AS author, m.source_repo_id,
           CASE WHEN m.source_repo_id IS NULL OR m.source_repo_id = m.repo_id
                THEN NULL ELSE su.username || '/' || sr.name END AS source_repo
      FROM merge_requests m
      LEFT JOIN users u ON u.id = m.author_id
      LEFT JOIN repos sr ON sr.id = m.source_repo_id
      LEFT JOIN users su ON su.id = sr.owner_id
     WHERE m.repo_id = $1 AND ($2::text = 'all' OR m.state = $2::text)
     ORDER BY m.number DESC
     LIMIT 200";

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    state: Option<String>,
}

async fn list(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Vec<MrView>>> {
    let (repo, _, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    let want = q.state.unwrap_or_else(|| "open".into());
    if !matches!(want.as_str(), "open" | "merged" | "closed" | "all") {
        return Err(AppError::bad("state must be open, merged, closed or all"));
    }

    let rows: Vec<MrRow> = sqlx::query_as(SELECT_LIST)
        .bind(repo.id)
        .bind(&want)
        .fetch_all(&state.db)
        .await?;
    // One query for every row's labels rather than one per row.
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let mut by: std::collections::HashMap<Uuid, Vec<super::issues::LabelView>> =
        std::collections::HashMap::new();
    let joined: Result<Vec<super::issues::MergeLabelRow>, _> = sqlx::query_as(
        "SELECT ml.merge_request_id, l.id, l.name, l.hue, l.description
           FROM merge_labels ml JOIN labels l ON l.id = ml.label_id
          WHERE ml.merge_request_id = ANY($1)
          ORDER BY lower(l.name)",
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await;
    if let Ok(joined) = joined {
        for (mr, id, name, hue, description) in joined {
            by.entry(mr)
                .or_default()
                .push(super::issues::LabelView { id, name, hue, description });
        }
    }

    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let id = r.id;
                let mut v = MrView::from(r);
                if let Some(ls) = by.remove(&id) {
                    v.labels = ls;
                }
                v
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateReq {
    title: String,
    #[serde(default)]
    description: Option<String>,
    source_branch: String,
    /// `owner/name` of the fork the source branch is in. Absent means this
    /// repository, which is the ordinary case.
    #[serde(default)]
    source_repo: Option<String>,
    target_branch: String,
}

async fn create(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Json(body): Json<CreateReq>,
) -> AppResult<impl IntoResponse> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    // Opening a request is a write to the repository's state, so it needs write
    // access. Read-only collaborators can view requests but not create them.
    // Proposing a change from a fork is not writing to the repository being
    // proposed to — it is the reason forks exist. So write access is required
    // only to move a branch that is already here; a request from elsewhere in
    // the network needs read on the target and read on the source, both of
    // which are checked below.
    if body.source_repo.is_none() {
        require_write(access)?;
    } else {
        crate::perms::require_read(access, &owner, &name)?;
    }
    let u = viewer.require()?;

    let title = body.title.trim();
    if title.is_empty() || title.len() > 200 {
        return Err(AppError::bad("title must be 1-200 characters"));
    }

    // Sequential per repository, allocated under a lock so two concurrent
    // creates cannot claim the same number.
    //
    // The lock is taken on the *repo* row: Postgres refuses `FOR UPDATE`
    // alongside an aggregate, and locking the parent row is what actually
    // serialises numbering for this repository anyway.
    // Where the source branch lives. A fork may propose its own branch into
    // its parent, which is the whole point of forking — and it works without
    // transferring anything, because both repositories read one store.
    let source_repo_id = match body.source_repo.as_deref() {
        None => repo.id,
        Some(spec) => {
            let (o, n) = spec
                .split_once('/')
                .ok_or_else(|| AppError::BadRequest("source_repo is owner/name".into()))?;
            let (src, src_access, _) = super::load_repo(&state, &viewer, o, n).await?;
            crate::perms::require_read(src_access, o, n)?;

            // Same network only. Two repositories that never shared a history
            // have no common ancestor and, more to the point, do not share a
            // store — the objects on one side would simply not be there.
            if src.network_id != repo.network_id {
                return Err(AppError::BadRequest(
                    "that repository is not a fork of this one".into(),
                ));
            }
            src.id
        }
    };

    // The source may be in another fork; the target is always here.
    if read_ref(&state, source_repo_id, &body.source_branch).await?.is_none() {
        return Err(AppError::not_found(format!(
            "no such branch: {}",
            body.source_branch
        )));
    }
    if read_ref(&state, repo.id, &body.target_branch).await?.is_none() {
        return Err(AppError::not_found(format!(
            "no such branch: {}",
            body.target_branch
        )));
    }
    // Merging a branch into itself is only a no-op when it is the same branch
    // of the same repository; a fork's `main` into its parent's `main` is a
    // perfectly ordinary request.
    if source_repo_id == repo.id && body.source_branch == body.target_branch {
        return Err(AppError::bad("a branch cannot be merged into itself"));
    }

    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT id FROM repos WHERE id = $1 FOR UPDATE")
        .bind(repo.id)
        .fetch_one(&mut *tx)
        .await?;
    let next = (super::next_number(&mut tx, repo.id).await?,);

    let id = Uuid::new_v4();
    let res = sqlx::query(
        "INSERT INTO merge_requests
            (id, repo_id, number, title, description, author_id, source_branch,
             target_branch, source_repo_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(repo.id)
    .bind(next.0)
    .bind(title)
    .bind(&body.description)
    .bind(u.id)
    .bind(&body.source_branch)
    .bind(&body.target_branch)
    .bind(source_repo_id)
    .execute(&mut *tx)
    .await;

    if let Err(sqlx::Error::Database(e)) = &res
        && e.is_unique_violation()
    {
        return Err(AppError::conflict(
            "an open merge request already proposes those branches",
        ));
    }
    res?;
    tx.commit().await?;

    super::audit(&state, Some(u.id), Some(repo.id), "merge_request.create",
        serde_json::json!({ "number": next.0, "source": body.source_branch, "target": body.target_branch })).await;

    let row: MrRow = sqlx::query_as(SELECT_BY_ID)
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok((StatusCode::CREATED, Json(MrView::from(row))))
}

#[derive(Serialize)]
struct MrDetail {
    #[serde(flatten)]
    request: MrView,
    /// Recomputed live, so it always reflects where the branches are now.
    comparison: Option<content::Comparison>,
    /// Whether the viewer may press merge.
    can_merge: bool,
}

async fn detail(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
) -> AppResult<Json<MrDetail>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    let row = load(&state, repo.id, number).await?;
    let store = state.store_for_network(repo.network_id).map_err(AppError::Internal)?;

    // A merged or closed request may reference branches that are gone; that is
    // not an error, the comparison is simply unavailable.
    // The source branch is read from the repository it lives in, which for a
    // request from a fork is not this one. Both resolve in the same store, so
    // the comparison itself needs nothing special.
    let src_id = row.source_repo_id.unwrap_or(repo.id);
    let comparison = match (
        read_ref(&state, repo.id, &row.target_branch).await?,
        read_ref(&state, src_id, &row.source_branch).await?,
    ) {
        (Some(base), Some(head)) => Some(content::compare(
            &store,
            &row.target_branch,
            base,
            &row.source_branch,
            head,
        )?),
        _ => None,
    };

    let can_merge = access.can_write() && row.state == "open";
    let id = row.id;
    let mut request = MrView::from(row);
    request.labels = super::issues::merge_labels_of(&state, id).await?;
    Ok(Json(MrDetail { request, comparison, can_merge }))
}

async fn load(state: &AppState, repo_id: Uuid, number: i32) -> AppResult<MrRow> {
    sqlx::query_as(SELECT_BY_NUMBER)
        .bind(repo_id)
        .bind(number)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::not_found(format!("no merge request #{number}")))
}

async fn read_ref(state: &AppState, repo_id: Uuid, branch: &str) -> AppResult<Option<Hash>> {
    let row: Option<(Vec<u8>,)> =
        sqlx::query_as("SELECT target FROM refs WHERE repo_id = $1 AND name = $2")
            .bind(repo_id)
            .bind(branch)
            .fetch_optional(&state.db)
            .await?;
    Ok(row.and_then(|(t,)| Some(Hash(t.try_into().ok()?))))
}

#[derive(Deserialize, Default)]
struct MergeReq {
    #[serde(default)]
    message: Option<String>,
}

/// Perform the merge and advance the target branch.
async fn do_merge(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
    body: Option<Json<MergeReq>>,
) -> AppResult<Json<MrView>> {
    use fkit_core::merge::{merge_base, merge_trees};

    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_write(access)?;
    let u = viewer.require()?;

    let row = load(&state, repo.id, number).await?;
    if row.state != "open" {
        return Err(AppError::conflict(format!(
            "this merge request is already {}",
            row.state
        )));
    }

    // A review comment is a question, and a merge request should not land
    // with questions outstanding. Only line comments count: a remark about
    // the change as a whole has nothing to be resolved against, so treating
    // it as a blocker would make "resolve" mean "acknowledge", which is a
    // different and much weaker thing.
    let (open_threads,): (i64,) = sqlx::query_as(
        "SELECT COUNT(DISTINCT (file_path, side, line, blob))
           FROM comments
          WHERE merge_request_id = $1 AND blob IS NOT NULL AND resolved_at IS NULL",
    )
    .bind(row.id)
    .fetch_one(&state.db)
    .await?;

    if open_threads > 0 {
        return Err(AppError::conflict(format!(
            "{open_threads} unresolved comment {} on this change — resolve {} before merging",
            if open_threads == 1 { "thread" } else { "threads" },
            if open_threads == 1 { "it" } else { "them" },
        )));
    }

    let target = read_ref(&state, repo.id, &row.target_branch)
        .await?
        .ok_or_else(|| AppError::conflict(format!("branch {} is gone", row.target_branch)))?;
    let source = read_ref(&state, row.source_repo_id.unwrap_or(repo.id), &row.source_branch)
        .await?
        .ok_or_else(|| AppError::conflict(format!("branch {} is gone", row.source_branch)))?;

    let store = state.store_for_network(repo.network_id).map_err(AppError::Internal)?;

    // Already contained: nothing to do but record it.
    if fkit_core::proto::is_ancestor(&store, source, target).map_err(AppError::Internal)? {
        return finish(&state, &repo, &row, u.id, target, "merged").await;
    }

    let mb = merge_base(&store, target, source).map_err(AppError::Internal)?;
    let base_tree = match mb.base {
        Some(b) => Some(commit_tree(&store, b)?),
        None => None,
    };

    // Fast-forward: our history is entirely contained in theirs, so there is
    // nothing to combine and no merge commit to write.
    let new_tip = if mb.base == Some(target) {
        source
    } else {
        let outcome = merge_trees(
            &store,
            base_tree,
            commit_tree(&store, target)?,
            commit_tree(&store, source)?,
        )
        .map_err(AppError::Internal)?;

        if !outcome.clean() {
            let paths: Vec<String> = outcome.conflicts.iter().map(|c| c.path.clone()).collect();
            return Err(AppError::conflict(format!(
                "cannot merge automatically — {} conflict(s): {}",
                paths.len(),
                paths.join(", ")
            )));
        }

        let message = body
            .and_then(|Json(b)| b.message)
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| {
                format!(
                    "merge {} into {} (#{})",
                    row.source_branch, row.target_branch, row.number
                )
            });

        let commit = Commit {
            tree: outcome.tree,
            // Order matters: first parent is the branch being merged *into*, so
            // first-parent history stays on the target branch.
            parents: vec![target, source],
            author: u.username.clone(),
            timestamp: fkit_core::repo::now_unix(),
            message,
        };
        let (id, _) = store.put(&Object::Commit(commit)).map_err(AppError::Internal)?;
        id
    };

    // Update the ref under the same check-and-set discipline as a push.
    let mut tx = state.db.begin().await?;
    let current: Option<(Vec<u8>,)> = sqlx::query_as(
        "SELECT target FROM refs WHERE repo_id = $1 AND name = $2 FOR UPDATE",
    )
    .bind(repo.id)
    .bind(&row.target_branch)
    .fetch_optional(&mut *tx)
    .await?;

    // Someone pushed while we were computing: refuse rather than clobber.
    if current.as_ref().and_then(|(b,)| b.clone().try_into().ok()).map(|a: [u8; 32]| Hash(a))
        != Some(target)
    {
        return Err(AppError::conflict(
            "the target branch moved while this merge was being prepared — reload and try again",
        ));
    }

    sqlx::query(
        "UPDATE refs SET target = $3, updated_at = now(), updated_by = $4
         WHERE repo_id = $1 AND name = $2",
    )
    .bind(repo.id)
    .bind(&row.target_branch)
    .bind(new_tip.0.to_vec())
    .bind(u.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    finish(&state, &repo, &row, u.id, new_tip, "merged").await
}

fn commit_tree(store: &Store, id: Hash) -> AppResult<Hash> {
    match store.get(id).map_err(AppError::Internal)? {
        Object::Commit(c) => Ok(c.tree),
        other => Err(AppError::bad(format!(
            "{} is a {}, not a commit",
            id.short(),
            other.kind().name()
        ))),
    }
}

async fn finish(
    state: &AppState,
    repo: &RepoRow,
    row: &MrRow,
    actor: Uuid,
    commit: Hash,
    new_state: &str,
) -> AppResult<Json<MrView>> {
    sqlx::query(
        "UPDATE merge_requests
         SET state = $2, merge_commit = $3, merged_at = now(), merged_by = $4, updated_at = now()
         WHERE id = $1",
    )
    .bind(row.id)
    .bind(new_state)
    .bind(commit.0.to_vec())
    .bind(actor)
    .execute(&state.db)
    .await?;

    sqlx::query("UPDATE repos SET updated_at = now() WHERE id = $1")
        .bind(repo.id)
        .execute(&state.db)
        .await?;

    // Close whatever the request said it closes, now that it has landed.
    //
    // After the merge rather than inside it: an issue left open because this
    // failed is a nuisance someone fixes in one click, whereas a merge rolled
    // back because an issue could not be closed is a far worse trade.
    let wants = closes_issues(&format!(
        "{} {}",
        row.title,
        row.description.as_deref().unwrap_or("")
    ));
    for n in &wants {
        let closed: Option<(Uuid,)> = sqlx::query_as(
            "UPDATE issues
                SET state = 'closed', closed_at = now(), closed_by = $3, updated_at = now()
              WHERE repo_id = $1 AND number = $2 AND state = 'open'
              RETURNING id",
        )
        .bind(repo.id)
        .bind(n)
        .bind(actor)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);

        // Say what closed it, on the issue, so the trail reads from either
        // end rather than only from the merge request.
        if let Some((issue_id,)) = closed {
            let _ = sqlx::query(
                "INSERT INTO comments (id, repo_id, issue_id, author_id, body)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(Uuid::new_v4())
            .bind(repo.id)
            .bind(issue_id)
            .bind(actor)
            .bind(format!("Closed by merge request #{}.", row.number))
            .execute(&state.db)
            .await;
        }
    }

    super::audit(state, Some(actor), Some(repo.id), "merge_request.merge",
        serde_json::json!({
            "number": row.number, "commit": commit.to_hex(), "closed": wants
        })).await;

    let updated: MrRow = sqlx::query_as(SELECT_BY_ID)
        .bind(row.id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(MrView::from(updated)))
}

async fn set_state(
    state: &AppState,
    viewer: &Viewer,
    owner: &str,
    name: &str,
    number: i32,
    to: &str,
    from: &str,
) -> AppResult<Json<MrView>> {
    let (repo, access, _) = super::load_repo(state, viewer, owner, name).await?;
    require_write(access)?;
    let row = load(state, repo.id, number).await?;
    if row.state != from {
        return Err(AppError::conflict(format!(
            "cannot {to} a request that is {}",
            row.state
        )));
    }
    sqlx::query("UPDATE merge_requests SET state = $2, updated_at = now() WHERE id = $1")
        .bind(row.id)
        .bind(to)
        .execute(&state.db)
        .await?;

    let updated: MrRow = sqlx::query_as(SELECT_BY_ID)
        .bind(row.id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(MrView::from(updated)))
}

async fn close(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
) -> AppResult<Json<MrView>> {
    set_state(&state, &viewer, &owner, &name, number, "closed", "open").await
}

async fn reopen(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i32)>,
) -> AppResult<Json<MrView>> {
    set_state(&state, &viewer, &owner, &name, number, "open", "closed").await
}

#[cfg(test)]
mod tests {
    use super::closes_issues;

    #[test]
    fn the_three_keywords_and_their_tenses_are_recognised() {
        for word in [
            "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves",
            "resolved",
        ] {
            assert_eq!(
                closes_issues(&format!("{word} #4")),
                vec![4],
                "{word} should link"
            );
        }
    }

    #[test]
    fn case_and_position_do_not_matter() {
        assert_eq!(closes_issues("Fixes #12"), vec![12]);
        assert_eq!(closes_issues("CLOSES #3"), vec![3]);
        assert_eq!(
            closes_issues("Rework the parser. Resolves #9 along the way."),
            vec![9]
        );
    }

    #[test]
    fn several_are_collected_and_repeats_are_not() {
        assert_eq!(closes_issues("closes #1, fixes #2, closes #1"), vec![1, 2]);
    }

    #[test]
    fn punctuation_around_the_reference_is_tolerated() {
        assert_eq!(closes_issues("(closes #7)"), vec![7]);
        assert_eq!(closes_issues("closes #7."), vec![7]);
        assert_eq!(closes_issues("- fixes #7"), vec![7]);
    }

    #[test]
    fn a_bare_reference_links_nothing() {
        // Mentioning an issue is not promising to close it.
        assert!(closes_issues("see #4 for background").is_empty());
        assert!(closes_issues("#4").is_empty());
    }

    #[test]
    fn the_keyword_must_be_a_whole_word() {
        // "prefixes" ends in "fixes" and means nothing of the sort.
        assert!(closes_issues("prefixes #4 are wrong").is_empty());
        assert!(closes_issues("unfixed #4").is_empty());
    }

    #[test]
    fn a_number_is_required() {
        assert!(closes_issues("fixes #").is_empty());
        assert!(closes_issues("fixes #abc").is_empty());
        assert!(closes_issues("fixes the parser").is_empty());
        assert!(closes_issues("fixes #0").is_empty());
    }

    #[test]
    fn the_keyword_and_the_number_must_be_adjacent() {
        // Otherwise any description containing both words anywhere would link.
        assert!(closes_issues("fixes the thing described in #4").is_empty());
    }
}
