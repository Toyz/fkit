//! Sending mail through Resend.
//!
//! Email is optional. With no API key configured the server simply has no
//! password-reset flow, and says so rather than pretending to send something —
//! a reset form that silently does nothing is worse than one that is absent.
//!
//! Only Resend is supported, deliberately: it is one HTTPS POST with a JSON
//! body, so the integration is small enough to read, and adding SMTP would mean
//! carrying an SMTP client, TLS negotiation and a queue for a feature most
//! self-hosted instances never turn on.

use anyhow::{bail, Context, Result};
use serde::Serialize;

const ENDPOINT: &str = "https://api.resend.com/emails";

#[derive(Serialize)]
struct Payload<'a> {
    from: &'a str,
    to: [&'a str; 1],
    subject: &'a str,
    text: &'a str,
}

pub struct Mailer {
    api_key: String,
    from: String,
}

impl Mailer {
    /// `None` when the instance has no key or no from-address configured.
    pub fn new(api_key: Option<&str>, from: &str) -> Option<Mailer> {
        let key = api_key?.trim().to_string();
        if key.is_empty() || from.trim().is_empty() {
            return None;
        }
        Some(Mailer { api_key: key, from: from.trim().to_string() })
    }

    pub async fn send(&self, to: &str, subject: &str, text: &str) -> Result<()> {
        let payload = Payload { from: &self.from, to: [to], subject, text };

        let res = reqwest::Client::new()
            .post(ENDPOINT)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .context("contacting Resend")?;

        if !res.status().is_success() {
            let status = res.status();
            // Resend puts the useful part in the body; the status alone does not
            // distinguish "unverified domain" from "bad key".
            let body = res.text().await.unwrap_or_default();
            bail!("Resend rejected the message ({status}): {}", body.trim());
        }
        Ok(())
    }
}

/// The reset email. Plain text on purpose: it renders everywhere, cannot carry
/// a tracking pixel, and makes the link visible rather than hidden behind
/// anchor text — which is what people are told to check before clicking.
pub fn reset_body(username: &str, link: &str, minutes: i64) -> String {
    format!(
        "Someone asked to reset the password for {username}.\n\n\
         Open this link to choose a new one:\n\n  {link}\n\n\
         The link works once and expires in {minutes} minutes.\n\n\
         If this was not you, nothing has changed and you can ignore this message.\n"
    )
}

/// The invitation email. Plain text for the same reasons as the reset above,
/// and it names the person who sent it — an unexplained link to a server you
/// have never heard of is indistinguishable from a phish.
pub fn invite_body(from_user: &str, site: &str, link: &str, days: i64) -> String {
    format!(
        "{from_user} invited you to {site}.\n\n\
         Open this link to create your account:\n\n  {link}\n\n\
         The link works once and expires in {days} days.\n\n\
         If you were not expecting this, you can ignore the message — \
         no account exists until someone uses the link.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_invite_names_its_sender_and_the_server() {
        let b = invite_body("travis", "fkit hub", "https://fkit.work/register?invite=x", 14);
        assert!(b.contains("travis invited you to fkit hub"));
        assert!(b.contains("https://fkit.work/register?invite=x"));
        assert!(b.contains("14 days"));
    }

    #[test]
    fn a_mailer_needs_both_a_key_and_a_sender() {
        assert!(Mailer::new(None, "hub@example.com").is_none());
        assert!(Mailer::new(Some(""), "hub@example.com").is_none());
        assert!(Mailer::new(Some("re_123"), "").is_none());
        assert!(Mailer::new(Some("  re_123  "), " hub@example.com ").is_some());
    }

    #[test]
    fn the_reset_body_shows_the_link_in_full() {
        let b = reset_body("travis", "https://hub.example.com/reset?t=abc", 30);
        assert!(b.contains("https://hub.example.com/reset?t=abc"));
        assert!(b.contains("travis"));
        assert!(b.contains("30 minutes"));
        assert!(b.contains("was not you"), "must say what to do if unexpected");
    }
}
