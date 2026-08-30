//! Repository listing, creation, settings, collaborators, and branch refs.

use crate::auth::Viewer;
use crate::error::{AppError, AppResult};
use crate::models::*;
use crate::perms::{require_admin, require_read, require_write, resolve};
use crate::perms::Access;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use fkit_core::hash::Hash;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A query over one person's repositories, carrying the rule for which of them
/// this viewer may be shown.
///
/// The rule is written once, here, and pasted between the caller's own two
/// halves at compile time. It has to be a macro rather than a constant because
/// sqlx will not accept a statement built at run time -- `SqlSafeStr` is only
/// implemented for literals, which is a guardrail worth keeping, and `concat!`
/// will not expand another macro to reach one. So the whole statement comes
/// out of this expansion as a single literal.
///
/// `$2` is the viewer. This composes a statement; it never carries a value
/// into one, and every value is still bound.
///
/// It filters for speed and for honest counts. It is not the authority:
/// `resolve` still runs per row before any of them is returned.
macro_rules! owned_sql {
    ($pre:literal, $post:literal) => {
        concat!(
            $pre,
            "r.visibility = 'public'
             OR r.owner_id = $2
             OR EXISTS (SELECT 1 FROM collaborators c
                        WHERE c.repo_id = r.id AND c.user_id = $2)",
            $post
        )
    };
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/repos", get(list_repos).post(create_repo))
        .route("/users/{username}", get(get_profile))
        .route("/users/{username}/activity", get(get_activity))
        .route("/users/{username}/pushes", get(get_pushes))
        .route("/users/{username}/repos", get(list_user_repos))
        .route("/repos/{owner}/{name}", get(get_repo))
        .route("/repos/{owner}/{name}", patch(update_repo))
        .route("/repos/{owner}/{name}", delete(delete_repo))
        .route("/repos/{owner}/{name}/refs", get(list_refs).delete(delete_ref))
        .route("/repos/{owner}/{name}/stashes", get(list_stashes).post(create_stash))
        .route("/repos/{owner}/{name}/stashes/{id}", delete(delete_stash))
        .route("/repos/{owner}/{name}/rules", get(list_rules).post(create_rule))
        .route("/repos/{owner}/{name}/rules/{id}", patch(update_rule).delete(delete_rule))
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
/// Every repository the viewer may see, newest first, a page at a time.
///
/// Searching happens here rather than in the browser. The old listing sent two
/// hundred rows and let the page filter them, which meant a filter that could
/// not find the two hundred and first repository and quietly said there was
/// nothing to find.
async fn list_repos(
    State(state): State<AppState>,
    viewer: Viewer,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<RepoPage>> {
    let vid = viewer.id();
    let limit = q.limit();
    let pattern = q.pattern();
    let cursor = q.cursor.as_deref().and_then(Cursor::decode);

    // One statement with optional predicates rather than a string built per
    // request: sqlx will not take a formatted query, and this way there is one
    // plan and one place where the visibility rule is written.
    let rows: Vec<RepoWithOwner> = sqlx::query_as(
        "SELECT r.*, u.username FROM repos r
         JOIN users u ON u.id = r.owner_id
         WHERE (r.visibility = 'public'
            OR r.owner_id = $1
            OR EXISTS (SELECT 1 FROM collaborators c
                       WHERE c.repo_id = r.id AND c.user_id = $1))
           AND ($2::text IS NULL
                OR r.name ILIKE $2 ESCAPE '\'
                OR u.username ILIKE $2 ESCAPE '\')
           AND ($3::timestamptz IS NULL OR (r.updated_at, r.id) < ($3, $4))
         ORDER BY r.updated_at DESC, r.id DESC
         LIMIT $5",
    )
    .bind(vid)
    .bind(pattern.as_deref())
    .bind(cursor.map(|c| c.updated_at))
    .bind(cursor.map(|c| c.id))
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;

    let (total,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM repos r
         JOIN users u ON u.id = r.owner_id
         WHERE (r.visibility = 'public'
            OR r.owner_id = $1
            OR EXISTS (SELECT 1 FROM collaborators c
                       WHERE c.repo_id = r.id AND c.user_id = $1))
           AND ($2::text IS NULL
                OR r.name ILIKE $2 ESCAPE '\'
                OR u.username ILIKE $2 ESCAPE '\')",
    )
    .bind(vid)
    .bind(pattern.as_deref())
    .fetch_one(&state.db)
    .await?;

    let out = page_of(&state, &viewer, rows, limit, total).await?;
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

// ---- stashes -------------------------------------------------------------

/// What this account has parked here.
///
/// Read access is enough. Losing write access to a repository should not strand
/// work you already put on it — you can still fetch it back and drop it.
async fn list_stashes(
    State(state): State<AppState>,
    viewer: Viewer,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
) -> AppResult<Json<Vec<crate::stash::Stash>>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_read(access, &owner, &name)?;
    let me = viewer.require()?;
    Ok(Json(crate::stash::list(&state.db, me.id, repo.id).await?))
}

#[derive(serde::Deserialize)]
struct NewStash {
    /// The stash commit, already pushed into the store.
    commit: String,
    message: String,
    /// What the objects cost, for the quota. The server recomputes nothing
    /// here; over-reporting only costs the pusher their own allowance.
    #[serde(default)]
    bytes: i64,
}

/// Register a stash whose objects are already in the store.
///
/// Write access, because parking one adds objects to somebody's repository.
/// The base is read from the commit rather than taken on trust: it is the
/// commit's own first parent, which is what makes the diff `base..commit` and
/// what `stash pop` merges against.
async fn create_stash(
    State(state): State<AppState>,
    viewer: Viewer,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    Json(body): Json<NewStash>,
) -> AppResult<Json<crate::stash::Stash>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_write(access)?;
    let me = viewer.require()?;

    let id = Hash::from_hex(&body.commit)
        .ok_or_else(|| AppError::bad("not a valid commit hash"))?;
    let store = state.store_for_network(repo.network_id).map_err(AppError::Internal)?;

    let fkit_core::Object::Commit(c) = store
        .get(id)
        .map_err(|_| AppError::bad("push the stash's objects before registering it"))?
    else {
        return Err(AppError::bad("that hash does not name a commit"));
    };
    let base = *c
        .parents
        .first()
        .ok_or_else(|| AppError::bad("a stash must have the commit it was taken from as a parent"))?;

    let message = body.message.trim();
    let message = if message.is_empty() { "work in progress" } else { message };

    // Tidy while we are here. A lapsed stash is already invisible and already
    // not a root; this just stops the rows accumulating.
    if let Err(e) = crate::stash::sweep(&state.db).await {
        tracing::warn!("sweeping expired stashes: {e}");
    }

    let row = crate::stash::create(
        &state.db,
        me.id,
        repo.id,
        id,
        base,
        message,
        body.bytes.max(0),
        crate::stash::DEFAULT_DAYS,
    )
    .await?;

    super::audit(&state, Some(me.id), Some(repo.id), "stash.push",
        serde_json::json!({ "commit": body.commit })).await;
    Ok(Json(row))
}

async fn delete_stash(
    State(state): State<AppState>,
    viewer: Viewer,
    axum::extract::Path((owner, name, id)): axum::extract::Path<(String, String, Uuid)>,
) -> AppResult<Json<serde_json::Value>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_read(access, &owner, &name)?;
    let me = viewer.require()?;
    crate::stash::drop_one(&state.db, me.id, repo.id, id).await?;
    super::audit(&state, Some(me.id), Some(repo.id), "stash.drop",
        serde_json::json!({ "id": id })).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---- branch rules --------------------------------------------------------

/// Anyone who can read the repository can see what its branches allow.
///
/// A rule is not a secret — it is the reason a push will be refused, and
/// finding that out at push time rather than beforehand is the whole
/// frustration this feature exists to remove.
async fn list_rules(
    State(state): State<AppState>,
    viewer: Viewer,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
) -> AppResult<Json<Vec<crate::rules::BranchRule>>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_read(access, &owner, &name)?;
    Ok(Json(crate::rules::for_repo(&state.db, repo.id).await?))
}

#[derive(serde::Deserialize)]
struct NewRule {
    pattern: String,
    #[serde(default = "yes")]
    no_force: bool,
    #[serde(default = "yes")]
    no_delete: bool,
}

fn yes() -> bool {
    true
}

async fn create_rule(
    State(state): State<AppState>,
    viewer: Viewer,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    Json(body): Json<NewRule>,
) -> AppResult<Json<crate::rules::BranchRule>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_admin(access)?;

    let pattern = body.pattern.trim().to_string();
    if pattern.is_empty() {
        return Err(AppError::bad("a rule needs a branch name or pattern"));
    }
    // `*` is only meaningful at the end, and a pattern that silently means
    // something other than it looks like is worse than no rule at all.
    if pattern.trim_end_matches('*').contains('*') {
        return Err(AppError::bad(
            "a pattern may only end in `*` — `main`, or `release/*`",
        ));
    }
    if pattern.starts_with(fkit_core::session::TAG_PREFIX) {
        return Err(AppError::bad("branch rules govern branches, not tags"));
    }
    if !body.no_force && !body.no_delete {
        return Err(AppError::bad("a rule that forbids nothing has no effect"));
    }

    let id = Uuid::new_v4();
    let done = sqlx::query(
        "INSERT INTO branch_rules (id, repo_id, pattern, no_force, no_delete)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (repo_id, pattern) DO NOTHING",
    )
    .bind(id)
    .bind(repo.id)
    .bind(&pattern)
    .bind(body.no_force)
    .bind(body.no_delete)
    .execute(&state.db)
    .await?;

    if done.rows_affected() == 0 {
        return Err(AppError::Conflict(format!(
            "there is already a rule for `{pattern}`"
        )));
    }

    super::audit(
        &state,
        viewer.id(),
        Some(repo.id),
        "rule.create",
        serde_json::json!({
            "pattern": pattern, "no_force": body.no_force, "no_delete": body.no_delete
        }),
    )
    .await;

    let row = sqlx::query_as(
        "SELECT id, pattern, no_force, no_delete, created_at FROM branch_rules WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

#[derive(serde::Deserialize)]
struct RulePatch {
    #[serde(default)]
    no_force: Option<bool>,
    #[serde(default)]
    no_delete: Option<bool>,
}

/// Turn one limit of a rule on or off.
///
/// The two are separate decisions — forbidding a rewrite of a release branch
/// while still allowing it to be retired is a coherent policy — so they are
/// toggled rather than fixed at creation.
async fn update_rule(
    State(state): State<AppState>,
    viewer: Viewer,
    axum::extract::Path((owner, name, id)): axum::extract::Path<(String, String, Uuid)>,
    Json(body): Json<RulePatch>,
) -> AppResult<Json<crate::rules::BranchRule>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_admin(access)?;

    // Read, decide, then write. The first version applied the update and then
    // deleted the row when the result forbade nothing — so a request that came
    // back 400 had already destroyed the rule it was refusing to empty. A
    // rejected request must leave everything exactly as it was.
    let current: Option<crate::rules::BranchRule> = sqlx::query_as(
        "SELECT id, pattern, no_force, no_delete, created_at
           FROM branch_rules WHERE id = $1 AND repo_id = $2",
    )
    .bind(id)
    .bind(repo.id)
    .fetch_optional(&state.db)
    .await?;

    let current = current.ok_or_else(|| AppError::not_found("no such rule"))?;
    let no_force = body.no_force.unwrap_or(current.no_force);
    let no_delete = body.no_delete.unwrap_or(current.no_delete);

    // A rule forbidding nothing looks like protection and is not, which is
    // worse than no rule at all — but that is a reason to refuse, not to
    // silently remove what is there.
    if !no_force && !no_delete {
        return Err(AppError::bad(
            "a rule must forbid something — turn the other one on, or remove the rule",
        ));
    }

    let row: crate::rules::BranchRule = sqlx::query_as(
        "UPDATE branch_rules SET no_force = $3, no_delete = $4
          WHERE id = $1 AND repo_id = $2
          RETURNING id, pattern, no_force, no_delete, created_at",
    )
    .bind(id)
    .bind(repo.id)
    .bind(no_force)
    .bind(no_delete)
    .fetch_one(&state.db)
    .await?;

    super::audit(
        &state,
        viewer.id(),
        Some(repo.id),
        "rule.update",
        serde_json::json!({
            "pattern": row.pattern, "no_force": row.no_force, "no_delete": row.no_delete
        }),
    )
    .await;
    Ok(Json(row))
}

async fn delete_rule(
    State(state): State<AppState>,
    viewer: Viewer,
    axum::extract::Path((owner, name, id)): axum::extract::Path<(String, String, Uuid)>,
) -> AppResult<Json<serde_json::Value>> {
    let (repo, access, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    require_admin(access)?;

    // Scoped to the repository, so a rule id from elsewhere is simply absent.
    let done = sqlx::query("DELETE FROM branch_rules WHERE id = $1 AND repo_id = $2")
        .bind(id)
        .bind(repo.id)
        .execute(&state.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::not_found("no such rule"));
    }

    super::audit(&state, viewer.id(), Some(repo.id), "rule.delete",
        serde_json::json!({ "id": id })).await;
    Ok(Json(serde_json::json!({ "ok": true })))
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
    /// The first page, so a profile is one request in the ordinary case.
    /// Further pages, and any search, come from `/users/{name}/repos`.
    repos: Vec<crate::models::RepoView>,
    /// The cursor for the page after `repos`, absent when there is none.
    next: Option<String>,
    /// How many this viewer may see in total, and how many of those are
    /// private. Counted rather than taken from `repos.len()`, which was only
    /// ever the size of the page and read as the size of the account.
    repo_count: i64,
    private_count: i64,
    /// What they work on, ranked over everything the viewer may see rather
    /// than over whichever repositories happened to fit on the first page.
    topics: Vec<String>,
    /// The most recent push to anything with a commit in it.
    last_push: Option<LastPush>,
    /// Whether the year graph will have anything in it.
    ///
    /// Here, on the profile, rather than left for the activity request to
    /// answer, because the page lays itself out around it: the identity band
    /// is one column or two depending on whether there is a year to put
    /// beside it. Waiting for a second request to find out means laying the
    /// band out one way and then the other, in front of the reader, on every
    /// profile that has nothing -- which on a new server is all of them.
    ///
    /// It counts private work like the graph does, since the graph counts it
    /// too. What it never does is say whose.
    has_activity: bool,
}

/// The tip of whatever this person pushed to most recently.
///
/// Server-side because the page cannot work it out any more: it used to scan
/// the repository list it had been given, which was the first page, so an
/// account with more repositories than fit would report the newest of those
/// rather than the newest of all.
#[derive(Debug, Serialize)]
struct LastPush {
    repo: String,
    at: chrono::DateTime<chrono::Utc>,
    commit: String,
    short: String,
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

    let page = owned_page(&state, &viewer, owner.id, &ListQuery::default()).await?;

    // Counted over everything the viewer may see, not over the page. These are
    // the numbers the profile prints, and taking them from the page meant an
    // account with a thousand repositories reported thirty.
    let vid = viewer.id();
    let (repo_count, private_count): (i64, i64) = sqlx::query_as(owned_sql!(
        "SELECT count(*),
                count(*) FILTER (WHERE r.visibility = 'private')
           FROM repos r
          WHERE r.owner_id = $1 AND (",
        ")"
    ))
    .bind(owner.id)
    .bind(vid)
    .fetch_one(&state.db)
    .await?;

    // Ranked across the lot for the same reason.
    let topics: Vec<(String,)> = sqlx::query_as(owned_sql!(
        "SELECT t FROM repos r, unnest(r.topics) AS t
          WHERE r.owner_id = $1 AND (",
        ")
          GROUP BY t
          ORDER BY count(*) DESC, t ASC
          LIMIT 6"
    ))
    .bind(owner.id)
    .bind(vid)
    .fetch_all(&state.db)
    .await?;

    // The newest push, over everything rather than over the page, and only
    // from a repository that has a commit -- ranking by `updated_at` alone
    // reported the creation of an empty repository as a push and then had no
    // tip to show for it.
    let last: Option<(String, chrono::DateTime<chrono::Utc>, Vec<u8>)> =
        sqlx::query_as(owned_sql!(
            "SELECT r.name, r.updated_at, f.target
               FROM repos r
               JOIN refs f ON f.repo_id = r.id AND f.name = r.default_branch
              WHERE r.owner_id = $1 AND (",
            ")
              ORDER BY r.updated_at DESC
              LIMIT 1"
        ))
    .bind(owner.id)
    .bind(vid)
    .fetch_optional(&state.db)
    .await?;

    let last_push = last.and_then(|(name, at, target)| {
        let bytes = <[u8; 32]>::try_from(target.as_slice()).ok()?;
        let hex = Hash(bytes).to_hex();
        Some(LastPush { repo: name, at, short: hex[..10].to_string(), commit: hex })
    });

    let (has_activity,): (bool,) = sqlx::query_as(
        "SELECT EXISTS(
                  SELECT 1 FROM commit_authors ca
                   WHERE ca.user_id = $1
                     AND ca.repo_id IS NOT NULL
                     AND LEAST(COALESCE(ca.committed_at, ca.pushed_at), ca.pushed_at)
                           >= now() - interval '1 year')",
    )
    .bind(owner.id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(Profile {
        username: owner.username,
        display_name: owner.display_name,
        is_admin: owner.is_admin,
        created_at: owner.created_at,
        repos: page.items,
        next: page.next,
        repo_count,
        private_count,
        topics: topics.into_iter().map(|(t,)| t).collect(),
        last_push,
        has_activity,
    }))
}


/// `GET /users/{username}/repos` -- one page of somebody's repositories.
async fn list_user_repos(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(username): Path<String>,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<RepoPage>> {
    if state.policy().require_auth {
        viewer.require()?;
    }
    let username = username.trim().to_ascii_lowercase();
    let owner: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM users WHERE username = $1 AND is_active = TRUE")
            .bind(&username)
            .fetch_optional(&state.db)
            .await?;
    let Some((owner_id,)) = owner else {
        return Err(AppError::NotFound(format!("no user named {username}")));
    };
    Ok(Json(owned_page(&state, &viewer, owner_id, &q).await?))
}

/// One page of the repositories `owner_id` has, as this viewer may see them.
async fn owned_page(
    state: &AppState,
    viewer: &Viewer,
    owner_id: Uuid,
    q: &ListQuery,
) -> AppResult<RepoPage> {
    let vid = viewer.id();
    let limit = q.limit();
    let pattern = q.pattern();
    let cursor = q.cursor.as_deref().and_then(Cursor::decode);

    let rows: Vec<RepoWithOwner> = sqlx::query_as(owned_sql!(
        "SELECT r.*, u.username FROM repos r
         JOIN users u ON u.id = r.owner_id
         WHERE r.owner_id = $1
           AND (",
        ")
           AND ($3::text IS NULL OR r.name ILIKE $3 ESCAPE '\\')
           AND ($4::timestamptz IS NULL OR (r.updated_at, r.id) < ($4, $5))
         ORDER BY r.updated_at DESC, r.id DESC
         LIMIT $6"
    ))
    .bind(owner_id)
    .bind(vid)
    .bind(pattern.as_deref())
    .bind(cursor.map(|c| c.updated_at))
    .bind(cursor.map(|c| c.id))
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;

    let (total,): (i64,) = sqlx::query_as(owned_sql!(
        "SELECT count(*) FROM repos r
          WHERE r.owner_id = $1
            AND (",
        ")
            AND ($3::text IS NULL OR r.name ILIKE $3 ESCAPE '\\')"
    ))
    .bind(owner_id)
    .bind(vid)
    .bind(pattern.as_deref())
    .fetch_one(&state.db)
    .await?;

    page_of(state, viewer, rows, limit, total).await
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
    // The point of the observer role: take part in what is here without also
    // being able to put new things on the server.
    crate::perms::require_site(
        u.site_role,
        u.site_role.can_create_repo(),
        "create repositories on this server",
    )?;

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
    // A new repository is the root of its own fork network, so it owns the
    // object store that any fork of it will later share. The column is NOT
    // NULL and self-referencing; leaving it out fails the insert outright.
    let res = sqlx::query(
        "INSERT INTO repos (id, owner_id, name, description, visibility, network_id)
         VALUES ($1, $2, $3, $4, $5, $1)",
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
        let mut roots: Vec<Hash> = roots
            .into_iter()
            .filter_map(|(b,)| Some(Hash(b.try_into().ok()?)))
            .collect();
        // Parked work points at objects nothing else does, so without this the
        // walk would reclaim exactly what somebody set aside to come back to.
        roots.extend(crate::stash::roots(&state.db, repo.network_id).await?);

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
    // A fork is a repository. Gating creation but not forking would be a hole
    // with a different name on it.
    crate::perms::require_site(
        u.site_role,
        u.site_role.can_create_repo(),
        "create repositories on this server",
    )?;
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

    // Branch rules govern branches, not tags: a tag is moved and deleted by
    // the release process, and its own protection is the force check on push.
    if !is_tag && !crate::rules::exempt(viewer.id(), repo.owner_id) {
        let rules = crate::rules::for_repo(&state.db, repo.id).await?;
        if let Some(why) = crate::rules::deny_delete(&rules, bare) {
            return Err(AppError::Forbidden(why));
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

    let mut views: Vec<RefView> = rows
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
        .collect();

    // One lookup for every branch and tag at once. A repository has a handful
    // of refs, so this is cheap, and it is the head commit the repository page
    // puts a name on.
    let found = crate::content::authors_of(
        &state.db,
        views.iter().filter_map(|v| v.head.as_ref()).map(|h| h.commit.as_str()),
    )
    .await;
    for v in views.iter_mut() {
        if let Some(h) = v.head.as_mut() {
            h.pushed_by = found.get(&h.commit).cloned();
        }
    }

    Ok(views)
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
        pushed_by: None,
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


/* ------------------------------------------------------------------ activity */

/// A day somebody pushed something.
#[derive(Debug, Serialize)]
struct DayView {
    date: chrono::NaiveDate,
    count: i64,
    /// The repository that took most of that day's commits, as `owner/name`.
    ///
    /// One name rather than a breakdown: the grid tints a square by it, and a
    /// square is eleven pixels wide. Ties go to whichever the database
    /// returned first, which is arbitrary and does not matter -- on a day
    /// split evenly between two projects, either answer is true.
    ///
    /// Empty when that repository is one the viewer may not be told about. The
    /// day still counts; it just has no name to give.
    repo: String,
}

/// What somebody has pushed, by day.
#[derive(Debug, Serialize)]
struct Activity {
    /// The window, inclusive. The client draws every day in it, including the
    /// empty ones, so it needs the bounds rather than inferring them from the
    /// days that happen to be present.
    since: chrono::NaiveDate,
    until: chrono::NaiveDate,
    total: i64,
    /// The busiest single day, so the client can scale its shading against
    /// this person's own year rather than against a number picked in advance.
    busiest: i64,
    /// Only the days with something on them. A year is 365 entries and most of
    /// them are noughts.
    days: Vec<DayView>,
}

/// `GET /users/{username}/activity` -- a year of pushes, day by day.
///
/// # Whose, and when
///
/// These are known in different ways and the graph should not blur them.
///
/// *Whose* is authenticated. `commit_authors` records the account that
/// delivered a commit, because the push carried a session or a token and the
/// server therefore knows -- an author string is content, and content is
/// whatever its writer typed. A token with `attributes` off records nothing,
/// which is the point of that switch: a mirror's traffic leaves no mark here.
///
/// *When* is the commit's own timestamp, which is claimed rather than
/// observed. It is used anyway, because the alternative is worse: bucketing by
/// arrival draws a five-year import as one enormous Tuesday and a fortnight of
/// offline work as a fortnight of nothing. Claimed time is the only record of
/// when work happened, and an imported history did happen on the days it says.
///
/// It is clamped to the push, though. A commit cannot have been written after
/// it was delivered, so a timestamp past its own `pushed_at` is a clock that
/// is wrong or a claim that is false, and either way today is the honest
/// answer. Backdating within that bound remains possible, exactly as it is in
/// git, and no amount of arithmetic here would change that.
///
/// # Private work counts, and is never named
///
/// Dropping a private repository's commits entirely would make the graph lie
/// about the person -- a fortnight spent on something unreleased would read as
/// a fortnight of nothing -- so they are counted. What is withheld is the
/// name: any repository this viewer may not read comes back with an empty
/// `repo`, and the client draws it in the neutral it uses for everything
/// outside the legend.
///
/// Every hidden repository shares that one empty label rather than getting an
/// anonymous one each, because distinct anonymous labels are a slow leak: the
/// colours alone would say how many private projects somebody keeps and
/// roughly when each was worked on.
///
/// What this does disclose is that the person was active on a given day and
/// how much. That is the deliberate trade -- an activity graph that hides
/// activity is not one -- and it is why the names, the messages and the hashes
/// stay behind the same access check every other listing uses. The feed at
/// `/pushes` makes the opposite choice for the same reason: it exists to show
/// what the work *was*, and there is no way to show that anonymously.
///
/// The counts are assembled per repository and then labelled rather than
/// summed in SQL, because a total is not something you can redact afterwards.
async fn get_activity(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(username): Path<String>,
) -> AppResult<Json<Activity>> {
    if state.policy().require_auth {
        viewer.require()?;
    }

    let username = username.trim().to_ascii_lowercase();
    let owner: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM users WHERE username = $1 AND is_active = TRUE")
            .bind(&username)
            .fetch_optional(&state.db)
            .await?;
    let Some((owner_id,)) = owner else {
        return Err(AppError::NotFound(format!("no user named {username}")));
    };

    let until = chrono::Utc::now().date_naive();

    // Always the full year, including for an account that has not been here
    // one. A short window would be a graph whose axis changes meaning between
    // profiles, and comparing two of them -- or one against itself a month
    // later -- would mean reading the heading first. Empty squares before
    // somebody joined are not a claim that they idled; they are the shape of a
    // year, which is what this is.
    let since = until - chrono::Duration::days(364);

    // Out to the start of that week, so the first column of the grid is a
    // whole one rather than a stub of two days.
    let back = chrono::Datelike::weekday(&since).num_days_from_sunday() as i64;
    let since = since - chrono::Duration::days(back);

    // Grouped by repository as well as by day, because the filtering below is
    // per repository and a day's total cannot be split up after it is summed.
    // A commit whose repository is gone has no visibility to check, so it is
    // dropped rather than guessed at.
    let rows: Vec<(chrono::NaiveDate, Uuid, i64)> = sqlx::query_as(
        "SELECT (LEAST(COALESCE(committed_at, pushed_at), pushed_at)
                   AT TIME ZONE 'UTC')::date AS day,
                repo_id, count(*)
           FROM commit_authors
          WHERE user_id = $1
            AND repo_id IS NOT NULL
            AND LEAST(COALESCE(committed_at, pushed_at), pushed_at) >= $2
          GROUP BY day, repo_id",
    )
    .bind(owner_id)
    .bind(
        since
            .and_hms_opt(0, 0, 0)
            .unwrap_or_default()
            .and_utc(),
    )
    .fetch_all(&state.db)
    .await?;

    // Which of those repositories may this viewer be told about at all.
    let mut ids: Vec<Uuid> = rows.iter().map(|(_, id, _)| *id).collect();
    ids.sort();
    ids.dedup();

    let repos: Vec<RepoWithOwner> = sqlx::query_as(
        "SELECT r.*, u.username FROM repos r
         JOIN users u ON u.id = r.owner_id
         WHERE r.id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;

    let (uid, admin, can_write) = match &viewer.user {
        Some(u) => (Some(u.id), u.is_admin, u.can_write),
        None => (None, false, false),
    };

    // Readable repositories get their name. The rest are not dropped -- see
    // the note on hidden work above -- they simply have no name to give.
    let mut visible: std::collections::HashMap<Uuid, String> = std::collections::HashMap::new();
    for row in repos {
        let access =
            resolve(&state.db, &row.repo, uid, admin, can_write, state.policy().require_auth)
                .await?;
        if access.can_read() {
            visible.insert(row.repo.id, format!("{}/{}", row.username, row.repo.name));
        }
    }

    // Per day: the total, and which label holds most of it.
    //
    // Every repository the viewer cannot read shares one label -- the empty
    // string -- rather than getting one each. Distinct anonymous labels would
    // be a slow leak: watch the squares change colour and you learn how many
    // private projects somebody keeps and roughly when each was worked on,
    // which is most of what the names would have told you.
    let mut by_day: std::collections::BTreeMap<chrono::NaiveDate, (i64, Vec<(String, i64)>)> =
        std::collections::BTreeMap::new();
    for (day, repo_id, n) in rows {
        let label = visible.get(&repo_id).cloned().unwrap_or_default();
        let slot = by_day.entry(day).or_insert_with(|| (0, Vec::new()));
        slot.0 += n;
        match slot.1.iter_mut().find(|(l, _)| *l == label) {
            Some((_, c)) => *c += n,
            None => slot.1.push((label, n)),
        }
    }

    let total: i64 = by_day.values().map(|(n, _)| *n).sum();
    let busiest: i64 = by_day.values().map(|(n, _)| *n).max().unwrap_or(0);
    let days = by_day
        .into_iter()
        .map(|(date, (count, mut tally))| {
            // Busiest label wins the square's colour. A tie between a named
            // project and hidden work goes to the named one, which is the more
            // useful answer and gives away nothing extra.
            tally.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
            let repo = tally.first().map(|(l, _)| l.clone()).unwrap_or_default();
            DayView { date, count, repo }
        })
        .collect();

    Ok(Json(Activity { since, until, total, busiest, days }))
}


/// One commit somebody delivered.
#[derive(Debug, Serialize)]
struct PushView {
    /// `owner/name`, which is both the label and the start of the link.
    repo: String,
    commit: String,
    short: String,
    summary: String,
    /// What the commit claims about who wrote it. Shown as the claim it is --
    /// the account is established by the push, not by this string.
    author: String,
    /// When it says it was written, and when it reached this server. Both,
    /// because they answer different questions and a feed that showed only one
    /// would be hiding the more interesting case: work that arrived in a lump
    /// long after it was done.
    committed_at: chrono::DateTime<chrono::Utc>,
    pushed_at: chrono::DateTime<chrono::Utc>,
}

/// `GET /users/{username}/pushes` -- the last commits this account delivered.
///
/// The graph says which seasons went where; this says what the work actually
/// was. Same table, same visibility rule, and the same split between what is
/// authenticated and what is claimed.
///
/// Over-fetches on purpose. Rows are dropped after the query by a per-
/// repository access check, so asking for exactly the number wanted would
/// return fewer than that whenever any of them turned out to be private -- and
/// the shortfall would leak the fact that something was filtered.
async fn get_pushes(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(username): Path<String>,
) -> AppResult<Json<Vec<PushView>>> {
    if state.policy().require_auth {
        viewer.require()?;
    }

    const WANT: usize = 20;
    const OVER: i64 = 120;

    let username = username.trim().to_ascii_lowercase();
    let owner: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM users WHERE username = $1 AND is_active = TRUE")
            .bind(&username)
            .fetch_optional(&state.db)
            .await?;
    let Some((owner_id,)) = owner else {
        return Err(AppError::NotFound(format!("no user named {username}")));
    };

    #[derive(sqlx::FromRow)]
    struct Row {
        commit_hash: Vec<u8>,
        repo_id: Uuid,
        committed_at: chrono::DateTime<chrono::Utc>,
        pushed_at: chrono::DateTime<chrono::Utc>,
    }

    let rows: Vec<Row> = sqlx::query_as(
        "SELECT commit_hash, repo_id,
                LEAST(COALESCE(committed_at, pushed_at), pushed_at) AS committed_at,
                pushed_at
           FROM commit_authors
          WHERE user_id = $1 AND repo_id IS NOT NULL
          ORDER BY LEAST(COALESCE(committed_at, pushed_at), pushed_at) DESC
          LIMIT $2",
    )
    .bind(owner_id)
    .bind(OVER)
    .fetch_all(&state.db)
    .await?;

    let mut ids: Vec<Uuid> = rows.iter().map(|r| r.repo_id).collect();
    ids.sort();
    ids.dedup();

    let repos: Vec<RepoWithOwner> = sqlx::query_as(
        "SELECT r.*, u.username FROM repos r
         JOIN users u ON u.id = r.owner_id
         WHERE r.id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;

    let (uid, admin, can_write) = match &viewer.user {
        Some(u) => (Some(u.id), u.is_admin, u.can_write),
        None => (None, false, false),
    };

    // Resolved once per repository rather than once per commit: twenty commits
    // on one project is one question, not twenty.
    let mut ok: std::collections::HashMap<Uuid, (String, Uuid)> =
        std::collections::HashMap::new();
    for row in repos {
        let access =
            resolve(&state.db, &row.repo, uid, admin, can_write, state.policy().require_auth)
                .await?;
        if access.can_read() {
            ok.insert(
                row.repo.id,
                (format!("{}/{}", row.username, row.repo.name), row.repo.network_id),
            );
        }
    }

    let mut out = Vec::with_capacity(WANT);
    for r in rows {
        if out.len() >= WANT {
            break;
        }
        let Some((full_name, network)) = ok.get(&r.repo_id) else {
            continue;
        };
        let Ok(bytes) = <[u8; 32]>::try_from(r.commit_hash.as_slice()) else {
            continue;
        };
        let Ok(store) = state.store_for_network(*network) else {
            continue;
        };
        // A row whose object is gone -- collected, or never fully received --
        // is skipped rather than rendered as a commit with no message.
        let Some(head) = head_view(&store, Hash(bytes)) else {
            continue;
        };
        out.push(PushView {
            repo: full_name.clone(),
            commit: head.commit,
            short: head.short,
            summary: head.summary,
            author: head.author,
            committed_at: r.committed_at,
            pushed_at: r.pushed_at,
        });
    }

    Ok(Json(out))
}

/* ------------------------------------------------------------- paged listings */

/// How many repositories a listing hands back at once, and the most it will.
///
/// The old answer was two hundred, in one shot, with no way to ask for more and
/// nothing said when there were. Somebody with three thousand repositories got
/// two hundred of them and a page that quietly claimed that was all of them.
pub const PAGE: i64 = 30;
pub const PAGE_MAX: i64 = 100;

/// Where a listing left off: the sort key of the last row it returned.
///
/// Keyset rather than an offset. `ORDER BY updated_at DESC` over a table where
/// a push changes `updated_at` means offsets slide underneath a reader -- push
/// to something while somebody is on page two and a repository they have
/// already seen moves onto page three, while another is skipped entirely. A
/// key does not move.
#[derive(Debug, Clone, Copy)]
struct Cursor {
    updated_at: chrono::DateTime<chrono::Utc>,
    id: Uuid,
}

impl Cursor {
    /// `<rfc3339>|<uuid>`. Opaque to the client by convention, not by
    /// encryption: it names a sort position, which is not a secret, and a
    /// forged one can only seek within what the query already allows.
    fn encode(&self) -> String {
        format!("{}|{}", self.updated_at.to_rfc3339(), self.id)
    }

    fn decode(raw: &str) -> Option<Self> {
        let (when, id) = raw.split_once('|')?;
        Some(Cursor {
            updated_at: chrono::DateTime::parse_from_rfc3339(when).ok()?.with_timezone(&chrono::Utc),
            id: id.parse().ok()?,
        })
    }
}

/// What a listing takes from the query string.
#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// Substring of the name to match, case-insensitively. Absent or blank
    /// means everything.
    q: Option<String>,
    /// Where to carry on from, from the previous response's `next`.
    cursor: Option<String>,
    limit: Option<i64>,
}

impl ListQuery {
    fn limit(&self) -> i64 {
        self.limit.unwrap_or(PAGE).clamp(1, PAGE_MAX)
    }

    /// The search term, normalised, with LIKE's wildcards defanged.
    ///
    /// Without escaping, a name containing `%` matches everything and one
    /// containing `_` matches any character -- so searching for a repository
    /// called `wip_2` would quietly return `wip-2` as well. The pattern is
    /// still bound as a parameter, so this is about correctness rather than
    /// injection.
    fn pattern(&self) -> Option<String> {
        let raw = self.q.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        let escaped = raw.replace('\\', r"\\").replace('%', r"\%").replace('_', r"\_");
        Some(format!("%{escaped}%"))
    }
}

/// One page of repositories, and where to carry on from.
#[derive(Debug, Serialize)]
pub struct RepoPage {
    items: Vec<RepoView>,
    /// The cursor for the next page, absent on the last one.
    next: Option<String>,
    /// How many match in total -- not how many are on this page.
    ///
    /// Counted with the same predicate rather than inferred from the page, so
    /// a heading can say "30 of 3,000" instead of implying the thirty are all
    /// there is.
    total: i64,
}

/// Turn rows into views: the access check, the head summaries, and the cursor.
///
/// The per-repository `resolve` here is what the SQL predicate cannot be
/// trusted to have done -- it is the authority, and a repository the viewer
/// cannot read must not appear even by name. It is a query per row, which is
/// why bounding the page matters beyond the payload size: at two hundred rows
/// this loop was two hundred round trips for one page load, and at thirty it
/// is thirty.
async fn page_of(
    state: &AppState,
    viewer: &Viewer,
    rows: Vec<RepoWithOwner>,
    limit: i64,
    total: i64,
) -> AppResult<RepoPage> {
    // One row was fetched beyond the page to find out whether there is another
    // one, which is cheaper and more honest than counting to decide.
    let more = rows.len() as i64 > limit;
    let rows: Vec<RepoWithOwner> = rows.into_iter().take(limit as usize).collect();

    let next = if more {
        rows.last().map(|r| {
            Cursor { updated_at: r.repo.updated_at, id: r.repo.id }.encode()
        })
    } else {
        None
    };

    let (uid, admin, can_write) = match &viewer.user {
        Some(u) => (Some(u.id), u.is_admin, u.can_write),
        None => (None, false, false),
    };

    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let access =
            resolve(&state.db, &row.repo, uid, admin, can_write, state.policy().require_auth)
                .await?;
        if access.can_read() {
            items.push(super::repo_view(&row.repo, &row.username, access));
        }
    }
    super::attach_heads(state, &mut items).await;

    Ok(RepoPage { items, next, total })
}
