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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefUpdate {
    Updated,
    AlreadyCurrent,
    /// The server's current tip is not an ancestor of the pushed commit.
    NotFastForward,
    /// The server refuses this update on policy grounds, with its reason.
    ///
    /// Distinct from `NotFastForward` because the advice is opposite: that one
    /// means "your history is behind, catch up and try again", this one means
    /// "what you are asking for is not allowed here, and retrying will not
    /// change that".
    Refused(String),
}

/// Everything the shared session loop needs from a particular server.
pub trait RepoHost {
    fn store(&self) -> &Store;

    // ---- stashes ----
    //
    // Parked work belongs to an account, so a server without accounts has
    // nobody to park it for. These default to refusing rather than being
    // required, which keeps the minimal daemon exactly as simple as it was.

    fn put_stash(&self, _tip: Hash, _message: &str) -> Result<()> {
        anyhow::bail!("this server does not keep stashes")
    }
    fn list_stashes(&self) -> Result<Vec<(Hash, String)>> {
        Ok(Vec::new())
    }
    /// Whether this stash is the caller's to read. False for anything else,
    /// including somebody else's.
    fn owns_stash(&self, _commit: Hash) -> Result<bool> {
        Ok(false)
    }
    fn drop_stash(&self, _commit: Hash) -> Result<()> {
        anyhow::bail!("this server does not keep stashes")
    }

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

/// Ref names become path components in some backends and column values in
/// others, so they are validated once, centrally.
///
/// Branch names may contain `/`, so the tag namespace cannot simply claim the
/// character. `tags/` is reserved instead: it is stripped here so a tag ref
/// validates as its bare name, and [`valid_new_branch`] refuses to create a
/// branch that would land inside it.
pub fn valid_branch(name: &str) -> bool {
    let name = name.strip_prefix(TAG_PREFIX).unwrap_or(name);
    !name.is_empty()
        && name.len() <= 127
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

/// Duplicated from `Repo` so the protocol layer does not depend on a working
/// tree — a server has refs but no checkout.
pub const TAG_PREFIX: &str = "tags/";

/// Is this ref a tag rather than a branch?
pub fn is_tag(name: &str) -> bool {
    name.starts_with(TAG_PREFIX)
}

/// May a *branch* be created with this name?
///
/// Same rules, minus the tag namespace. Without this a branch called `tags/v1`
/// would arrive at the server indistinguishable from the tag `v1`.
pub fn valid_new_branch(name: &str) -> bool {
    valid_branch(name) && !is_tag(name)
}

/// A tag name: like a branch, but flat. Tags are stored one per file in a
/// single directory, and a `/` would nest them where nothing looks.
pub fn valid_tag(name: &str) -> bool {
    valid_branch(name) && !name.contains('/')
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
            // ---- stashes ----
            //
            // Parked work. It is kept and it is not a ref, so it needs verbs of
            // its own; a host without accounts has nobody to park it for and
            // refuses through the trait's default.
            Msg::PushStash { tip, message } => {
                if !host.can_write() {
                    send_error(t, "you do not have write access to this repository")?;
                    continue;
                }
                // Objects first: what is recorded must already be here, the
                // same order a ref push uses and for the same reason.
                let stats = fetch_closure(host.store(), t, &[tip])?;
                totals.pushed.merge(&stats);
                match host.put_stash(tip, &message) {
                    Ok(()) => crate::proto::send(t, &Msg::Ok {
                        message: format!("stashed {} ({} objects)", tip.short(), stats.objects),
                    })?,
                    Err(e) => send_error(t, e.to_string())?,
                }
            }

            Msg::ListStashes => match host.list_stashes() {
                Ok(entries) => crate::proto::send(t, &Msg::StashList { entries })?,
                Err(e) => send_error(t, e.to_string())?,
            },

            Msg::PullStash { commit } => {
                // Ownership is the host's to judge; it answers false for
                // anything that is not this account's.
                match host.owns_stash(commit) {
                    Ok(true) => {
                        let stats = serve_wants(host.store(), t)?;
                        totals.pulled.merge(&stats);
                        crate::proto::send(t, &Msg::Ok {
                            message: format!("sent {} objects", stats.objects),
                        })?;
                    }
                    Ok(false) => send_error(t, "no such stash")?,
                    Err(e) => send_error(t, e.to_string())?,
                }
            }

            Msg::DropStash { commit } => match host.drop_stash(commit) {
                Ok(()) => crate::proto::send(t, &Msg::Ok {
                    message: format!("dropped {}", commit.short()),
                })?,
                Err(e) => send_error(t, e.to_string())?,
            },

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
                    RefUpdate::Refused(why) => {
                        send_error(t, format!("rejected: {why}"))?;
                    }
                    RefUpdate::NotFastForward => {
                        // A tag has no ancestry to be behind, so the branch
                        // advice — "pull first" — would send someone looking
                        // for a merge that does not exist.
                        let message = match branch.strip_prefix(TAG_PREFIX) {
                            Some(tag) => format!(
                                "rejected: tag {tag} already exists on the server at a \
                                 different commit — move it with \
                                 `fkit push --tag {tag} --force`, which touches nothing else"
                            ),
                            None => format!(
                                "rejected: {branch} on the server is not an ancestor of your \
                                 commit (pull first, or push with --force)"
                            ),
                        };
                        send_error(t, message)?;
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

    #[test]
    fn the_tag_namespace_is_reserved_against_branches() {
        // A tag ref validates as its bare name...
        assert!(valid_branch("tags/v1.0"));
        assert!(is_tag("tags/v1.0"));
        // ...but nothing may create a branch that lands in the namespace,
        // which would otherwise be indistinguishable on the wire.
        assert!(!valid_new_branch("tags/v1.0"));
        assert!(valid_new_branch("feature/x"), "slashes are still fine in a branch");

        // Tag names are flat: they are one file in one directory.
        assert!(valid_tag("v1.0"));
        assert!(!valid_tag("release/v1.0"));
        assert!(!valid_tag(""));
    }
}
