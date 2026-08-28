//! Registration, login, logout, and personal access tokens.

use crate::auth::{self, Viewer};
use crate::error::{AppError, AppResult};
use crate::models::*;
use crate::state::AppState;
use crate::ratelimit::{client_ip, Quota};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{Duration, Utc};
use std::net::SocketAddr;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/meta", get(meta))
        .route("/auth/register", post(register))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/invite", get(peek_invite))
        .route("/auth/forgot", post(forgot_password))
        .route("/auth/reset", post(reset_password))
        .route("/auth/me", get(me).patch(update_me))
        .route("/auth/password", post(change_password))
        .route("/auth/sessions", get(list_sessions).delete(revoke_other_sessions))
        .route("/auth/sessions/{id}", delete(revoke_session))
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/{id}", delete(revoke_token))
}


// ---- rate limiting --------------------------------------------------------
//
// These are the endpoints that are cheap to ask for and expensive to answer:
// a password guess costs an Argon2 verification, a forgotten-password request
// costs an email. Argon2 makes each attempt slow for the server, not for the
// attacker, so the ceiling has to be explicit.
//
// Quotas are per window per key and deliberately generous — they exist to stop
// a script, not to inconvenience someone who mistypes a password twice.

/// Ten a minute stops guessing without troubling a real person.
const LOGIN_PER_IP: Quota = Quota::per_minute(10);
/// Failures against one account, from anywhere. Higher than the per-IP limit
/// because it is the one an attacker can reach from many addresses, and it is
/// cleared on success so it cannot be used to lock somebody out.
const LOGIN_PER_USER: Quota = Quota::per_hour(30);
/// Attempts, including the ones that fail validation. Generous, because a
/// person who fumbles the form three times is not an attacker and must not be
/// locked out for an hour — this only exists so the endpoint cannot be
/// hammered into doing unbounded Argon2 work.
const REGISTER_TRIES_PER_IP: Quota = Quota::per_hour(20);
/// Accounts actually created. Counted only on success, so it caps farming
/// without a fumbled form ever touching it.
const REGISTER_MADE_PER_IP: Quota = Quota::per_hour(5);
/// Sending mail costs money and reputation, so this is the tightest one.
const FORGOT_PER_IP: Quota = Quota::per_hour(5);
/// Also per address, so one mailbox cannot be flooded from many machines.
const FORGOT_PER_EMAIL: Quota = Quota::per_hour(3);
const RESET_PER_IP: Quota = Quota::per_hour(10);
/// An invite token is 256 bits, so this is not about guessing it — it stops
/// the endpoint being a free oracle to hammer.
const INVITE_PEEK_PER_IP: Quota = Quota::per_hour(30);
const TOKEN_CREATE_PER_USER: Quota = Quota::per_hour(20);

/// Count one request and turn a refusal into a 429 carrying `Retry-After`.
async fn limit(state: &AppState, scope: &str, id: &str, quota: Quota) -> AppResult<()> {
    let key = format!("{scope}:{id}");
    let d = state.limiter.check(&key, quota).await;
    if d.allowed {
        Ok(())
    } else {
        Err(AppError::TooManyRequests(d.retry_after))
    }
}

/// The address to charge a request to. See [`crate::ratelimit::client_ip`] for
/// why this is not simply the peer.
fn who(state: &AppState, headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    client_ip(headers, peer, state.trust_proxy)
}

/// Instance policy, readable without authentication.
///
/// This is deliberately the *only* thing an anonymous caller learns about a
/// locked-down server: whether it will accept a sign-up and whether it needs a
/// login. It exposes no names, counts, or versions.
async fn meta(State(state): State<AppState>) -> Json<serde_json::Value> {
    let p = state.policy();

    // A server with no accounts always accepts one — `register` lets the first
    // through whatever the policy says, so that an instance shipped with
    // registration closed is not locked out of itself. The sign-in page needs
    // to know, or it hides the only form that would work.
    let empty: bool = sqlx::query_scalar("SELECT NOT EXISTS (SELECT 1 FROM users)")
        .fetch_one(&state.db)
        .await
        .unwrap_or(false);

    Json(serde_json::json!({
        "site_name": p.site_name,
        "email_enabled": p.email_configured(),
        "open_registration": p.open_registration,
        "require_auth": p.require_auth,
        "default_repo_visibility": p.default_repo_visibility,
        "needs_setup": empty,
    }))
}

