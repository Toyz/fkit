//! Instance-wide policy, held in the database and cached in memory.
//!
//! These are read on nearly every request — permission resolution consults
//! `require_auth` for each repository in a listing — so a database round trip
//! per check would be a real cost. The row is cached behind an `RwLock` and
//! refreshed when an administrator changes it.
//!
//! The config file seeds the row on first boot and is otherwise ignored: once
//! an administrator has set something from the web, a stale file should not
//! quietly undo it on the next restart.

use crate::config::EnvEmail;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Instance {
    pub site_name: String,
    pub open_registration: bool,
    pub require_auth: bool,
    pub default_repo_visibility: String,
    /// What a new account gets. `observer` out of the box: accepting sign-ups
    /// should not mean accepting repositories from whoever finds the server.
    pub default_site_role: String,
    pub allowed_email_domains: Vec<String>,
    /// Sender address. Must be on a domain verified with Resend.
    pub email_from: String,
    /// Base URL used to build links in outbound mail. Falls back to the
    /// request's own origin, which is wrong behind a proxy that rewrites Host.
    pub public_url: String,
    /// The API key itself is never serialised — see the test below.
    #[serde(skip)]
    pub resend_api_key: Option<String>,
    /// Which mail fields the environment pins. The admin UI uses these to stop
    /// offering a field that cannot take effect, which is more honest than
    /// accepting a value and ignoring it.
    #[serde(skip)]
    #[sqlx(default)]
    pub key_from_env: bool,
    #[serde(skip)]
    #[sqlx(default)]
    pub sender_from_env: bool,
    #[serde(skip)]
    #[sqlx(default)]
    pub url_from_env: bool,
}

impl Instance {
    /// The configured default, or the cautious answer if the column somehow
    /// holds something unrecognised.
    pub fn default_site_role(&self) -> crate::perms::SiteRole {
        crate::perms::SiteRole::parse(&self.default_site_role)
            .unwrap_or(crate::perms::SiteRole::Observer)
    }

    /// Can this server send mail?
    pub fn email_configured(&self) -> bool {
        self.resend_api_key.as_deref().map(|k| !k.trim().is_empty()).unwrap_or(false)
            && !self.email_from.trim().is_empty()
    }
}

impl Default for Instance {
    fn default() -> Self {
        Instance {
            site_name: "fkit hub".into(),
            open_registration: true,
            require_auth: false,
            default_repo_visibility: "private".into(),
            // Cautious by default: signing up is not the same as being handed
            // a server to put repositories on.
            default_site_role: "observer".into(),
            allowed_email_domains: vec![],
            email_from: String::new(),
            public_url: String::new(),
            resend_api_key: None,
            key_from_env: false,
            sender_from_env: false,
            url_from_env: false,
        }
    }
}

impl Instance {
    /// Is this email allowed to register?
    pub fn email_allowed(&self, email: &str) -> bool {
        if self.allowed_email_domains.is_empty() {
            return true;
        }
        let domain = email.rsplit('@').next().unwrap_or_default().to_ascii_lowercase();
        self.allowed_email_domains
            .iter()
            .any(|d| d.trim().to_ascii_lowercase() == domain)
    }
}

#[derive(Clone)]
pub struct Settings {
    cache: Arc<RwLock<Instance>>,
    /// Mail settings supplied by the environment. Held apart from the cached
    /// row because they are not seeds: they are re-applied after every refresh,
    /// so a write to the database column can never shadow them.
    env: EnvEmail,
}

