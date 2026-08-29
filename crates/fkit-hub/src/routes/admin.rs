//! Server administration: instance policy and user accounts.
//!
//! Every route here requires `users.is_admin`. The first account created on a
//! server becomes an administrator; after that, administrators promote others.
//!
//! Two invariants are enforced throughout, because getting either wrong locks a
//! server out of itself permanently:
//!
//! * an administrator cannot remove their own admin rights, and
//! * the last remaining administrator cannot be demoted, disabled or deleted.

use crate::auth::{Viewer, ViewerUser};
use crate::error::{AppError, AppResult};
use crate::settings::Instance;
use crate::perms::SiteRole;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::routing::{delete, get, patch};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/settings", get(get_settings).patch(patch_settings))
        .route("/admin/stats", get(stats))
        .route("/admin/cache", get(cache).delete(drop_cache))
        .route("/admin/users", get(list_users))
        .route("/admin/users/{id}", patch(patch_user))
        .route("/admin/users/{id}", delete(delete_user))
        .route("/admin/email", get(get_email).patch(patch_email))
        .route("/admin/email/test", axum::routing::post(test_email))
        .route("/admin/invites", get(list_invites).post(create_invite))
        .route("/admin/invites/{id}", delete(revoke_invite))
}

/// Gate every route in this module.
async fn require_admin(viewer: &Viewer) -> AppResult<&ViewerUser> {
    let u = viewer.require()?;
    // Asked of the role rather than the derived flag: the role is the fact,
    // and this is the place a capability check belongs.
    if !u.site_role.can_administer_site() {
        // The admin area's existence is not a secret; only its contents are.
        return Err(AppError::Forbidden("administrator access required".into()));
    }
    Ok(u)
}

async fn get_settings(State(state): State<AppState>, viewer: Viewer) -> AppResult<Json<Instance>> {
    require_admin(&viewer).await?;
    Ok(Json(state.policy()))
}

#[derive(Deserialize)]
struct SettingsPatch {
    site_name: Option<String>,
    open_registration: Option<bool>,
    require_auth: Option<bool>,
    default_repo_visibility: Option<String>,
    /// What a new account gets: "observer", "member" or "admin".
    default_site_role: Option<String>,
    allowed_email_domains: Option<Vec<String>>,
}

async fn patch_settings(
    State(state): State<AppState>,
    viewer: Viewer,
    Json(body): Json<SettingsPatch>,
) -> AppResult<Json<Instance>> {
    let u = require_admin(&viewer).await?;

    if let Some(v) = &body.default_repo_visibility
        && !matches!(v.as_str(), "public" | "private")
    {
        return Err(AppError::bad("visibility must be 'public' or 'private'"));
    }
    if let Some(r) = &body.default_site_role
        && SiteRole::parse(r).is_none()
    {
        return Err(AppError::bad("role must be admin, member or observer"));
    }
    let domains: Option<Vec<String>> = body.allowed_email_domains.map(|list| {
        list.into_iter()
            .map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase())
            .filter(|d| !d.is_empty())
            .collect()
    });

    sqlx::query(
        "UPDATE instance_settings SET
            site_name               = COALESCE($1, site_name),
            open_registration       = COALESCE($2, open_registration),
            require_auth            = COALESCE($3, require_auth),
            default_repo_visibility = COALESCE($4, default_repo_visibility),
            default_site_role       = COALESCE($5, default_site_role),
            allowed_email_domains   = COALESCE($6, allowed_email_domains),
            updated_at              = now(),
            updated_by              = $7
         WHERE id = TRUE",
    )
    .bind(&body.site_name)
    .bind(body.open_registration)
    .bind(body.require_auth)
    .bind(&body.default_repo_visibility)
    .bind(&body.default_site_role)
    .bind(&domains)
    .bind(u.id)
    .execute(&state.db)
    .await?;

    // Re-read through `Settings`, never with a column list written here: it is
    // the only path that reads *every* column and re-applies the environment
    // overlay. A narrower query silently drops the mail configuration out of
    // the cache — and, since `Instance` requires those columns, fails outright.
    let next = state.settings.refresh(&state.db).await.map_err(AppError::Internal)?;

    super::audit(&state, Some(u.id), None, "instance.settings",
        serde_json::json!({
            "open_registration": next.open_registration,
            "require_auth": next.require_auth,
        })).await;

    Ok(Json(next))
}

#[derive(Serialize)]
struct Stats {
    users: i64,
    admins: i64,
    repos: i64,
    public_repos: i64,
    merge_requests: i64,
    open_merge_requests: i64,
    /// Bytes the object stores occupy on disk.
    disk_bytes: u64,
}

