//! Passwords, sessions, personal access tokens, and the request extractor that
//! turns any of them into a `Viewer`.
//!
//! # What is stored
//!
//! Nothing that can be replayed. A dump of this database yields no credential
//! that can be presented back to the server.
//!
//! # Two kinds of secret, two kinds of hashing
//!
//! **Passwords** are low-entropy and chosen by humans, so they get Argon2id: the
//! whole point is to make each guess expensive.
//!
//! **Session tokens and access tokens** are 256 bits straight from the OS
//! CSPRNG. There is no dictionary to run against them and no amount of hashing
//! speed helps an attacker, so a slow KDF buys nothing — while costing ~15 ms on
//! *every authenticated request*. They get a plain BLAKE3 digest instead, which
//! keeps the "a database leak is not directly replayable" property and turns
//! authentication into a single indexed equality lookup.
//!
//! Getting this backwards is a common and expensive mistake: it looks more
//! secure, and is actually just slower.
//!
//! Tokens still carry a public `prefix` segment, now purely so the UI can show
//! you which token is which without ever holding the secret.

use crate::error::{AppError, AppResult};
use crate::models::UserRow;
use crate::state::AppState;
use argon2::password_hash::{phc::PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

pub const SESSION_COOKIE: &str = "fkit_session";
pub const SESSION_DAYS: i64 = 30;
const TOKEN_PREFIX: &str = "fkit_pat";

/// 32 bytes from the OS CSPRNG, hex-encoded.
/// A fresh 256-bit secret, hex encoded. Used for sessions, access tokens and
/// password resets — anywhere an unguessable value is handed to someone.
pub fn random_token() -> String {
    random_hex(32)
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("system RNG failure");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Digest for high-entropy tokens. Fast on purpose — see the module docs.
pub fn token_digest(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

/// Argon2id, for passwords only.
pub fn hash_secret(secret: &str) -> AppResult<String> {
    let hash: PasswordHash = Argon2::default()
        .hash_password(secret.as_bytes())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("hashing failed: {e}")))?;
    Ok(hash.to_string())
}

/// Constant-time by construction: Argon2's verifier compares digests, not the
/// inputs, and does not short-circuit.
pub fn verify_secret(secret: &str, stored: &str) -> bool {
    match PasswordHash::new(stored) {
        Ok(parsed) => Argon2::default()
            .verify_password(secret.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Validate an email address structurally.
///
/// Not RFC 5322 — that grammar admits quoted strings, comments and nested
/// parentheses that no registration form should accept. This checks the shape
/// real addresses actually take, and rejects the things that cause trouble
/// later: no domain, no dot in the domain, a trailing dot, doubled dots,
/// or a hyphen where a label boundary should be.
///
/// Deliverability is not knowable from syntax; a confirmation email is the only
/// real test. This exists to stop typos and obvious garbage.
pub fn validate_email(raw: &str) -> AppResult<String> {
    let email = raw.trim().to_ascii_lowercase();
    let bad = |m: &str| AppError::bad(format!("that email address is not valid: {m}"));

    if email.len() > 254 {
        return Err(bad("it is too long"));
    }
    // Exactly one @, and something on each side.
    let mut parts = email.split('@');
    let (local, domain) = match (parts.next(), parts.next(), parts.next()) {
        (Some(l), Some(d), None) if !l.is_empty() && !d.is_empty() => (l, d),
        _ => return Err(bad("it needs exactly one @ with text either side")),
    };

    if local.len() > 64 {
        return Err(bad("the part before @ is too long"));
    }
    if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return Err(bad("the part before @ has a misplaced dot"));
    }
    if !local
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ".!#$%&'*+/=?^_`{|}~-".contains(c))
    {
        return Err(bad("the part before @ has an unusable character"));
    }

    // A domain needs at least two labels: `user@localhost` is valid on a mail
    // server and useless on the public internet, which is where this is going.
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return Err(bad("the domain needs a dot, like example.com"));
    }
    for label in &labels {
        if label.is_empty() || label.len() > 63 {
            return Err(bad("a part of the domain is empty or too long"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(bad("a part of the domain starts or ends with a hyphen"));
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(bad("the domain has an unusable character"));
        }
    }
    let tld = labels.last().expect("checked non-empty");
    if tld.len() < 2 || !tld.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(bad("the last part of the domain is not a valid suffix"));
    }

    Ok(email)
}

pub fn validate_password(pw: &str) -> AppResult<()> {
    // Length is the only requirement that reliably correlates with strength.
    // Composition rules ("must contain a symbol") push people toward
    // `Password1!` and are not imposed here.
    if pw.chars().count() < 10 {
        return Err(AppError::bad("password must be at least 10 characters"));
    }
    if pw.len() > 1024 {
        return Err(AppError::bad("password is implausibly long"));
    }
    Ok(())
}

/// Names nobody may register.
///
/// Three separate reasons, deliberately kept in one list because the cost of an
/// over-broad reservation is one person picking a different handle, and the cost
/// of a gap is a URL collision or a convincing impersonation.
///
/// 1. **Route collisions.** A username becomes a top-level path
///    (`/{owner}/{repo}`), so anything that is or might become a real route has
///    to be off limits. The forward-looking entries matter as much as the
///    current ones: reserving `docs` costs nothing today and avoids having to
///    rename somebody's account the day a docs page ships.
///
/// 2. **Impersonation.** `admin`, `security`, `support` and friends carry
///    implied authority. Someone opening a merge request as `security` is a
///    social-engineering primitive, not a username.
///
/// 3. **Infrastructure and mail.** `noreply`, `postmaster`, `www` and similar
///    are conventionally system-owned; a user holding one can intercept
///    conventions other software assumes.
const RESERVED: &[&str] = &[
    // -- routes that exist today --
    "api", "admin", "assets", "static", "login", "logout", "register", "new",
    "settings", "explore", "search", "help", "about", "_health",
    // Link previews. `/og/...` and `/oembed` are fetched by crawlers exactly
    // as published, so they are static routes and would shadow an account.
    "og", "oembed",
    // -- routes a forge tends to grow --
    "blog", "docs", "documentation", "status", "pricing", "terms", "privacy",
    "legal", "contact", "download", "downloads", "changelog", "releases",
    "notifications", "dashboard", "profile", "account", "billing", "invoices",
    "signin", "signup", "sso", "oauth", "auth", "session", "sessions",
    "organizations", "orgs", "teams", "users", "user", "repos", "repositories",
    // -- authority and impersonation --
    "administrator", "root", "sysadmin", "superuser", "system", "daemon",
    "operator", "owner", "moderator", "mod", "staff", "official", "security",
    "support", "abuse", "webmaster", "postmaster", "hostmaster", "noreply",
    "no-reply", "donotreply", "mailer-daemon", "info", "sales", "marketing",
    // -- infrastructure conventions --
    "www", "mail", "smtp", "imap", "pop", "pop3", "ftp", "ns", "ns1", "ns2",
    "dns", "cdn", "media", "img", "images", "files", "uploads", "cache",
    "localhost", "test", "example",
    // -- names this project itself uses --
    "fkit", "fkitd", "hub", "objects", "refs", "head", "git", "ssh",
];

/// Is this name reserved? Exposed so a signup form can say so before submitting.
pub fn is_reserved(name: &str) -> bool {
    RESERVED.contains(&name)
}

pub fn normalize_username(raw: &str) -> AppResult<String> {
    let name = raw.trim().to_ascii_lowercase();

    if name.is_empty() || name.len() > 39 {
        return Err(AppError::bad("username must be 1-39 characters"));
    }

    // ASCII only. Unicode would allow a homograph attack — `travıs` with a
    // dotless i renders almost identically to `travis` in most typefaces, and a
    // reader confirming a merge request has no way to tell them apart.
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(AppError::bad(
            "username may contain only a-z, 0-9, dot, underscore and hyphen",
        ));
    }

    // Must start and end with something substantive. A trailing separator makes
    // `travis-` and `travis` hard to tell apart in a list.
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err(AppError::bad("username must start with a letter or digit"));
    }
    if !name.ends_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err(AppError::bad("username must end with a letter or digit"));
    }

    // `a..b` and `a__b` are near-invisible variations on `a.b`.
    if name
        .as_bytes()
        .windows(2)
        .any(|w| !w[0].is_ascii_alphanumeric() && !w[1].is_ascii_alphanumeric())
    {
        return Err(AppError::bad(
            "username may not contain two separators in a row",
        ));
    }

    // A name that is only digits would be indistinguishable from an id in any
    // future URL that accepts either.
    if name.chars().all(|c| c.is_ascii_digit()) {
        return Err(AppError::bad("username must contain at least one letter"));
    }

    if is_reserved(&name) {
        return Err(AppError::bad(format!(
            "'{name}' is reserved and cannot be registered"
        )));
    }

    Ok(name)
}

