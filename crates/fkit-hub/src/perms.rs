//! Access resolution — the single place that decides who may do what.
//!
//! Every route funnels through [`resolve`]. Scattering `if owner_id == user.id`
//! checks across handlers is how forges grow permission bugs; here there is one
//! function to read, and one to test.

use crate::error::{AppError, AppResult};
use crate::models::RepoRow;
use uuid::Uuid;

/// What someone may do to *the instance*, as opposed to a repository.
///
/// Deliberately three fixed roles rather than arbitrary grants. The question a
/// public server actually needs to answer is "may this person create
/// repositories here", and a capability system general enough to express
/// anything is a system nobody can audit at a glance.
///
/// Every role can open issues and comment on what it can already read — that
/// is the point of having an observer at all, and why participation is not a
/// capability anything has to check.
///
/// Orthogonal to [`Access`]: a site role never grants access to someone else's
/// repository, and an observer who is made a collaborator on a repository can
/// write to it. The two answer different questions and are resolved
/// separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SiteRole {
    /// Read what is public, open issues, comment. What a new account gets.
    Observer,
    /// Create and own repositories. Everything an observer can do.
    Member,
    /// The instance: users, settings, and every repository.
    Admin,
}

impl SiteRole {
    pub fn parse(s: &str) -> Option<SiteRole> {
        match s {
            "observer" => Some(SiteRole::Observer),
            "member" => Some(SiteRole::Member),
            "admin" => Some(SiteRole::Admin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SiteRole::Observer => "observer",
            SiteRole::Member => "member",
            SiteRole::Admin => "admin",
        }
    }

    /// Create a repository, or fork one — a fork is a repository.
    pub fn can_create_repo(self) -> bool {
        self >= SiteRole::Member
    }

    /// Instance settings, other people's accounts, every repository.
    pub fn can_administer_site(self) -> bool {
        self == SiteRole::Admin
    }

}

/// Refuse an action the site role does not carry.
pub fn require_site(role: SiteRole, allowed: bool, what: &str) -> AppResult<()> {
    if allowed {
        Ok(())
    } else {
        Err(AppError::Forbidden(format!(
            "your account ({}) cannot {what}",
            role.as_str()
        )))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    /// Cannot see that this repository exists.
    None,
    Read,
    Write,
    /// Settings, collaborators, deletion.
    Admin,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::None => "none",
            Access::Read => "read",
            Access::Write => "write",
            Access::Admin => "admin",
        }
    }

    fn from_role(role: &str) -> Access {
        match role {
            "read" => Access::Read,
            "write" => Access::Write,
            "admin" => Access::Admin,
            _ => Access::None,
        }
    }

    pub fn can_read(self) -> bool {
        self >= Access::Read
    }
    pub fn can_write(self) -> bool {
        self >= Access::Write
    }
    pub fn can_admin(self) -> bool {
        self >= Access::Admin
    }
}

/// Resolve a viewer's effective access to a repository.
///
/// `token_can_write` is the *ceiling* imposed by the credential itself: a
/// read-only personal access token cannot write even for a repository owner.
/// A credential may narrow access; it may never widen it.
/// Would this viewer see nothing here if they did not administer the server?
///
/// Asked separately from [`resolve`] rather than threaded through it, because
/// only one page needs the answer and every caller of `resolve` would
/// otherwise have to carry it.
pub async fn only_via_site_admin(
    db: &sqlx::PgPool,
    repo: &RepoRow,
    viewer_id: Option<Uuid>,
    viewer_is_admin: bool,
) -> AppResult<bool> {
    let Some(uid) = viewer_id else { return Ok(false) };
    if !viewer_is_admin || repo.owner_id == uid || repo.visibility == "public" {
        return Ok(false);
    }
    let role: Option<(String,)> =
        sqlx::query_as("SELECT role FROM collaborators WHERE repo_id = $1 AND user_id = $2")
            .bind(repo.id)
            .bind(uid)
            .fetch_optional(db)
            .await?;
    Ok(role.is_none())
}