async fn stats(State(state): State<AppState>, viewer: Viewer) -> AppResult<Json<Stats>> {
    require_admin(&viewer).await?;

    let one = |sql: &'static str| {
        let db = state.db.clone();
        async move {
            sqlx::query_as::<_, (i64,)>(sql)
                .fetch_one(&db)
                .await
                .map(|r| r.0)
                .unwrap_or(0)
        }
    };

    Ok(Json(Stats {
        users: one("SELECT count(*) FROM users").await,
        admins: one("SELECT count(*) FROM users WHERE is_admin").await,
        repos: one("SELECT count(*) FROM repos").await,
        public_repos: one("SELECT count(*) FROM repos WHERE visibility = 'public'").await,
        merge_requests: one("SELECT count(*) FROM merge_requests").await,
        open_merge_requests: one("SELECT count(*) FROM merge_requests WHERE state = 'open'").await,
        disk_bytes: dir_size(&state.data_dir.join("repos")),
    }))
}

/// What the object cache is holding, and whether it is earning its memory.
///
/// Worth exposing because the alternative is guessing. A process that settles
/// well above its idle size looks exactly like a leak from the outside, and
/// the only way to tell the difference is to ask the cache what it is doing.
#[derive(Serialize)]
struct CacheView {
    /// "memory", or "memory, then <host>" when a shared tier is configured.
    backend: String,
    entries: usize,
    bytes: usize,
    capacity: usize,
    hits: u64,
    misses: u64,
    /// Hits as a percentage of lookups. `null` before anything has been asked
    /// for — zero would read as "not working" rather than "not yet used".
    hit_rate: Option<f64>,
    /// How full it is, as a percentage of the configured capacity.
    fill: f64,
}

async fn cache(State(state): State<AppState>, viewer: Viewer) -> AppResult<Json<CacheView>> {
    require_admin(&viewer).await?;
    let s = state.object_cache.stats();
    let lookups = s.hits + s.misses;

    Ok(Json(CacheView {
        backend: state.cache_backend.clone(),
        entries: s.entries,
        bytes: s.bytes,
        capacity: s.capacity,
        hits: s.hits,
        misses: s.misses,
        hit_rate: (lookups > 0).then(|| s.hits as f64 * 100.0 / lookups as f64),
        fill: if s.capacity == 0 {
            0.0
        } else {
            s.bytes as f64 * 100.0 / s.capacity as f64
        },
    }))
}

/// Empty the cache.
///
/// Not a correctness tool — a cached object can never be stale, because its
/// key is a digest of its value. It is here to hand the memory back without a
/// restart, and to make a cold measurement possible.
async fn drop_cache(State(state): State<AppState>, viewer: Viewer) -> AppResult<Json<CacheView>> {
    let u = require_admin(&viewer).await?;
    state.object_cache.clear();
    super::audit(&state, Some(u.id), None, "cache.clear", serde_json::json!({})).await;
    cache(State(state), viewer).await
}

fn dir_size(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .filter_map(|e| e.ok())
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            _ => e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .sum()
}

#[derive(Serialize, sqlx::FromRow)]
struct AdminUser {
    id: Uuid,
    username: String,
    email: String,
    display_name: Option<String>,
    site_role: String,
    is_admin: bool,
    is_active: bool,
    created_at: DateTime<Utc>,
    repo_count: i64,
}

async fn list_users(State(state): State<AppState>, viewer: Viewer) -> AppResult<Json<Vec<AdminUser>>> {
    require_admin(&viewer).await?;
    let rows: Vec<AdminUser> = sqlx::query_as(
        "SELECT u.id, u.username, u.email, u.display_name, u.site_role, u.is_admin, u.is_active,
                u.created_at,
                (SELECT count(*) FROM repos r WHERE r.owner_id = u.id) AS repo_count
           FROM users u
          ORDER BY u.created_at",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct UserPatch {
    /// "admin", "member" or "observer".
    site_role: Option<String>,
    is_active: Option<bool>,
}

/// Would this change leave the server with no administrator?
async fn would_orphan(state: &AppState, target: Uuid, still_admin: bool) -> AppResult<bool> {
    if still_admin {
        return Ok(false);
    }
    let others: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM users WHERE is_admin AND is_active AND id <> $1",
    )
    .bind(target)
    .fetch_one(&state.db)
    .await?;
    Ok(others.0 == 0)
}