async fn register(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<RegisterReq>,
) -> AppResult<impl IntoResponse> {
    let ip = who(&state, &headers, Some(peer));
    limit(&state, "register-try", &ip, REGISTER_TRIES_PER_IP).await?;

    let username = auth::normalize_username(&body.username)?;
    auth::validate_password(&body.password)?;

    let email = auth::validate_email(&body.email)?;

    // The very first account to register becomes the server administrator.
    let count: (i64,) = sqlx::query_as("SELECT count(*) FROM users")
        .fetch_one(&state.db)
        .await?;
    let is_admin = count.0 == 0;

    let policy = state.policy();

    // An invitation is checked before anything else it might excuse. It is
    // consumed further down, only once the account has actually been created —
    // a failed registration must not burn the link.
    let invite = match body.invite.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(token) => Some(claim_invite(&state, token, &email).await?),
        None => None,
    };

    // An invite to a specific address is proof enough for that address; a
    // domain policy is about who may sign *themselves* up.
    if invite.is_none() && !policy.email_allowed(&email) {
        return Err(AppError::bad(
            "that email domain is not permitted on this server",
        ));
    }
    // A closed instance still allows the very first (admin) account, so a fresh
    // private server is not locked out of itself.
    if !policy.open_registration && !is_admin && invite.is_none() {
        return Err(AppError::Forbidden(
            "registration is closed on this server — ask an administrator for an invite".into(),
        ));
    }

    let is_admin = is_admin || invite.as_ref().is_some_and(|i| i.is_admin);
    let id = Uuid::new_v4();
    let hash = auth::hash_secret(&body.password)?;

    let inserted = sqlx::query(
        "INSERT INTO users (id, username, email, password_hash, is_admin)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(&username)
    .bind(&email)
    .bind(&hash)
    .bind(is_admin)
    .execute(&state.db)
    .await;

    if let Err(sqlx::Error::Database(e)) = &inserted
        && e.is_unique_violation()
    {
        return Err(AppError::conflict("that username or email is already taken"));
    }
    inserted?;

    // Counted here rather than at the top: the account exists, so this is the
    // quota that caps farming. A form fumbled five times never reaches it.
    limit(&state, "register-made", &ip, REGISTER_MADE_PER_IP).await?;

    // The account exists, so the invite is now spent. The UPDATE re-checks
    // `used_at IS NULL`, so two people racing the same link cannot both win it;
    // the loser has an account either way, which is the harmless outcome.
    if let Some(i) = &invite {
        sqlx::query("UPDATE invites SET used_at = now(), used_by = $1 WHERE id = $2 AND used_at IS NULL")
            .bind(id)
            .bind(i.id)
            .execute(&state.db)
            .await?;
        super::audit(&state, Some(id), None, "invite.redeem",
            serde_json::json!({ "invite": i.id })).await;
    }

    // Any other outstanding invite to this address can never be redeemed now —
    // the address is taken — so retire them instead of leaving dead links in
    // the administrator's list.
    sqlx::query("UPDATE invites SET used_at = now() WHERE lower(email) = lower($1) AND used_at IS NULL")
        .bind(&email)
        .execute(&state.db)
        .await?;

    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    let session = auth::create_session(&state.db, id, ua).await?;
    let cookie = auth::session_cookie(&session.secret, session.expires_at, state.secure_cookies);

    let user: UserRow = sqlx::query_as("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;

    Ok((
        StatusCode::CREATED,
        [(header::SET_COOKIE, cookie)],
        Json(SelfView { user: UserView::from(&user), email: user.email.clone() }),
    ))
}

struct ClaimedInvite {
    id: Uuid,
    is_admin: bool,
}

/// Look up an invite by its token and check it is still good for this address.
///
/// Every failure returns the same message. The token is a secret; telling a
/// stranger holding a guess whether it merely expired is telling them the guess
/// was right.
async fn claim_invite(state: &AppState, token: &str, email: &str) -> AppResult<ClaimedInvite> {
    let bad = || AppError::Forbidden("that invitation is invalid, expired or already used".into());

    let row: Option<(Uuid, Option<String>, bool)> = sqlx::query_as(
        "SELECT id, email, is_admin FROM invites
          WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()",
    )
    .bind(auth::token_digest(token))
    .fetch_optional(&state.db)
    .await?;

    let (id, bound, is_admin) = row.ok_or_else(bad)?;

    // An invite addressed to someone is not transferable: the whole point of
    // naming the address is that this link admits that person and no one else.
    if let Some(want) = bound
        && !want.eq_ignore_ascii_case(email)
    {
        return Err(AppError::bad(
            "this invitation was sent to a different address — register with that one",
        ));
    }

    Ok(ClaimedInvite { id, is_admin })
}

/// Whether a `/register?invite=…` token is worth showing a form for.
///
/// The registration page needs to know before asking for a password, and this
/// says only yes-or-no plus the address it is bound to, so the form can be
/// pre-filled and locked.
async fn peek_invite(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<InviteQuery>,
) -> AppResult<Json<serde_json::Value>> {
    limit(&state, "invite-peek", &who(&state, &headers, Some(peer)), INVITE_PEEK_PER_IP).await?;

    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT email FROM invites
          WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()",
    )
    .bind(auth::token_digest(q.token.trim()))
    .fetch_optional(&state.db)
    .await?;

    Ok(Json(match row {
        Some((email,)) => serde_json::json!({ "valid": true, "email": email }),
        None => serde_json::json!({ "valid": false, "email": null }),
    }))
}

