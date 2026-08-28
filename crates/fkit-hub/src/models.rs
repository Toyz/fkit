//! Database rows and the API shapes they are projected into.
//!
//! Row types and response types are deliberately separate. A `UserRow` holds a
//! password hash; a `UserView` cannot, because the field does not exist on it.
//! Keeping them distinct means no `#[serde(skip)]` can ever be deleted by
//! accident and leak a credential — the type system refuses to build it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserRow {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub password_hash: String,
    pub display_name: Option<String>,
    pub is_admin: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserView {
    pub id: Uuid,
    pub username: String,
    pub display_name: Option<String>,
    pub is_admin: bool,
    pub created_at: DateTime<Utc>,
}

impl From<&UserRow> for UserView {
    fn from(u: &UserRow) -> Self {
        UserView {
            id: u.id,
            username: u.username.clone(),
            display_name: u.display_name.clone(),
            is_admin: u.is_admin,
            created_at: u.created_at,
        }
    }
}

/// The authenticated caller's own record — includes email, which is not public.
#[derive(Debug, Clone, Serialize)]
pub struct SelfView {
    #[serde(flatten)]
    pub user: UserView,
    pub email: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RepoRow {
    pub id: Uuid,
    pub owner_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub visibility: String,
    pub default_branch: String,
    pub homepage: String,
    pub topics: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A repo row joined to its owner's username.
///
/// `query_as` cannot decode a struct as a tuple element, so the join is
/// expressed with `#[sqlx(flatten)]` rather than `(RepoRow, String)`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RepoWithOwner {
    #[sqlx(flatten)]
    pub repo: RepoRow,
    pub username: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoView {
    pub id: Uuid,
    pub owner: String,
    pub name: String,
    pub full_name: String,
    pub description: Option<String>,
    pub visibility: String,
    pub default_branch: String,
    pub homepage: String,
    pub topics: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// The viewer's effective access, so the UI can hide controls it would be
    /// rejected for anyway.
    pub access: String,
    /// The tip of the default branch. `None` for a repository nobody has
    /// pushed to yet, which is a state the listing has to render honestly
    /// rather than as an empty row.
    pub head: Option<HeadView>,
    /// How many branches exist. One is the uninteresting case, so the UI only
    /// says anything when there is more than one.
    pub branches: i64,
    /// How many tags. Counted apart from branches: both are refs, and tags
    /// share the branch namespace behind a prefix.
    pub tags: i64,
}

/// A one-line summary of a commit, for a listing that should say what the
/// repository last did rather than only that it exists.
#[derive(Debug, Clone, Serialize)]
pub struct HeadView {
    pub commit: String,
    pub short: String,
    /// First line of the message.
    pub summary: String,
    pub author: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CollaboratorView {
    pub user_id: Uuid,
    pub username: String,
    pub role: String,
    pub granted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenView {
    pub id: Uuid,
    pub name: String,
    pub prefix: String,
    pub can_write: bool,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Returned exactly once, at creation. The plaintext is never stored.
#[derive(Debug, Serialize)]
pub struct NewTokenView {
    #[serde(flatten)]
    pub token: TokenView,
    pub secret: String,
}

// ---- request bodies -----------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RegisterReq {
    pub username: String,
    pub email: String,
    pub password: String,
    /// An invitation token from `/register?invite=…`. Lets one account through
    /// on a server with registration closed.
    #[serde(default)]
    pub invite: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateRepoReq {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRepoReq {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub topics: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTokenReq {
    pub name: String,
    #[serde(default = "default_true")]
    pub can_write: bool,
    #[serde(default)]
    pub expires_in_days: Option<i64>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct AddCollaboratorReq {
    pub username: String,
    pub role: String,
}