// ---- sessions -----------------------------------------------------------

pub struct IssuedSession {
    pub secret: String,
    pub expires_at: DateTime<Utc>,
}

pub async fn create_session(
    db: &sqlx::PgPool,
    user_id: Uuid,
    user_agent: Option<&str>,
) -> AppResult<IssuedSession> {
    let secret = random_hex(32);
    let expires_at = Utc::now() + Duration::days(SESSION_DAYS);
    sqlx::query(
        "INSERT INTO sessions (id, user_id, token_hash, user_agent, expires_at)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(token_digest(&secret))
    .bind(user_agent)
    .bind(expires_at)
    .execute(db)
    .await?;
    Ok(IssuedSession { secret, expires_at })
}

/// One indexed lookup: digest the presented cookie and join straight to the
/// user. No candidate scan, no per-request KDF.
pub async fn lookup_session(db: &sqlx::PgPool, cookie: &str) -> Option<(UserRow, Uuid)> {
    let row: (Uuid,) = sqlx::query_as(
        "SELECT id FROM sessions WHERE token_hash = $1 AND expires_at > now()",
    )
    .bind(token_digest(cookie))
    .fetch_optional(db)
    .await
    .ok()??;

    // The session id travels with the viewer so the sessions list can mark
    // which row is the browser asking — revoking your own session by accident
    // because every row looks alike is a small disaster.
    // Filtered here rather than only at login, so disabling an account ends
    // the sessions it already has instead of waiting for them to expire.
    let user = sqlx::query_as::<_, UserRow>(
        "SELECT u.* FROM sessions s JOIN users u ON u.id = s.user_id
          WHERE s.id = $1 AND u.is_active",
    )
    .bind(row.0)
    .fetch_optional(db)
    .await
    .ok()??;
    Some((user, row.0))
}

pub async fn destroy_session(db: &sqlx::PgPool, cookie: &str) {
    let _ = sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(token_digest(cookie))
        .execute(db)
        .await;
}

pub fn session_cookie(secret_with_id: &str, expires: DateTime<Utc>, secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}={secret_with_id}; Path=/; HttpOnly; SameSite=Lax; Expires={}{}",
        expires.format("%a, %d %b %Y %H:%M:%S GMT"),
        if secure { "; Secure" } else { "" }
    )
}