#[derive(Deserialize)]
struct InviteQuery {
    token: String,
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginReq>,
) -> AppResult<impl IntoResponse> {
    let username = body.username.trim().to_ascii_lowercase();

    limit(&state, "login-ip", &who(&state, &headers, Some(peer)), LOGIN_PER_IP).await?;
    limit(&state, "login-user", &username, LOGIN_PER_USER).await?;

    let user: Option<UserRow> = sqlx::query_as("SELECT * FROM users WHERE username = $1")
        .bind(&username)
        .fetch_optional(&state.db)
        .await?;

    // Verify against a dummy hash when the user does not exist, so the response
    // time does not reveal whether the account is real.
    const DUMMY: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHR2YWx1ZQ$\
                         d2hvY2FyZXN0aGlzaXNuZXZlcnZhbGlkYXRlZA";
    let ok = match &user {
        Some(u) => auth::verify_secret(&body.password, &u.password_hash),
        None => {
            let _ = auth::verify_secret(&body.password, DUMMY);
            false
        }
    };

    if !ok {
        return Err(AppError::Unauthorized);
    }
    let user = user.expect("verified above");

    // Proving you own the account clears its failure count, so an attacker
    // guessing at someone else's username cannot lock them out of their own.
    state.limiter.reset(&format!("login-user:{username}")).await;

    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    let session = auth::create_session(&state.db, user.id, ua).await?;
    let cookie = auth::session_cookie(&session.secret, session.expires_at, state.secure_cookies);

    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(SelfView { user: UserView::from(&user), email: user.email.clone() }),
    ))
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(raw) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        for kv in raw.split(';') {
            if let Some((k, v)) = kv.trim().split_once('=')
                && k == auth::SESSION_COOKIE
            {
                auth::destroy_session(&state.db, v).await;
            }
        }
    }
    (
        [(header::SET_COOKIE, auth::clear_cookie(state.secure_cookies))],
        Json(serde_json::json!({ "ok": true })),
    )
}

/// Reset links are short-lived: long enough to find the mail, short enough that
/// a link left in an inbox is not a standing key to the account.
const RESET_MINUTES: i64 = 30;

#[derive(Debug, Deserialize)]
struct ForgotReq {
    email: String,
}

