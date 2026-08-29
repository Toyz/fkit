//! Access resolution — the single place that decides who may do what.
//!
//! Every route funnels through [`resolve`]. Scattering `if owner_id == user.id`
//! checks across handlers is how forges grow permission bugs; here there is one
//! function to read, and one to test.

use crate::error::{AppError, AppResult};
use crate::models::RepoRow;
use uuid::Uuid;

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