async fn patch_user(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<Uuid>,
    Json(body): Json<UserPatch>,
) -> AppResult<Json<AdminUser>> {
    let me = require_admin(&viewer).await?;

    // Validated before anything is compared, so a typo cannot fall through to
    // a role nobody has.
    let asked = match body.site_role.as_deref() {
        None => None,
        Some(r) => Some(
            SiteRole::parse(r)
                .ok_or_else(|| AppError::bad("role must be admin, member or observer"))?,
        ),
    };

    // Self-demotion is almost always a mistake and is trivially recoverable
    // only if another admin exists — refuse it outright and make the operator
    // use a second account.
    let demoting_self = asked.is_some_and(|r| r != SiteRole::Admin);
    if id == me.id && (demoting_self || body.is_active == Some(false)) {
        return Err(AppError::bad(
            "you cannot remove your own administrator rights or disable your own account",
        ));
    }

    let current: (String, bool) =
        sqlx::query_as("SELECT site_role, is_active FROM users WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| AppError::not_found("no such user"))?;

    let next_role =
        asked.or_else(|| SiteRole::parse(&current.0)).unwrap_or(SiteRole::Observer);
    let next_active = body.is_active.unwrap_or(current.1);

    if would_orphan(&state, id, next_role == SiteRole::Admin && next_active).await? {
        return Err(AppError::bad(
            "this is the last active administrator — promote someone else first",
        ));
    }

    // `is_admin` is generated from this column, so there is only one thing to
    // write and the two can never disagree.
    sqlx::query("UPDATE users SET site_role = $2, is_active = $3 WHERE id = $1")
        .bind(id)
        .bind(next_role.as_str())
        .bind(next_active)
        .execute(&state.db)
        .await?;

    // A disabled account must lose its live credentials immediately, or it
    // stays signed in and its tokens keep pushing.
    if !next_active {
        let _ = sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(id)
            .execute(&state.db)
            .await;
        let _ = sqlx::query("DELETE FROM access_tokens WHERE user_id = $1")
            .bind(id)
            .execute(&state.db)
            .await;
    }

    super::audit(&state, Some(me.id), None, "user.update",
        serde_json::json!({
            "user": id,
            "site_role": next_role.as_str(),
            "is_active": next_active,
        })).await;

    let row: AdminUser = sqlx::query_as(
        "SELECT u.id, u.username, u.email, u.display_name, u.site_role, u.is_admin, u.is_active,
                u.created_at,
                (SELECT count(*) FROM repos r WHERE r.owner_id = u.id) AS repo_count
           FROM users u WHERE u.id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row))
}

async fn delete_user(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let me = require_admin(&viewer).await?;
    if id == me.id {
        return Err(AppError::bad("you cannot delete your own account from here"));
    }
    if would_orphan(&state, id, false).await? {
        return Err(AppError::bad(
            "this is the last active administrator — promote someone else first",
        ));
    }

    // Repositories cascade, so their object stores have to go too or they are
    // orphaned on disk forever.
    let repos: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM repos WHERE owner_id = $1")
        .bind(id)
        .fetch_all(&state.db)
        .await?;

    let done = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::not_found("no such user"));
    }
    for (repo,) in repos {
        let _ = std::fs::remove_dir_all(state.repo_path(repo));
    }

    super::audit(&state, Some(me.id), None, "user.delete", serde_json::json!({ "user": id })).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---- email configuration ------------------------------------------------

#[derive(Deserialize)]
pub struct EmailPatch {
    pub email_from: Option<String>,
    pub public_url: Option<String>,
    /// `Some("")` clears the stored key; `None` leaves it untouched. There is
    /// deliberately no way to read it back.
    pub resend_api_key: Option<String>,
}

#[derive(Serialize)]
pub struct EmailStatus {
    pub configured: bool,
    pub email_from: String,
    pub public_url: String,
    /// Whether a key is stored, never the key.
    pub has_api_key: bool,
    /// Fields the server takes from its environment; these cannot be changed
    /// from here, so the UI shows them as fixed instead of editable.
    pub key_from_env: bool,
    pub sender_from_env: bool,
    pub url_from_env: bool,
}

impl EmailStatus {
    fn of(p: &Instance) -> EmailStatus {
        EmailStatus {
            configured: p.email_configured(),
            email_from: p.email_from.clone(),
            public_url: p.public_url.clone(),
            has_api_key: p.resend_api_key.as_deref().is_some_and(|k| !k.trim().is_empty()),
            key_from_env: p.key_from_env,
            sender_from_env: p.sender_from_env,
            url_from_env: p.url_from_env,
        }
    }
}

pub async fn get_email(State(state): State<AppState>, viewer: Viewer) -> AppResult<Json<EmailStatus>> {
    require_admin(&viewer).await?;
    Ok(Json(EmailStatus::of(&state.policy())))
}