/// Begin a password reset.
///
/// Always reports success, whatever happened. Saying "no account with that
/// email" turns this endpoint into a membership oracle — anyone could test a
/// list of addresses against the server. The person who owns the address learns
/// the truth from their inbox; nobody else learns anything.
async fn forgot_password(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<ForgotReq>,
) -> AppResult<Json<serde_json::Value>> {
    limit(&state, "forgot-ip", &who(&state, &headers, Some(peer)), FORGOT_PER_IP).await?;
    // Keyed by a digest, not the address itself: these keys may end up in an
    // external store, and that store should not become a list of who has an
    // account here.
    let email_key = auth::token_digest(&body.email.trim().to_ascii_lowercase());
    limit(&state, "forgot-email", &email_key, FORGOT_PER_EMAIL).await?;

    let quiet_ok = || {
        Ok(Json(serde_json::json!({
            "ok": true,
            "message": "If that address has an account, a reset link is on its way."
        })))
    };

    let policy = state.policy();
    if !policy.email_configured() {
        return Err(AppError::bad(
            "this server cannot send email, so passwords cannot be reset from here — \
             ask an administrator",
        ));
    }
    let Ok(email) = auth::validate_email(&body.email) else {
        return quiet_ok();
    };

    let found: Option<(Uuid, String, bool)> =
        sqlx::query_as("SELECT id, username, is_active FROM users WHERE email = $1")
            .bind(&email)
            .fetch_optional(&state.db)
            .await?;

    let Some((user_id, username, active)) = found else {
        return quiet_ok();
    };
    if !active {
        // A disabled account should not be resettable back into use.
        return quiet_ok();
    }

    // Any earlier outstanding link becomes void, so a person who clicks twice
    // does not end up with two live keys to their account.
    sqlx::query("UPDATE password_resets SET used_at = now() WHERE user_id = $1 AND used_at IS NULL")
        .bind(user_id)
        .execute(&state.db)
        .await?;

    let secret = auth::random_token();
    sqlx::query(
        "INSERT INTO password_resets (id, user_id, token_hash, expires_at)
         VALUES ($1, $2, $3, now() + ($4 || ' minutes')::interval)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(auth::token_digest(&secret))
    .bind(RESET_MINUTES.to_string())
    .execute(&state.db)
    .await?;

    let base = policy.public_url.trim_end_matches('/').to_string();
    let link = format!("{base}/reset?token={secret}");

    if let Some(mailer) = crate::email::Mailer::new(policy.resend_api_key.as_deref(), &policy.email_from)
    {
        // A delivery failure must not tell the caller whether the account
        // exists, so it is logged and swallowed.
        if let Err(e) = mailer
            .send(
                &email,
                "Reset your fkit hub password",
                &crate::email::reset_body(&username, &link, RESET_MINUTES),
            )
            .await
        {
            tracing::error!("password reset email failed: {e:#}");
            // The account is now holding a live link nobody received. The
            // server operator can read it here; it goes nowhere else.
            tracing::warn!("undelivered reset link for {username}: {link}");
        }
    }

    super::audit(&state, None, None, "auth.forgot", serde_json::json!({ "user": user_id })).await;
    quiet_ok()
}

#[derive(Debug, Deserialize)]
struct ResetReq {
    token: String,
    password: String,
}

async fn reset_password(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<ResetReq>,
) -> AppResult<impl IntoResponse> {
    limit(&state, "reset", &who(&state, &headers, Some(peer)), RESET_PER_IP).await?;

    auth::validate_password(&body.password)?;

    // One indexed lookup on the digest; the token itself is never stored.
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, user_id FROM password_resets
         WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()",
    )
    .bind(auth::token_digest(&body.token))
    .fetch_optional(&state.db)
    .await?;

    let Some((reset_id, user_id)) = row else {
        return Err(AppError::bad(
            "that reset link is invalid or has expired — request a new one",
        ));
    };

    let hash = auth::hash_secret(&body.password)?;
    let mut tx = state.db.begin().await?;

    // Spend the token inside the same transaction that changes the password, so
    // two simultaneous uses of one link cannot both succeed.
    let spent = sqlx::query("UPDATE password_resets SET used_at = now() WHERE id = $1 AND used_at IS NULL")
        .bind(reset_id)
        .execute(&mut *tx)
        .await?;
    if spent.rows_affected() == 0 {
        return Err(AppError::bad("that reset link has already been used"));
    }

    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(user_id)
        .bind(&hash)
        .execute(&mut *tx)
        .await?;

    // A reset is a recovery from losing control of the account: drop every
    // session and token so whoever else had access loses it.
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM access_tokens WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    let session = auth::create_session(&state.db, user_id, ua).await?;
    let cookie = auth::session_cookie(&session.secret, session.expires_at, state.secure_cookies);

    super::audit(&state, Some(user_id), None, "auth.reset", serde_json::json!({})).await;

    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({
            "ok": true,
            "message": "Password changed. All other sessions and access tokens were revoked."
        })),
    ))
}

