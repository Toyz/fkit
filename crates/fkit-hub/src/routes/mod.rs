//! HTTP routing.

pub mod admin;
pub mod browse;
pub mod gomod;
pub mod issues;
pub mod merges;
pub mod repos;
pub mod session;
pub mod social;

use crate::auth::Viewer;
use crate::error::{AppError, AppResult};
use crate::models::{HeadView, RepoRow, RepoView};
use crate::perms::{resolve, Access};
use crate::state::AppState;

/// Load a repository by `owner/name` and resolve the viewer's access in one
/// step. Every repository route starts here, which is what keeps the permission
/// check impossible to forget.
pub async fn load_repo(
    state: &AppState,
    viewer: &Viewer,
    owner: &str,
    name: &str,
) -> AppResult<(RepoRow, Access, String)> {
    let owner_lc = owner.to_ascii_lowercase();

    let repo: Option<RepoRow> = sqlx::query_as(
        "SELECT r.* FROM repos r
         JOIN users u ON u.id = r.owner_id
         WHERE u.username = $1 AND r.name = $2",
    )
    .bind(&owner_lc)
    .bind(name)
    .fetch_optional(&state.db)
    .await?;

    // A missing repo and an invisible one must be indistinguishable.
    let repo = repo.ok_or_else(|| AppError::not_found(format!("no such repository: {owner}/{name}")))?;

    let (vid, admin, can_write) = match &viewer.user {
        Some(u) => (Some(u.id), u.is_admin, u.can_write),
        None => (None, false, false),
    };
    let access = resolve(&state.db, &repo, vid, admin, can_write, state.policy().require_auth).await?;
    crate::perms::require_read(access, owner, name)?;

    Ok((repo, access, owner_lc))
}

pub fn repo_view(repo: &RepoRow, owner: &str, access: Access) -> RepoView {
    RepoView {
        id: repo.id,
        owner: owner.to_string(),
        name: repo.name.clone(),
        full_name: format!("{owner}/{}", repo.name),
        description: repo.description.clone(),
        visibility: repo.visibility.clone(),
        default_branch: repo.default_branch.clone(),
        homepage: repo.homepage.clone(),
        topics: repo.topics.clone(),
        created_at: repo.created_at,
        updated_at: repo.updated_at,
        access: access.as_str().to_string(),
        head: None,
        branches: 0,
        tags: 0,
        open_issues: 0,
        open_merges: 0,
        forked_from: None,
        via_admin: false,
        network_id: repo.network_id,
    }
}

