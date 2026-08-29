//! Branch protection: what may be done to a branch, and by whom.
//!
//! Write access is one bit — you can push, or you cannot — which is right for
//! a scratch branch and wrong for the one everything is cut from. The
//! operations worth stopping there are not "push" but "rewrite" and "remove",
//! because a fast-forward only ever adds and those two destroy work that was
//! already pushed.
//!
//! # The owner is never bound
//!
//! Deliberately, and it is the reason this can exist at all on a hub that
//! mirrors. A mirror pushes with a token, and a token belongs to an account;
//! mirroring somebody else's repository means rewriting whatever they rewrote,
//! so a rule that stopped force-pushes would stop the mirror. Exempting the
//! owner — whatever credential they hold, session or token — keeps that
//! working while still binding every collaborator.
//!
//! The cost is that these rules cannot protect an owner from themselves. That
//! is a real limitation and it is the trade being made: they are here to say
//! what *other people* may do to a branch.

use uuid::Uuid;

/// One rule, as stored.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct BranchRule {
    pub id: Uuid,
    pub pattern: String,
    pub no_force: bool,
    pub no_delete: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Does `pattern` govern `branch`?
///
/// Exact names, or a prefix ending in `*`. Deliberately not a full glob: the
/// point of a protection rule is that a reader can tell at a glance what it
/// covers, and `release/*` answers that where `release/[0-9]*.?(x|y)` does not.
pub fn matches(pattern: &str, branch: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => branch.starts_with(prefix),
        None => pattern == branch,
    }
}

/// The rules covering a branch, most specific first.
///
/// Longer patterns win, so `release/1.x` beats `release/*` beats `*`. Every
/// matching rule still applies — a limit is never lifted by a broader rule
/// also matching — but the order decides which one is named when explaining a
/// refusal, and naming the vaguest of them would be the least useful answer.
pub fn covering<'a>(rules: &'a [BranchRule], branch: &str) -> Vec<&'a BranchRule> {
    let mut hit: Vec<&BranchRule> =
        rules.iter().filter(|r| matches(&r.pattern, branch)).collect();
    hit.sort_by_key(|r| std::cmp::Reverse(r.pattern.len()));
    hit
}

/// What a rule set says about one operation on one branch.
///
/// `None` means allowed; `Some` carries a sentence fit to hand to whoever
/// tried, naming the rule that stopped them.
pub fn deny_force(rules: &[BranchRule], branch: &str) -> Option<String> {
    let r = covering(rules, branch).into_iter().find(|r| r.no_force)?;
    Some(format!(
        "{branch} is protected by the rule `{}` — force-pushing would drop \
         commits other people have already pulled. Push a fast-forward, or ask \
         the repository's owner.",
        r.pattern
    ))
}

pub fn deny_delete(rules: &[BranchRule], branch: &str) -> Option<String> {
    let r = covering(rules, branch).into_iter().find(|r| r.no_delete)?;
    Some(format!(
        "{branch} is protected by the rule `{}` and cannot be deleted.",
        r.pattern
    ))
}

/// Load a repository's rules.
///
/// An error is not a reason to let something through, so callers treat a
/// failure as "no opinion" only where the operation is already safe. The two
/// enforcement points both fail closed instead.
pub async fn for_repo(db: &sqlx::PgPool, repo: Uuid) -> sqlx::Result<Vec<BranchRule>> {
    sqlx::query_as(
        "SELECT id, pattern, no_force, no_delete, created_at
           FROM branch_rules WHERE repo_id = $1 ORDER BY pattern",
    )
    .bind(repo)
    .fetch_all(db)
    .await
}

/// Whether this account is exempt. See the module note: the owner always is.
pub fn exempt(user_id: Option<Uuid>, owner_id: Uuid) -> bool {
    user_id == Some(owner_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, no_force: bool, no_delete: bool) -> BranchRule {
        BranchRule {
            id: Uuid::nil(),
            pattern: pattern.into(),
            no_force,
            no_delete,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn an_exact_pattern_covers_only_that_branch() {
        assert!(matches("main", "main"));
        assert!(!matches("main", "maintenance"));
        assert!(!matches("main", "feature/main"));
    }

    #[test]
    fn a_trailing_star_covers_the_prefix() {
        assert!(matches("release/*", "release/1.0"));
        assert!(matches("release/*", "release/"));
        assert!(!matches("release/*", "releases/1.0"));
        assert!(matches("*", "anything"));
    }

    #[test]
    fn the_most_specific_rule_is_the_one_named() {
        let rules = vec![rule("*", true, true), rule("release/*", true, true)];
        let why = deny_force(&rules, "release/1.0").expect("denied");
        assert!(why.contains("release/*"), "{why}");
    }

    #[test]
    fn a_rule_that_does_not_forbid_the_operation_does_not_stop_it() {
        // Forbids deletion but permits a rewrite.
        let rules = vec![rule("main", false, true)];
        assert!(deny_force(&rules, "main").is_none());
        assert!(deny_delete(&rules, "main").is_some());
    }

    #[test]
    fn a_broader_rule_still_applies_when_the_specific_one_is_silent() {
        // `release/*` allows force; `*` does not. The branch is still covered:
        // a narrower rule must not quietly lift a wider limit.
        let rules = vec![rule("*", true, true), rule("release/*", false, false)];
        assert!(deny_force(&rules, "release/1.0").is_some());
    }

    #[test]
    fn an_unmatched_branch_is_free() {
        let rules = vec![rule("main", true, true)];
        assert!(deny_force(&rules, "scratch").is_none());
        assert!(deny_delete(&rules, "scratch").is_none());
    }

    #[test]
    fn the_owner_is_exempt_and_nobody_else_is() {
        let owner = Uuid::from_u128(1);
        let other = Uuid::from_u128(2);
        assert!(exempt(Some(owner), owner));
        assert!(!exempt(Some(other), owner));
        assert!(!exempt(None, owner), "anonymous is not the owner");
    }
}