impl Settings {
    /// Load the row, seeding it from the config file if this is a fresh install.
    pub async fn load(db: &sqlx::PgPool, seed: Instance, env: EnvEmail) -> Result<Settings> {
        sqlx::query(
            "INSERT INTO instance_settings
                (id, site_name, open_registration, require_auth, default_repo_visibility,
                 default_site_role, email_from, public_url)
             VALUES (TRUE, $1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&seed.site_name)
        .bind(seed.open_registration)
        .bind(seed.require_auth)
        .bind(&seed.default_repo_visibility)
        .bind(&seed.default_site_role)
        .bind(&seed.email_from)
        .bind(&seed.public_url)
        .execute(db)
        .await?;

        let s = Settings { cache: Arc::new(RwLock::new(Instance::default())), env };
        s.refresh(db).await?;
        Ok(s)
    }

    /// Re-read the row from the database.
    pub async fn refresh(&self, db: &sqlx::PgPool) -> Result<Instance> {
        let mut next = fetch(db).await?;
        self.env.apply(&mut next);
        self.put(next.clone());
        Ok(next)
    }

    /// Fields the environment pins, so the admin routes can refuse a write that
    /// would appear to succeed and change nothing.
    pub fn env_email(&self) -> &EnvEmail {
        &self.env
    }

    pub fn get(&self) -> Instance {
        self.cache.read().expect("settings lock poisoned").clone()
    }

    /// Replace the cached copy after a write.
    pub fn put(&self, next: Instance) {
        *self.cache.write().expect("settings lock poisoned") = next;
    }
}

impl EnvEmail {
    /// Overlay the environment onto a freshly-read row.
    fn apply(&self, i: &mut Instance) {
        if let Some(v) = &self.api_key {
            i.resend_api_key = Some(v.clone());
            i.key_from_env = true;
        }
        if let Some(v) = &self.from {
            i.email_from = v.clone();
            i.sender_from_env = true;
        }
        if let Some(v) = &self.public_url {
            i.public_url = v.trim_end_matches('/').to_string();
            i.url_from_env = true;
        }
    }
}

async fn fetch(db: &sqlx::PgPool) -> Result<Instance> {
    let row: Instance = sqlx::query_as(
        "SELECT site_name, open_registration, require_auth, default_repo_visibility,
                default_site_role, allowed_email_domains, email_from, public_url, resend_api_key
           FROM instance_settings WHERE id = TRUE",
    )
    .fetch_one(db)
    .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_is_only_configured_when_both_halves_are_present() {
        let mut i = Instance::default();
        assert!(!i.email_configured());
        i.resend_api_key = Some("re_123".into());
        assert!(!i.email_configured(), "a key alone cannot address a message");
        i.email_from = "hub@example.com".into();
        assert!(i.email_configured());
        i.resend_api_key = Some("   ".into());
        assert!(!i.email_configured(), "a blank key is not a key");
    }

    #[test]
    fn the_environment_overlays_the_stored_row_and_marks_each_field() {
        let env = EnvEmail {
            api_key: Some("re_env".into()),
            public_url: Some("https://fkit.work/".into()),
            from: None,
        };
        let mut i = Instance {
            resend_api_key: Some("re_stored".into()),
            public_url: "https://old.example".into(),
            email_from: "stored@example.com".into(),
            ..Default::default()
        };
        env.apply(&mut i);
        assert_eq!(i.resend_api_key.as_deref(), Some("re_env"));
        assert_eq!(i.public_url, "https://fkit.work", "a trailing slash would double up in links");
        assert_eq!(i.email_from, "stored@example.com", "an unset variable overrides nothing");
        assert!(i.key_from_env && i.url_from_env && !i.sender_from_env);
    }

    #[test]
    fn a_key_from_the_environment_is_not_advertised_as_stored() {
        // `key_from_env` is what the admin UI keys its "supplied by the
        // environment" notice off; it must not ride along in JSON either.
        let i = Instance {
            resend_api_key: Some("re_env".into()),
            key_from_env: true,
            email_from: "hub@example.com".into(),
            ..Default::default()
        };
        assert!(i.email_configured());
        let json = serde_json::to_string(&i).unwrap();
        assert!(!json.contains("key_from_env"));
    }

    #[test]
    fn the_api_key_is_never_serialised() {
        let i = Instance {
            resend_api_key: Some("re_supersecret".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&i).unwrap();
        assert!(!json.contains("re_supersecret"), "the key must not leave the server");
        assert!(!json.contains("resend_api_key"));
    }

    #[test]
    fn no_domain_list_allows_everything() {
        let i = Instance::default();
        assert!(i.email_allowed("anyone@anywhere.test"));
    }

    #[test]
    fn a_domain_list_restricts_registration() {
        let i = Instance {
            allowed_email_domains: vec!["example.com".into(), "Corp.Test".into()],
            ..Default::default()
        };
        assert!(i.email_allowed("me@example.com"));
        assert!(i.email_allowed("me@corp.test"), "domain match is case-insensitive");
        assert!(!i.email_allowed("me@elsewhere.com"));
        // A lookalike must not slip through on a suffix match.
        assert!(!i.email_allowed("me@notexample.com"));
    }
}