pub async fn patch_email(
    State(state): State<AppState>,
    viewer: Viewer,
    Json(body): Json<EmailPatch>,
) -> AppResult<Json<EmailStatus>> {
    let u = require_admin(&viewer).await?;

    if let Some(from) = &body.email_from
        && !from.trim().is_empty()
    {
        crate::auth::validate_email(from)?;
    }
    if let Some(url) = &body.public_url
        && !url.trim().is_empty()
        && !(url.starts_with("http://") || url.starts_with("https://"))
    {
        return Err(AppError::bad("the public URL must start with http:// or https://"));
    }

    // Accepting a value the environment is going to override would be a form
    // that appears to work and silently does nothing.
    let env = state.settings.env_email();
    for (sent, pinned, var) in [
        (body.resend_api_key.is_some(), env.api_key.is_some(), "RESEND_API_KEY"),
        (body.email_from.is_some(), env.from.is_some(), "FKIT_EMAIL_FROM"),
        (body.public_url.is_some(), env.public_url.is_some(), "FKIT_PUBLIC_URL"),
    ] {
        if sent && pinned {
            return Err(AppError::bad(format!(
                "this server takes that setting from the {var} environment variable; \
                 change it there and restart"
            )));
        }
    }

    // An empty string means "forget the key"; absent means "leave it".
    let key: Option<Option<String>> = body.resend_api_key.map(|k| {
        let k = k.trim().to_string();
        if k.is_empty() { None } else { Some(k) }
    });

    sqlx::query(
        "UPDATE instance_settings SET
            email_from     = COALESCE($1, email_from),
            public_url     = COALESCE($2, public_url),
            resend_api_key = CASE WHEN $3 THEN $4 ELSE resend_api_key END,
            updated_at     = now(),
            updated_by     = $5
         WHERE id = TRUE",
    )
    .bind(body.email_from.as_ref().map(|s| s.trim().to_string()))
    .bind(body.public_url.as_ref().map(|s| s.trim().trim_end_matches('/').to_string()))
    .bind(key.is_some())
    .bind(key.clone().flatten())
    .bind(u.id)
    .execute(&state.db)
    .await?;

    let next = state.settings.refresh(&state.db).await.map_err(AppError::Internal)?;

    super::audit(&state, Some(u.id), None, "instance.email",
        serde_json::json!({ "configured": next.email_configured(), "key_changed": key.is_some() })).await;

    Ok(Json(EmailStatus::of(&next)))
}

/// Send a test message to the administrator's own address.
///
/// Resend fails for undramatic reasons — an unverified sending domain, a key
/// scoped to the wrong account — and none of them are visible until a real
/// message is attempted. Better to find out here than when someone is locked
/// out and waiting for a reset link.
pub async fn test_email(State(state): State<AppState>, viewer: Viewer) -> AppResult<Json<serde_json::Value>> {
    let u = require_admin(&viewer).await?;
    let p = state.policy();

    let mailer = crate::email::Mailer::new(p.resend_api_key.as_deref(), &p.email_from)
        .ok_or_else(|| AppError::bad("set a Resend API key and a sender address first"))?;

    let to: (String,) = sqlx::query_as("SELECT email FROM users WHERE id = $1")
        .bind(u.id)
        .fetch_one(&state.db)
        .await?;

    mailer
        .send(
            &to.0,
            "fkit hub test message",
            "Email is working. Password reset links will be delivered from this address.\n",
        )
        .await
        .map_err(|e| AppError::bad(format!("{e}")))?;

    Ok(Json(serde_json::json!({ "sent_to": to.0 })))
}

// ---- invitations ---------------------------------------------------------
//
// A server with `open_registration` off has no sign-up form. An invite is how
// a named person gets in anyway: a single-use token, optionally bound to one
// address, that suspends the policy for exactly one registration.

/// How long a new invite is good for, unless the caller says otherwise.
const INVITE_DAYS: i64 = 14;
/// A month is already generous for something that grants an account.
const INVITE_MAX_DAYS: i64 = 90;

