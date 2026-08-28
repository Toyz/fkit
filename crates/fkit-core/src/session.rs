//! The server side of a sync session, independent of where refs live.
//!
//! Both servers speak the identical protocol but disagree about storage and
//! authorisation: `fkitd` keeps refs in files and trusts everyone who has the
//! shared token, while `fkit-hub` keeps refs in Postgres and resolves per-repo
//! roles. Only those differences belong to each server; the conversation itself
//! is one implementation, here.
//!
//! A [`RepoHost`] supplies the parts that differ. It is deliberately synchronous
//! — an async host (the hub) blocks on its runtime handle inside these methods,
//! which keeps `fkit-core` free of any async runtime dependency.

use crate::hash::Hash;
use crate::proto::{fetch_closure, serve_wants, verify_closure, Msg, TransferStats, Transport};
use crate::store::Store;
use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefUpdate {
    Updated,
    AlreadyCurrent,
    /// The server's current tip is not an ancestor of the pushed commit.
    NotFastForward,
}

/// Everything the shared session loop needs from a particular server.
pub trait RepoHost {
    fn store(&self) -> &Store;
    fn refs(&self) -> Result<Vec<(String, Hash)>>;
    fn read_ref(&self, branch: &str) -> Result<Option<Hash>>;

    /// Whether this session may move refs at all. Checked before any objects
    /// are accepted, so an unauthorised push costs nothing.
    fn can_write(&self) -> bool {
        true
    }

    /// Advance a branch, enforcing fast-forward unless `force`.
    ///
    /// Implementations are expected to make the check and the write atomic;
    /// otherwise two concurrent pushes can both pass the check and one will
    /// silently lose commits.
    fn advance_ref(&self, branch: &str, tip: Hash, force: bool) -> Result<RefUpdate>;

    /// Hook for logging and auditing. Failure here must not fail the push.
    fn on_push(&self, _branch: &str, _tip: Hash, _stats: &TransferStats) {}
    fn on_pull(&self, _branch: &str, _stats: &TransferStats) {}
}

/// Branch names become path components in some backends and column values in
/// others, so they are validated once, centrally.
pub fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 127
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

/// Read the opening `Hello`, returning `(repo, token)`.
///
/// Kept separate from [`serve_session`] because authentication is the one part
/// each server genuinely owns.
pub fn read_hello<T: Transport + ?Sized>(t: &mut T) -> Result<(String, String)> {
    match crate::proto::recv(t)? {
        Msg::Hello { repo, token } => Ok((repo, token)),
        _ => {
            let _ = crate::proto::send(t, &Msg::Error { message: "expected Hello".into() });
            bail!("client did not open with Hello")
        }
    }
}

pub fn send_welcome<T: Transport + ?Sized>(t: &mut T, refs: Vec<(String, Hash)>) -> Result<()> {
    crate::proto::send(t, &Msg::Welcome { refs })
}

pub fn send_error<T: Transport + ?Sized>(t: &mut T, message: impl Into<String>) -> Result<()> {
    crate::proto::send(t, &Msg::Error { message: message.into() })
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SessionStats {
    pub pushed: TransferStats,
    pub pulled: TransferStats,
}

/// Serve push and pull requests until the peer disconnects or says `Done`.
///
/// Call after `read_hello` and `send_welcome`.
pub fn serve_session<T: Transport + ?Sized, H: RepoHost + ?Sized>(
    t: &mut T,
    host: &H,
) -> Result<SessionStats> {
    let mut totals = SessionStats::default();

    loop {
        // A disconnect after the work is done is the normal ending, not an error.
        let msg = match crate::proto::recv(t) {
            Ok(m) => m,
            Err(_) => return Ok(totals),
        };

        match msg {
            Msg::PushRef { branch, tip, force } => {
                if !host.can_write() {
                    send_error(t, "you do not have write access to this repository")?;
                    continue;
                }
                if !valid_branch(&branch) {
                    send_error(t, format!("invalid branch name: {branch}"))?;
                    continue;
                }

                // Receive the closure first: the ref cannot be allowed to point
                // at objects we do not hold, so verification precedes the move.
                let stats = fetch_closure(host.store(), t, &[tip])?;
                verify_closure(host.store(), tip)?;
                totals.pushed.merge(&stats);

                match host.advance_ref(&branch, tip, force)? {
                    RefUpdate::Updated => {
                        host.on_push(&branch, tip, &stats);
                        crate::proto::send(t, &Msg::Ok {
                            message: format!(
                                "{branch} -> {} ({} objects received)",
                                tip.short(),
                                stats.objects
                            ),
                        })?;
                    }
                    RefUpdate::AlreadyCurrent => {
                        crate::proto::send(t, &Msg::Ok {
                            message: format!("{branch} already up to date"),
                        })?;
                    }
                    RefUpdate::NotFastForward => {
                        send_error(
                            t,
                            format!(
                                "rejected: {branch} on the server is not an ancestor of your \
                                 commit (pull first, or push with --force)"
                            ),
                        )?;
                    }
                }
            }

            Msg::PullRef { branch } => {
                let tip = host.read_ref(&branch)?;
                crate::proto::send(t, &Msg::RefIs { branch: branch.clone(), tip })?;
                if tip.is_some() {
                    let stats = serve_wants(host.store(), t)?;
                    totals.pulled.merge(&stats);
                    host.on_pull(&branch, &stats);
                    crate::proto::send(t, &Msg::Ok {
                        message: format!("sent {} objects", stats.objects),
                    })?;
                }
            }

            Msg::Done => return Ok(totals),

            other => {
                send_error(t, format!("unexpected message {other:?}"))?;
                return Ok(totals);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_are_validated() {
        for good in ["main", "feature/x", "v1.2.3", "a", "release-2026_01"] {
            assert!(valid_branch(good), "should accept {good:?}");
        }
        for bad in ["", "-leading", "/leading", "has space", "a..b", "with\\slash", &"x".repeat(128)] {
            assert!(!valid_branch(bad), "should reject {bad:?}");
        }
    }
}