/// Fill in the head commit and branch count for a page of repositories.
///
/// One query for every ref of every listed repository, then one object read per
/// repository — rather than two queries each. The commit body lives in the
/// per-repository store on disk, so it cannot come from SQL at all; a listing
/// that showed only a name and a timestamp was the result of not wanting to
/// pay for this, and it said nothing useful.
pub async fn attach_heads(state: &AppState, views: &mut [RepoView]) {
    use std::collections::HashMap;

    if views.is_empty() {
        return;
    }
    let ids: Vec<uuid::Uuid> = views.iter().map(|v| v.id).collect();

    let rows: Vec<(uuid::Uuid, String, Vec<u8>)> =
        match sqlx::query_as("SELECT repo_id, name, target FROM refs WHERE repo_id = ANY($1)")
            .bind(&ids)
            .fetch_all(&state.db)
            .await
        {
            Ok(r) => r,
            // A listing is worth showing without this decoration.
            Err(e) => {
                tracing::warn!("listing heads: {e}");
                return;
            }
        };

    // Two more bulk counts rather than two queries per repository. A listing
    // is worth showing without them, so a failure here is logged and dropped
    // the same way the refs are.
    // What each of these was forked from, as `owner/name`.
    let mut parents: HashMap<uuid::Uuid, String> = HashMap::new();
    let forked: Result<Vec<(uuid::Uuid, String, String)>, _> = sqlx::query_as(
        "SELECT c.id, u.username, p.name
           FROM repos c JOIN repos p ON p.id = c.forked_from
           JOIN users u ON u.id = p.owner_id
          WHERE c.id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await;
    if let Ok(rows) = forked {
        for (id, owner, name) in rows {
            parents.insert(id, format!("{owner}/{name}"));
        }
    }

    let mut open: HashMap<uuid::Uuid, (i64, i64)> = HashMap::new();
    let counts: Result<Vec<(uuid::Uuid, i64, i64)>, _> = sqlx::query_as(
        "SELECT r.id,
                (SELECT COUNT(*) FROM issues i
                  WHERE i.repo_id = r.id AND i.state = 'open'),
                (SELECT COUNT(*) FROM merge_requests m
                  WHERE m.repo_id = r.id AND m.state = 'open')
           FROM repos r WHERE r.id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await;
    match counts {
        Ok(rows) => {
            for (id, issues, merges) in rows {
                open.insert(id, (issues, merges));
            }
        }
        Err(e) => tracing::warn!("listing open counts: {e}"),
    }

    let mut tips: HashMap<uuid::Uuid, HashMap<String, Vec<u8>>> = HashMap::new();
    for (id, name, target) in rows {
        tips.entry(id).or_default().insert(name, target);
    }

    for v in views.iter_mut() {
        if let Some(&(issues, merges)) = open.get(&v.id) {
            v.open_issues = issues;
            v.open_merges = merges;
        }
        if let Some(p) = parents.remove(&v.id) {
            v.forked_from = Some(p);
        }
        let Some(refs) = tips.get(&v.id) else { continue };
        // Counted separately: `refs` is every ref, and tags live in the same
        // namespace. Counting the lot reported a repository with one branch
        // and three tags as having four branches.
        v.branches = refs.keys().filter(|n| !fkit_core::session::is_tag(n)).count() as i64;
        v.tags = refs.keys().filter(|n| fkit_core::session::is_tag(n)).count() as i64;

        let Some(target) = refs.get(&v.default_branch) else { continue };
        let Ok(bytes) = <[u8; 32]>::try_from(target.as_slice()) else { continue };
        let hash = fkit_core::Hash(bytes);

        let Ok(store) = state.store_for_network(v.network_id) else { continue };
        let Ok(fkit_core::Object::Commit(c)) = store.get(hash) else { continue };

        let hex = hash.to_hex();
        v.head = Some(HeadView {
            short: hex[..10].to_string(),
            commit: hex,
            summary: c.message.lines().next().unwrap_or_default().to_string(),
            author: c.author,
            timestamp: c.timestamp,
            // A listing of many repositories spans many stores; the account
            // link is a per-repository detail and is filled in there.
            pushed_by: None,
        });
    }
}

/// The next `#N` for a repository.
///
/// One counter across issues and merge requests, because `#4` has to name one
/// thing. Callers hold `FOR UPDATE` on the repo row: Postgres refuses that
/// alongside an aggregate, and locking the parent is what actually serialises
/// numbering for this repository anyway.
pub async fn next_number(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    repo_id: uuid::Uuid,
) -> Result<i32, sqlx::Error> {
    let (n,): (i32,) = sqlx::query_as(
        "SELECT COALESCE(GREATEST(
             (SELECT MAX(number) FROM merge_requests WHERE repo_id = $1),
             (SELECT MAX(number) FROM issues WHERE repo_id = $1)
         ), 0) + 1",
    )
    .bind(repo_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(n)
}

pub async fn audit(
    state: &AppState,
    actor: Option<uuid::Uuid>,
    repo: Option<uuid::Uuid>,
    action: &str,
    detail: serde_json::Value,
) {
    let _ = sqlx::query(
        "INSERT INTO audit_log (actor_id, repo_id, action, detail) VALUES ($1, $2, $3, $4)",
    )
    .bind(actor)
    .bind(repo)
    .bind(action)
    .bind(detail)
    .execute(&state.db)
    .await;
}