pub async fn resolve(
    db: &sqlx::PgPool,
    repo: &RepoRow,
    viewer_id: Option<Uuid>,
    viewer_is_admin: bool,
    token_can_write: bool,
    require_auth: bool,
) -> AppResult<Access> {
    // A locked-down instance grants nothing to anonymous callers, regardless of
    // any repository's own visibility.
    if require_auth && viewer_id.is_none() {
        return Ok(Access::None);
    }

    let base = if let Some(uid) = viewer_id {
        if repo.owner_id == uid {
            Access::Admin
        } else if viewer_is_admin {
            // Server administrators can see everything; useful for operations
            // and dangerous enough to be worth the audit_log entry callers write.
            Access::Admin
        } else {
            let role: Option<(String,)> = sqlx::query_as(
                "SELECT role FROM collaborators WHERE repo_id = $1 AND user_id = $2",
            )
            .bind(repo.id)
            .bind(uid)
            .fetch_optional(db)
            .await?;

            match role {
                Some((r,)) => Access::from_role(&r),
                None if repo.visibility == "public" => Access::Read,
                None => Access::None,
            }
        }
    } else if repo.visibility == "public" {
        Access::Read
    } else {
        Access::None
    };

    if !token_can_write && base > Access::Read {
        return Ok(Access::Read);
    }
    Ok(base)
}

/// Collapse "you may not" into "it does not exist", so error codes cannot be
/// used to enumerate private repositories.
pub fn require_read(access: Access, owner: &str, name: &str) -> AppResult<()> {
    if access.can_read() {
        Ok(())
    } else {
        Err(AppError::not_found(format!("no such repository: {owner}/{name}")))
    }
}

/// Once read access is established the viewer already knows the repository
/// exists, so a write refusal can honestly say 403.
pub fn require_write(access: Access) -> AppResult<()> {
    if access.can_write() {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "you do not have write access to this repository".into(),
        ))
    }
}

pub fn require_admin(access: Access) -> AppResult<()> {
    if access.can_admin() {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "you do not have admin access to this repository".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_site_role_carries_exactly_what_it_says() {
        assert!(SiteRole::Admin.can_administer_site());
        assert!(!SiteRole::Member.can_administer_site());
        assert!(!SiteRole::Observer.can_administer_site());

        assert!(SiteRole::Admin.can_create_repo());
        assert!(SiteRole::Member.can_create_repo());
        // The whole reason the role exists.
        assert!(!SiteRole::Observer.can_create_repo());

    }

    #[test]
    fn a_site_role_round_trips_through_the_database_spelling() {
        for r in [SiteRole::Observer, SiteRole::Member, SiteRole::Admin] {
            assert_eq!(SiteRole::parse(r.as_str()), Some(r));
        }
        // An unknown value is not silently an admin.
        assert_eq!(SiteRole::parse("root"), None);
        assert_eq!(SiteRole::parse(""), None);
    }

    #[test]
    fn a_site_role_grants_nothing_over_someone_elses_repository() {
        // The two ladders are separate on purpose: being able to create your
        // own repositories says nothing about anyone else's, and this is the
        // assertion that stops the two being conflated later.
        assert!(SiteRole::Member.can_create_repo());
        assert!(!Access::None.can_read());
    }

    #[test]
    fn access_is_ordered_so_comparisons_are_meaningful() {
        assert!(Access::Admin > Access::Write);
        assert!(Access::Write > Access::Read);
        assert!(Access::Read > Access::None);
        assert!(Access::Admin.can_read() && Access::Admin.can_write() && Access::Admin.can_admin());
        assert!(Access::Write.can_read() && Access::Write.can_write() && !Access::Write.can_admin());
        assert!(Access::Read.can_read() && !Access::Read.can_write());
        assert!(!Access::None.can_read());
    }

    #[test]
    fn hiding_a_private_repo_reports_not_found_not_forbidden() {
        let err = require_read(Access::None, "someone", "secret").unwrap_err();
        assert!(
            matches!(err, AppError::NotFound(_)),
            "must be 404 so the error cannot confirm the repo exists"
        );
    }
}