async fn me(State(state): State<AppState>, viewer: Viewer) -> AppResult<Json<SelfView>> {
    let u = viewer.require()?;
    let user: UserRow = sqlx::query_as("SELECT * FROM users WHERE id = $1")
        .bind(u.id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(SelfView { user: UserView::from(&user), email: user.email.clone() }))
}

#[derive(Debug, Deserialize)]
struct ProfilePatch {
    display_name: Option<String>,
    email: Option<String>,
}

async fn update_me(
    State(state): State<AppState>,
    viewer: Viewer,
    Json(body): Json<ProfilePatch>,
) -> AppResult<Json<SelfView>> {
    let u = viewer.require()?;

    let email = body.email.map(|e| auth::validate_email(&e)).transpose()?;
    // `display_name` is free text and may be cleared, so an empty string is a
    // deliberate "remove it" rather than "leave it alone".
    let display = body.display_name.map(|d| d.trim().to_string());

    let res = sqlx::query(
        "UPDATE users SET
            display_name = COALESCE($2, display_name),
            email        = COALESCE($3, email)
         WHERE id = $1",
    )
    .bind(u.id)
    .bind(display.as_ref().filter(|d| !d.is_empty()))
    .bind(&email)
    .execute(&state.db)
    .await;

    if let Err(sqlx::Error::Database(e)) = &res
        && e.is_unique_violation()
    {
        return Err(AppError::conflict("that email is already in use"));
    }
    res?;

    let user: UserRow = sqlx::query_as("SELECT * FROM users WHERE id = $1")
        .bind(u.id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(SelfView { user: UserView::from(&user), email: user.email.clone() }))
}

#[derive(Debug, Deserialize)]
struct PasswordChange {
    current: String,
    new: String,
}

async fn change_password(
    State(state): State<AppState>,
    viewer: Viewer,
    headers: HeaderMap,
    Json(body): Json<PasswordChange>,
) -> AppResult<impl IntoResponse> {
    let u = viewer.require()?;
    auth::validate_password(&body.new)?;

    let row: (String,) = sqlx::query_as("SELECT password_hash FROM users WHERE id = $1")
        .bind(u.id)
        .fetch_one(&state.db)
        .await?;

    // Requiring the current password is what stops a stolen session from
    // becoming a permanent account takeover.
    if !auth::verify_secret(&body.current, &row.0) {
        return Err(AppError::Unauthorized);
    }

    let hash = auth::hash_secret(&body.new)?;
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(u.id)
        .bind(&hash)
        .execute(&state.db)
        .await?;

    // Every other session is now suspect; drop them all and re-issue one here,
    // so changing a password after a compromise actually evicts the attacker.
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(u.id)
        .execute(&state.db)
        .await?;

    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    let session = auth::create_session(&state.db, u.id, ua).await?;
    let cookie = auth::session_cookie(&session.secret, session.expires_at, state.secure_cookies);

    super::audit(&state, Some(u.id), None, "user.password", serde_json::json!({})).await;

    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({ "ok": true, "other_sessions_revoked": true })),
    ))
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: Uuid,
    user_agent: Option<String>,
    created_at: chrono::DateTime<Utc>,
    expires_at: chrono::DateTime<Utc>,
}

#[derive(Serialize)]
struct SessionView {
    id: Uuid,
    user_agent: Option<String>,
    created_at: chrono::DateTime<Utc>,
    expires_at: chrono::DateTime<Utc>,
    /// The browser making this request. Revoking it signs you out here.
    current: bool,
}