pub fn clear_cookie(secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}

// ---- personal access tokens ---------------------------------------------

pub struct IssuedToken {
    pub secret: String,
    pub prefix: String,
    pub hash: String,
}

pub fn mint_token() -> AppResult<IssuedToken> {
    let prefix = random_hex(6); // 12 hex chars, public — for display only
    let secret = random_hex(32); // 64 hex chars, 256 bits of entropy
    let full = format!("{TOKEN_PREFIX}_{prefix}_{secret}");
    Ok(IssuedToken {
        // The digest covers the whole presented string, so a token cannot be
        // replayed under a different prefix.
        hash: token_digest(&full),
        secret: full,
        prefix,
    })
}

/// A token's owner, and the two things the token itself decides.
pub struct TokenAuth {
    pub user: UserRow,
    pub can_write: bool,
    /// Link what this token pushes to its owner. Off for a mirror, which
    /// carries other people's history.
    pub attributes: bool,
}

/// Verify `fkit_pat_<prefix>_<secret>` in one indexed lookup.
pub async fn lookup_token(db: &sqlx::PgPool, presented: &str) -> Option<TokenAuth> {
    if !presented.starts_with(TOKEN_PREFIX) {
        return None;
    }

    let row: (Uuid, Uuid, bool, bool) = sqlx::query_as(
        "SELECT t.id, t.user_id, t.can_write, t.attributes FROM access_tokens t
         WHERE t.token_hash = $1 AND (t.expires_at IS NULL OR t.expires_at > now())",
    )
    .bind(token_digest(presented))
    .fetch_optional(db)
    .await
    .ok()??;

    // Best-effort; a failure here must not fail the request.
    let _ = sqlx::query("UPDATE access_tokens SET last_used_at = now() WHERE id = $1")
        .bind(row.0)
        .execute(db)
        .await;

    // Same for a token: a disabled account's credentials stop working at once,
    // which is what an administrator pressing "disable" is asking for.
    let user = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE id = $1 AND is_active")
        .bind(row.1)
        .fetch_optional(db)
        .await
        .ok()??;
    Some(TokenAuth { user, can_write: row.2, attributes: row.3 })
}