#[derive(Deserialize)]
pub struct NewInvite {
    /// Bind the invite to one address, and mail the link there if the server
    /// can send. Omit for a link you will hand over yourself.
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    /// Make the new account an administrator. Off unless asked for.
    #[serde(default)]
    pub is_admin: bool,
    #[serde(default)]
    pub expires_days: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct InviteRow {
    pub id: Uuid,
    pub email: Option<String>,
    pub note: String,
    pub is_admin: bool,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
    pub used_by: Option<String>,
}

#[derive(Serialize)]
pub struct CreatedInvite {
    #[serde(flatten)]
    pub invite: InviteRow,
    /// The only time the link is ever readable. Shown once, then gone.
    pub url: String,
    /// Whether the invite mail actually went out.
    pub emailed: bool,
    /// Why it did not, when it did not.
    pub email_error: Option<String>,
}

const SELECT_INVITES: &str = "SELECT i.id, i.email, i.note, i.is_admin,
            c.username AS created_by, i.created_at, i.expires_at, i.used_at,
            u.username AS used_by
       FROM invites i
       LEFT JOIN users c ON c.id = i.created_by
       LEFT JOIN users u ON u.id = i.used_by
      ORDER BY i.used_at IS NOT NULL, i.created_at DESC
      LIMIT 200";

async fn list_invites(State(state): State<AppState>, viewer: Viewer) -> AppResult<Json<Vec<InviteRow>>> {
    require_admin(&viewer).await?;
    Ok(Json(sqlx::query_as(SELECT_INVITES).fetch_all(&state.db).await?))
}

async fn create_invite(
    State(state): State<AppState>,
    viewer: Viewer,
    Json(body): Json<NewInvite>,
) -> AppResult<Json<CreatedInvite>> {
    let admin = require_admin(&viewer).await?;
    let policy = state.policy();

    let email = match body.email.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => {
            let e = crate::auth::validate_email(e)?;
            // An invite to an address the domain policy will reject is a link
            // that fails at the last step, after the person has chosen a
            // password. Catch it here instead.
            if !policy.email_allowed(&e) {
                return Err(AppError::bad(
                    "that email domain is not permitted on this server — \
                     change the allowed domains first",
                ));
            }
            if taken(&state, &e).await? {
                return Err(AppError::conflict("that address already has an account"));
            }
            Some(e)
        }
        None => None,
    };

    let days = body.expires_days.unwrap_or(INVITE_DAYS);
    if !(1..=INVITE_MAX_DAYS).contains(&days) {
        return Err(AppError::bad(format!(
            "an invite may last between 1 and {INVITE_MAX_DAYS} days"
        )));
    }

    let id = Uuid::new_v4();
    let secret = crate::auth::random_token();
    let note = body.note.unwrap_or_default().trim().to_string();

    sqlx::query(
        "INSERT INTO invites (id, token_hash, email, note, is_admin, created_by, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, now() + ($7 || ' days')::interval)",
    )
    .bind(id)
    .bind(crate::auth::token_digest(&secret))
    .bind(&email)
    .bind(&note)
    .bind(body.is_admin)
    .bind(admin.id)
    .bind(days.to_string())
    .execute(&state.db)
    .await?;

    let base = policy.public_url.trim_end_matches('/');
    let url = format!("{base}/register?invite={secret}");

    // Mail it only when there is somewhere to send it. A failure here is worth
    // reporting — the invite still exists and the link is on screen.
    let mut emailed = false;
    let mut email_error = None;
    if let (Some(to), Some(mailer)) = (
        &email,
        crate::email::Mailer::new(policy.resend_api_key.as_deref(), &policy.email_from),
    ) {
        match mailer
            .send(
                to,
                &format!("You have been invited to {}", policy.site_name),
                &crate::email::invite_body(&admin.username, &policy.site_name, &url, days),
            )
            .await
        {
            Ok(()) => emailed = true,
            Err(e) => email_error = Some(format!("{e}")),
        }
    }

    super::audit(&state, Some(admin.id), None, "invite.create",
        serde_json::json!({ "invite": id, "email": email, "admin": body.is_admin })).await;

    let invite = sqlx::query_as(
        "SELECT i.id, i.email, i.note, i.is_admin, c.username AS created_by,
                i.created_at, i.expires_at, i.used_at, NULL::text AS used_by
           FROM invites i LEFT JOIN users c ON c.id = i.created_by
          WHERE i.id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(CreatedInvite { invite, url, emailed, email_error }))
}

/// Revoke an invite. Spent ones are kept as a record of who let whom in, so
/// this only deletes those still outstanding.
async fn revoke_invite(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let admin = require_admin(&viewer).await?;
    let done = sqlx::query("DELETE FROM invites WHERE id = $1 AND used_at IS NULL")
        .bind(id)
        .execute(&state.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::bad("that invite has already been used or does not exist"));
    }
    super::audit(&state, Some(admin.id), None, "invite.revoke",
        serde_json::json!({ "invite": id })).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn taken(state: &AppState, email: &str) -> AppResult<bool> {
    let n: (i64,) = sqlx::query_as("SELECT count(*) FROM users WHERE email = $1")
        .bind(email)
        .fetch_one(&state.db)
        .await?;
    Ok(n.0 > 0)
}