async fn list_sessions(
    State(state): State<AppState>,
    viewer: Viewer,
) -> AppResult<Json<Vec<SessionView>>> {
    let u = viewer.require()?;
    let rows: Vec<SessionRow> = sqlx::query_as(
        "SELECT id, user_agent, created_at, expires_at FROM sessions
         WHERE user_id = $1 AND expires_at > now() ORDER BY created_at DESC",
    )
    .bind(u.id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| SessionView {
                current: Some(r.id) == u.session_id,
                id: r.id,
                user_agent: r.user_agent,
                created_at: r.created_at,
                expires_at: r.expires_at,
            })
            .collect(),
    ))
}

/// Sign out everywhere except here.
///
/// The common reason to open this page is a suspicion that something else is
/// signed in; doing that one row at a time is the wrong shape for the task.
async fn revoke_other_sessions(
    State(state): State<AppState>,
    viewer: Viewer,
) -> AppResult<Json<serde_json::Value>> {
    let u = viewer.require()?;
    let done = sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND id <> $2")
        .bind(u.id)
        // A bearer token has no session; `Uuid::nil()` matches nothing, so the
        // call still clears every browser session for the account.
        .bind(u.session_id.unwrap_or_else(Uuid::nil))
        .execute(&state.db)
        .await?;
    Ok(Json(serde_json::json!({ "revoked": done.rows_affected() })))
}

async fn revoke_session(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let u = viewer.require()?;
    let done = sqlx::query("DELETE FROM sessions WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(u.id)
        .execute(&state.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::not_found("no such session"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn list_tokens(State(state): State<AppState>, viewer: Viewer) -> AppResult<Json<Vec<TokenView>>> {
    let u = viewer.require()?;
    // Named row type: the tuple form was long enough that its column order was
    // easy to get wrong when editing the SELECT.
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        name: String,
        prefix: String,
        can_write: bool,
        created_at: chrono::DateTime<Utc>,
        last_used_at: Option<chrono::DateTime<Utc>>,
        expires_at: Option<chrono::DateTime<Utc>>,
    }

    let rows: Vec<Row> =
        sqlx::query_as(
            "SELECT id, name, prefix, can_write, created_at, last_used_at, expires_at
             FROM access_tokens WHERE user_id = $1 ORDER BY created_at DESC",
        )
        .bind(u.id)
        .fetch_all(&state.db)
        .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| TokenView {
                id: r.id,
                name: r.name,
                prefix: r.prefix,
                can_write: r.can_write,
                created_at: r.created_at,
                last_used_at: r.last_used_at,
                expires_at: r.expires_at,
            })
            .collect(),
    ))
}

async fn create_token(
    State(state): State<AppState>,
    viewer: Viewer,
    Json(body): Json<CreateTokenReq>,
) -> AppResult<impl IntoResponse> {
    let u = viewer.require()?;
    limit(&state, "token-create", &u.id.to_string(), TOKEN_CREATE_PER_USER).await?;

    let name = body.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(AppError::bad("token name must be 1-64 characters"));
    }

    let minted = auth::mint_token()?;
    let id = Uuid::new_v4();
    let expires = body.expires_in_days.map(|d| Utc::now() + Duration::days(d));

    sqlx::query(
        "INSERT INTO access_tokens (id, user_id, name, prefix, token_hash, can_write, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(u.id)
    .bind(name)
    .bind(&minted.prefix)
    .bind(&minted.hash)
    .bind(body.can_write)
    .bind(expires)
    .execute(&state.db)
    .await?;

    super::audit(&state, Some(u.id), None, "token.create",
        serde_json::json!({ "name": name, "can_write": body.can_write })).await;

    Ok((
        StatusCode::CREATED,
        Json(NewTokenView {
            token: TokenView {
                id,
                name: name.to_string(),
                prefix: minted.prefix,
                can_write: body.can_write,
                created_at: Utc::now(),
                last_used_at: None,
                expires_at: expires,
            },
            // Shown exactly once. There is no endpoint that can return it again.
            secret: minted.secret,
        }),
    ))
}

async fn revoke_token(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<Uuid>,
) -> AppResult<Json<serde_json::Value>> {
    let u = viewer.require()?;
    let done = sqlx::query("DELETE FROM access_tokens WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(u.id)
        .execute(&state.db)
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::not_found("no such token"));
    }
    super::audit(&state, Some(u.id), None, "token.revoke", serde_json::json!({ "id": id })).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}