// ---- the request extractor ----------------------------------------------

/// Who is making this request, and by what means.
#[derive(Debug, Clone)]
pub struct Viewer {
    pub user: Option<ViewerUser>,
}

#[derive(Debug, Clone)]
pub struct ViewerUser {
    pub id: Uuid,
    pub username: String,
    pub is_admin: bool,
    /// What this account may do to the instance: create repositories,
    /// administer it, or only take part in what already exists.
    pub site_role: crate::perms::SiteRole,
    /// Set when the caller authenticated with a browser session.
    pub session_id: Option<Uuid>,
    /// False for a read-only personal access token, regardless of repo role.
    /// A token can only ever *narrow* what its owner may do.
    pub can_write: bool,
}

impl Viewer {
    pub fn anonymous() -> Self {
        Viewer { user: None }
    }
    pub fn require(&self) -> AppResult<&ViewerUser> {
        self.user.as_ref().ok_or(AppError::Unauthorized)
    }
    pub fn id(&self) -> Option<Uuid> {
        self.user.as_ref().map(|u| u.id)
    }
}

fn cookie_value<'a>(parts: &'a Parts, name: &str) -> Option<&'a str> {
    parts
        .headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

impl FromRequestParts<AppState> for Viewer {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        // A bearer token wins over a cookie: an explicit credential should not
        // be silently overridden by an ambient one.
        let bearer = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string);

        if let Some(tok) = bearer
            && let Some(TokenAuth { user, can_write, .. }) =
                lookup_token(&state.db, &tok).await
        {
            return Ok(Viewer {
                user: Some(ViewerUser {
                    id: user.id,
                    username: user.username,
                    is_admin: user.is_admin,
                    // An unrecognised value is the least privilege, never the
                    // most: a row that somehow says "root" must not administer
                    // anything.
                    site_role: crate::perms::SiteRole::parse(&user.site_role)
                        .unwrap_or(crate::perms::SiteRole::Observer),
                    session_id: None,
                    can_write,
                }),
            });
        }

        if let Some(cookie) = cookie_value(parts, SESSION_COOKIE)
            && let Some((user, session_id)) = lookup_session(&state.db, cookie).await
        {
            return Ok(Viewer {
                user: Some(ViewerUser {
                    id: user.id,
                    username: user.username,
                    is_admin: user.is_admin,
                    site_role: crate::perms::SiteRole::parse(&user.site_role)
                        .unwrap_or(crate::perms::SiteRole::Observer),
                    session_id: Some(session_id),
                    can_write: true,
                }),
            });
        }

        Ok(Viewer::anonymous())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hashes_verify_and_differ_per_call() {
        let a = hash_secret("correct horse battery staple").unwrap();
        let b = hash_secret("correct horse battery staple").unwrap();
        assert_ne!(a, b, "identical passwords must get distinct salts");
        assert!(verify_secret("correct horse battery staple", &a));
        assert!(verify_secret("correct horse battery staple", &b));
        assert!(!verify_secret("wrong password entirely", &a));
    }

    #[test]
    fn a_garbage_hash_never_verifies() {
        assert!(!verify_secret("anything", "not-a-phc-string"));
        assert!(!verify_secret("anything", ""));
    }

    #[test]
    fn usernames_are_normalised_and_validated() {
        assert_eq!(normalize_username("  Travis  ").unwrap(), "travis");
        assert_eq!(normalize_username("a.b-c1").unwrap(), "a.b-c1");
        assert_eq!(normalize_username("x9").unwrap(), "x9");

        for bad in ["", "has space", "has/slash", "travis@example.com"] {
            assert!(normalize_username(bad).is_err(), "should reject {bad:?}");
        }
        assert!(normalize_username(&"x".repeat(40)).is_err(), "too long");
        assert!(normalize_username("x").is_ok(), "a single letter is fine");
    }

    #[test]
    fn separators_may_not_lead_trail_or_double_up() {
        // Each of these renders close enough to a legitimate name to be used
        // for impersonation in a list of usernames.
        for bad in ["-travis", ".travis", "_travis", "travis-", "travis.", "travis_",
                    "tra..vis", "tra--vis", "tra._vis"] {
            assert!(normalize_username(bad).is_err(), "should reject {bad:?}");
        }
        assert!(normalize_username("tra-vis.mc_millan").is_ok());
    }

    #[test]
    fn non_ascii_is_rejected_so_homographs_cannot_impersonate() {
        // `travıs` (dotless i) is visually identical to `travis` at small sizes.
        assert!(normalize_username("trav\u{131}s").is_err());
        assert!(normalize_username("\u{0430}dmin").is_err(), "Cyrillic a");
        assert!(normalize_username("tr\u{0430}vis").is_err());
    }

    #[test]
    fn an_all_digit_username_is_rejected() {
        assert!(normalize_username("12345").is_err());
        assert!(normalize_username("1travis").is_ok(), "a leading digit is fine");
    }

    #[test]
    fn reserved_names_cover_routes_authority_and_infrastructure() {
        // Present routes.
        for n in ["api", "admin", "settings", "login", "new", "assets", "_health"] {
            assert!(normalize_username(n).is_err(), "route {n:?} must be reserved");
        }
        // Authority-implying names, the impersonation risk.
        for n in ["administrator", "root", "security", "support", "staff", "moderator"] {
            assert!(normalize_username(n).is_err(), "authority {n:?} must be reserved");
        }
        // Infrastructure conventions.
        for n in ["www", "noreply", "postmaster", "mail", "cdn"] {
            assert!(normalize_username(n).is_err(), "infra {n:?} must be reserved");
        }
        // Case and whitespace must not slip past the check.
        assert!(normalize_username("  ADMIN ").is_err());
        assert!(normalize_username("Root").is_err());

        // A name merely containing a reserved word is fine — only exact matches
        // collide with a route or imply authority.
        assert!(normalize_username("admins-helper").is_ok());
        assert!(normalize_username("rootbeer").is_ok());
    }

    #[test]
    fn email_validation_accepts_real_addresses() {
        for good in [
            "travis@example.com",
            "first.last@sub.example.co.uk",
            "a+tag@example.io",
            "x_y-z@my-domain.dev",
        ] {
            assert!(validate_email(good).is_ok(), "should accept {good:?}");
        }
        assert_eq!(validate_email("  Travis@Example.COM ").unwrap(), "travis@example.com");
    }

    #[test]
    fn email_validation_rejects_the_usual_mistakes() {
        for bad in [
            "",                       // empty
            "travis",                 // no domain
            "travis@",                // no domain
            "@example.com",           // no local part
            "travis@localhost",       // no dot in domain
            "travis@example",         // no suffix
            "a@b.c",                  // one-letter tld
            "a@b.1c",                 // non-alphabetic tld
            "travis@exa mple.com",    // space
            "tra..vis@example.com",   // doubled dot
            ".travis@example.com",    // leading dot
            "travis.@example.com",    // trailing dot
            "travis@-example.com",    // label starts with hyphen
            "travis@example-.com",    // label ends with hyphen
            "travis@example..com",    // empty label
            "a@b@example.com",        // two @
        ] {
            assert!(validate_email(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn short_passwords_are_rejected() {
        assert!(validate_password("short").is_err());
        assert!(validate_password("just long enough").is_ok());
    }

    #[test]
    fn minted_tokens_are_unique_and_digest_to_their_stored_hash() {
        let a = mint_token().unwrap();
        let b = mint_token().unwrap();
        assert_ne!(a.secret, b.secret);
        assert_ne!(a.prefix, b.prefix);
        assert!(a.secret.starts_with("fkit_pat_"));
        assert!(a.secret.contains(&a.prefix), "prefix must be visible in the token");

        assert_eq!(token_digest(&a.secret), a.hash);
        assert_ne!(token_digest(&b.secret), a.hash);
    }

    /// The stored value must not be the token itself.
    #[test]
    fn the_stored_digest_is_not_the_secret() {
        let t = mint_token().unwrap();
        assert_ne!(t.hash, t.secret);
        assert!(!t.hash.contains(&t.secret));
        assert_eq!(t.hash.len(), 64, "blake3 hex digest");
    }

    #[test]
    fn digests_are_deterministic_and_collision_free_for_distinct_inputs() {
        assert_eq!(token_digest("abc"), token_digest("abc"));
        assert_ne!(token_digest("abc"), token_digest("abd"));
    }
}
