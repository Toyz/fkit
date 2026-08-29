//! Serving Go modules, without git.
//!
//! `go get` does not have to speak a version control protocol. Its discovery
//! step reads a `<meta name="go-import">` tag, and one of the legal values for
//! the VCS field is `mod`, meaning "fetch this from a module proxy". That
//! reduces the whole problem to five plain GETs — `go help goproxy`:
//!
//! ```text
//! GET  /gomod/<module>/@v/list           newline-separated versions
//! GET  /gomod/<module>/@v/<ver>.info     {"Version":…, "Time":…}
//! GET  /gomod/<module>/@v/<ver>.mod      the go.mod file
//! GET  /gomod/<module>/@v/<ver>.zip      the module, every path prefixed
//! GET  /gomod/<module>/@latest           info for the newest version
//! ```
//!
//! Every one of those is something this store already answers: tags are
//! versions, a tag resolves to a commit, `go.mod` is a blob in that commit's
//! tree, and `archive::write_zip` already prefixes every entry — which is the
//! one thing Go is strict about.
//!
//! Two things consumers need to know, because neither is a bug here:
//!
//! * `GOPRIVATE=host/*` (or `GONOSUMDB`). By default the toolchain checks
//!   every module against `sum.golang.org`, which will never have heard of a
//!   private host, and the failure reads like this server is broken.
//! * A private repository needs credentials in `~/.netrc` for the host. The
//!   endpoints below authenticate exactly like the rest of the API, so a token
//!   that can read the repository can fetch the module.

use crate::error::{AppError, AppResult};
use crate::routes::browse::resolve_ref;
use crate::state::AppState;
use crate::auth::Viewer;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use fkit_core::hash::Hash;

pub fn routes() -> Router<AppState> {
    Router::new().route("/gomod/{*rest}", get(proxy))
}

/// Answer `?go-get=1` with the meta tag that points at the proxy above.
///
/// Go asks the import path itself — `https://host/alice/loom/inner?go-get=1` —
/// and reads the `go-import` tag whose first field is a prefix of what it is
/// looking for. So the same answer is correct at any depth under the
/// repository, and the tag always names the repository root rather than the
/// path that was asked for.
///
/// Returns `None` for anything that is not a repository path, so the caller
/// can fall through to the ordinary page.
pub fn go_import_page(host: &str, scheme: &str, path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let (owner, repo) = match parts.as_slice() {
        [owner, repo, ..] => (*owner, *repo),
        _ => return None,
    };
    // Reserved prefixes are the site's own, not somebody's repository.
    if matches!(owner, "api" | "assets" | "gomod" | "_health") {
        return None;
    }
    let root = format!("{host}/{owner}/{repo}");
    Some(format!(
        "<!doctype html>\n<html><head>\n\
         <meta name=\"go-import\" content=\"{root} mod {scheme}://{host}/gomod\">\n\
         <meta name=\"go-source\" content=\"{root} {scheme}://{host}/{owner}/{repo} \
         {scheme}://{host}/{owner}/{repo}/tree/main{{/dir}} \
         {scheme}://{host}/{owner}/{repo}/blob/main{{/dir}}/{{file}}#L{{line}}\">\n\
         </head><body>\n\
         <p>Module <code>{root}</code>. \
         Fetch it with <code>go get {root}</code>.</p>\n\
         </body></html>\n"
    ))
}

/// A module path, split into what this server can act on.
struct Target {
    /// The full module path as Go knows it, e.g. `host/alice/loom/v2`.
    module: String,
    owner: String,
    repo: String,
    /// The major version a `/vN` suffix demands, if there is one.
    major: Option<u64>,
}

/// Undo the proxy protocol's case encoding.
///
/// A module path is case-sensitive but has to survive case-insensitive
/// filesystems, so the protocol writes an uppercase letter as `!` plus its
/// lowercase form. `github.com/Masterminds` arrives as
/// `github.com/!masterminds`.
fn decode_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut bang = false;
    for c in s.chars() {
        if bang {
            out.extend(c.to_uppercase());
            bang = false;
        } else if c == '!' {
            bang = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// Split a module path into the repository it names.
///
/// `host/owner/repo` and `host/owner/repo/vN` are supported. A module living
/// in a subdirectory is not: Go would look for a `go.mod` down there, and
/// serving it would mean deciding which subtree is the module root — worth
/// doing deliberately rather than guessing.
fn split_module(raw: &str) -> AppResult<Target> {
    let module = decode_case(raw.trim_matches('/'));
    let parts: Vec<String> =
        module.split('/').filter(|p| !p.is_empty()).map(str::to_string).collect();

    // Drop the host. It is part of the module path but says nothing about
    // where the repository lives on *this* server.
    let rest = parts.get(1..).unwrap_or(&[]);

    let (owner, repo, major) = match rest {
        [owner, repo] => (owner.clone(), repo.clone(), None),
        [owner, repo, v] if v.starts_with('v') => {
            let n: u64 = v[1..]
                .parse()
                .map_err(|_| AppError::not_found("no such module"))?;
            if n < 2 {
                // v0 and v1 have no suffix; `/v1` is not a legal module path.
                return Err(AppError::not_found("no such module"));
            }
            (owner.clone(), repo.clone(), Some(n))
        }
        _ => return Err(AppError::not_found("no such module")),
    };

    Ok(Target { module, owner, repo, major })
}

/// A version this server will serve, parsed enough to sort and to filter.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    /// A pre-release suffix sorts *before* the same version without one.
    pre: Option<String>,
    raw: String,
}

impl SemVer {
    fn parse(tag: &str) -> Option<SemVer> {
        let body = tag.strip_prefix('v')?;
        // Build metadata is not part of precedence and Go rejects it in a
        // module version, so it is not accepted here either.
        if body.contains('+') {
            return None;
        }
        let (core, pre) = match body.split_once('-') {
            Some((c, p)) => (c, Some(p.to_string())),
            None => (body, None),
        };
        let mut it = core.split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next()?.parse().ok()?;
        let patch = it.next()?.parse().ok()?;
        if it.next().is_some() {
            return None;
        }
        Some(SemVer { major, minor, patch, pre, raw: tag.to_string() })
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                // A release outranks any pre-release of the same version.
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (a, b) => a.cmp(b),
            })
    }
}
impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The version Go invents for a commit that carries no tag.
///
/// `v0.0.0-<utc timestamp>-<12 hex>`. The format is not decorative: the
/// toolchain parses the timestamp to order two untagged commits, so it has to
/// be UTC and it has to be exactly fourteen digits.
fn pseudo_version(unix: i64, commit: Hash) -> String {
    let t = chrono::DateTime::from_timestamp(unix, 0).unwrap_or_default();
    format!("v0.0.0-{}-{}", t.format("%Y%m%d%H%M%S"), &commit.to_hex()[..12])
}

/// What `.info` must answer with.
///
/// Always a canonical version, never the string that was asked for. `go get
/// repo@main` reaches here as "main", and answering "main" is rejected by the
/// toolchain outright — a branch is not a version, so what it resolves to is a
/// pseudo-version naming the commit.
fn canonical_version(requested: &str, unix: i64, commit: Hash) -> String {
    match SemVer::parse(requested) {
        Some(_) => requested.to_string(),
        None => pseudo_version(unix, commit),
    }
}

/// The commit a pseudo-version names, if it is one.
///
/// The hash is abbreviated to twelve hex in the last field. Kept separate from
/// ref resolution because that resolves *names* and otherwise expects a full
/// 64-character hash — an abbreviation is neither, and asking for one is how
/// `go get repo@branch` comes back for its zip.
fn abbreviated_commit(version: &str) -> Option<&str> {
    let short = version.rsplit('-').next()?;
    (short.len() == 12 && short.bytes().all(|b| b.is_ascii_hexdigit())).then_some(short)
}

/// Whether a version belongs to the major series this module path asks for.
fn in_series(v: &SemVer, major: Option<u64>) -> bool {
    match major {
        Some(n) => v.major == n,
        // Without a `/vN` suffix a module is v0 or v1, and nothing else.
        None => v.major <= 1,
    }
}

async fn proxy(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(rest): Path<String>,
) -> AppResult<axum::response::Response> {
    // `<module>/@v/<file>` or `<module>/@latest`. Split on the marker rather
    // than counting segments, because a module path has no fixed length.
    let (raw_module, action) = match rest.rsplit_once("/@") {
        Some((m, a)) => (m, a),
        None => return Err(AppError::not_found("not a module proxy request")),
    };

    let t = split_module(raw_module)?;
    let (repo, _, _) = super::load_repo(&state, &viewer, &t.owner, &t.repo).await?;
    let store = state
        .store_for_network(repo.network_id)
        .map_err(AppError::Internal)?;

    // Tags are the versions. A branch is not a version: it moves, and a module
    // version may never change once published.
    let rows: Vec<(String, Vec<u8>)> =
        sqlx::query_as("SELECT name, target FROM refs WHERE repo_id = $1 AND name LIKE 'tags/%'")
            .bind(repo.id)
            .fetch_all(&state.db)
            .await?;

    let mut versions: Vec<(SemVer, Hash)> = rows
        .into_iter()
        .filter_map(|(name, target)| {
            let tag = name.strip_prefix(fkit_core::session::TAG_PREFIX)?;
            let v = SemVer::parse(tag)?;
            let bytes: [u8; 32] = target.try_into().ok()?;
            in_series(&v, t.major).then_some((v, Hash(bytes)))
        })
        .collect();
    versions.sort_by(|a, b| a.0.cmp(&b.0));

    if action == "latest" {
        return latest(&state, &repo, &store, &versions).await;
    }

    let file = action
        .strip_prefix("v/")
        .ok_or_else(|| AppError::not_found("not a module proxy request"))?;

    if file == "list" {
        // Pre-releases are deliberately absent: `go get` without an explicit
        // version should not pick one up, and it reads this list to decide.
        let body = versions
            .iter()
            .filter(|(v, _)| v.pre.is_none())
            .map(|(v, _)| v.raw.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // Newline-terminated when non-empty; an empty list is an empty body.
        let body = if body.is_empty() { body } else { body + "\n" };
        return Ok(text(body));
    }

    let (version, kind) = split_ext(file)?;
    let version = decode_case(&version);
    let commit = resolve_version(&state, &repo, &store, &versions, &version).await?;

    match kind {
        "info" => {
            let c = crate::content::commit_of(&store, commit)?;
            // The answer must be a *canonical* version, never the string that
            // was asked for. `go get repo@main` resolves the branch here, and
            // handing "main" back is rejected outright — a branch is not a
            // version, so what it resolves to is a pseudo-version.
            Ok(json_info(&canonical_version(&version, c.timestamp, commit), c.timestamp))
        }
        "mod" => {
            let tree = crate::content::commit_of(&store, commit)?.tree;
            Ok(text(go_mod(&store, tree, &t.module)))
        }
        "zip" => zip(&state, &store, commit, &t.module, &version).await,
        _ => Err(AppError::not_found("no such module file")),
    }
}

fn split_ext(file: &str) -> AppResult<(String, &'static str)> {
    for (ext, kind) in [(".info", "info"), (".mod", "mod"), (".zip", "zip")] {
        if let Some(v) = file.strip_suffix(ext) {
            return Ok((v.to_string(), kind));
        }
    }
    Err(AppError::not_found("no such module file"))
}

/// Turn whatever Go asked for into a commit.
///
/// A tagged version is a tag. Anything else — a pseudo-version, a branch name,
/// a raw hash — is resolved through the ordinary ref machinery, which is what
/// makes `go get host/owner/repo@main` work on a repository that has never
/// been tagged.
async fn resolve_version(
    state: &AppState,
    repo: &crate::models::RepoRow,
    store: &fkit_core::store::Store,
    versions: &[(SemVer, Hash)],
    version: &str,
) -> AppResult<Hash> {
    if let Some((_, h)) = versions.iter().find(|(v, _)| v.raw == version) {
        return Ok(*h);
    }

    // A pseudo-version carries the commit in its last field, abbreviated to
    // twelve hex. Not `resolve_ref`: that resolves a *name*, and falls back to
    // a full 64-character hash — an abbreviation is neither, and has to go to
    // the store to be widened.
    if let Some(short) = abbreviated_commit(version)
        && let Ok(h) = store.resolve_prefix(short)
    {
        return Ok(h);
    }

    resolve_ref(state, repo, version).await
}

async fn latest(
    state: &AppState,
    repo: &crate::models::RepoRow,
    store: &fkit_core::store::Store,
    versions: &[(SemVer, Hash)],
) -> AppResult<axum::response::Response> {
    if let Some((v, h)) = versions.iter().rfind(|(v, _)| v.pre.is_none()) {
        let c = crate::content::commit_of(store, *h)?;
        return Ok(json_info(&v.raw, c.timestamp));
    }
    // Untagged: the default branch's tip, as a pseudo-version. Without this a
    // module has to be tagged before it can be used at all, which is a worse
    // default than git's.
    let tip = resolve_ref(state, repo, &repo.default_branch).await?;
    let c = crate::content::commit_of(store, tip)?;
    Ok(json_info(&pseudo_version(c.timestamp, tip), c.timestamp))
}

/// The module's `go.mod`.
///
/// Synthesized when the repository has none, which is what every proxy does
/// for a module that predates them: Go requires the file to exist, and the
/// only thing it must contain is the module path.
fn go_mod(store: &fkit_core::store::Store, tree: Hash, module: &str) -> String {
    match crate::content::read_blob(store, tree, "go.mod") {
        Ok(b) if !b.binary && !b.truncated => {
            String::from_utf8(b.bytes).unwrap_or_else(|_| format!("module {module}\n"))
        }
        _ => format!("module {module}\n"),
    }
}

async fn zip(
    state: &AppState,
    store: &fkit_core::store::Store,
    commit: Hash,
    module: &str,
    version: &str,
) -> AppResult<axum::response::Response> {
    let tree = crate::content::commit_of(store, commit)?.tree;
    let plan = fkit_core::archive::plan(store, tree, "").map_err(AppError::Internal)?;

    let limit = state.max_archive_bytes;
    if limit > 0 && plan.bytes > limit {
        return Err(AppError::bad("that module is larger than this server will serve"));
    }

    // Go requires every path in the zip to sit under `module@version/`, and
    // rejects the archive outright otherwise.
    let root = format!("{module}@{version}");
    let mut buf = Vec::with_capacity(plan.bytes as usize + 4096);
    fkit_core::archive::write_zip(store, &plan, &root, &mut buf).map_err(AppError::Internal)?;

    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/zip")],
        buf,
    )
        .into_response())
}

fn text(body: String) -> axum::response::Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}

fn json_info(version: &str, unix: i64) -> axum::response::Response {
    let t = chrono::DateTime::from_timestamp(unix, 0).unwrap_or_default();
    axum::Json(serde_json::json!({
        "Version": version,
        "Time": t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_points_at_the_proxy_and_names_the_repository_root() {
        let html = go_import_page("fkit.work", "https", "/alice/loom").unwrap();
        assert!(
            html.contains(r#"content="fkit.work/alice/loom mod https://fkit.work/gomod""#),
            "{html}"
        );
        // A path inside the repository answers for the repository, because Go
        // walks up from the import path it was given.
        let deep = go_import_page("fkit.work", "https", "/alice/loom/inner/pkg").unwrap();
        assert!(deep.contains("fkit.work/alice/loom mod"), "{deep}");
    }

    #[test]
    fn the_sites_own_paths_are_not_modules() {
        for p in ["/api/repos", "/assets/app.js", "/gomod/x/@v/list", "/", "/alice"] {
            assert!(go_import_page("h", "https", p).is_none(), "{p} should not answer");
        }
    }

    #[test]
    fn an_encoded_module_path_decodes_to_its_real_case() {
        assert_eq!(decode_case("github.com/!masterminds/semver"), "github.com/Masterminds/semver");
        assert_eq!(decode_case("host/alice/loom"), "host/alice/loom");
        assert_eq!(decode_case("!a!b"), "AB");
    }

    #[test]
    fn a_module_path_names_a_repository() {
        let t = split_module("fkit.work/alice/loom").unwrap();
        assert_eq!((t.owner.as_str(), t.repo.as_str(), t.major), ("alice", "loom", None));

        let t = split_module("fkit.work/alice/loom/v3").unwrap();
        assert_eq!((t.owner.as_str(), t.repo.as_str(), t.major), ("alice", "loom", Some(3)));
        assert_eq!(t.module, "fkit.work/alice/loom/v3");
    }

    #[test]
    fn a_path_that_is_not_a_repository_is_refused() {
        // `/v1` is not a legal module suffix, and a subdirectory module would
        // need a decision about where its root is.
        for bad in ["fkit.work/alice/loom/v1", "fkit.work/alice", "fkit.work/a/b/c/d", "x"] {
            assert!(split_module(bad).is_err(), "{bad} should not resolve");
        }
    }

    #[test]
    fn versions_sort_the_way_go_expects() {
        let mut v: Vec<SemVer> = ["v1.2.0", "v1.10.0", "v1.2.3", "v2.0.0", "v1.2.3-rc1"]
            .iter()
            .map(|s| SemVer::parse(s).unwrap())
            .collect();
        v.sort();
        let order: Vec<&str> = v.iter().map(|s| s.raw.as_str()).collect();
        assert_eq!(order, ["v1.2.0", "v1.2.3-rc1", "v1.2.3", "v1.10.0", "v2.0.0"]);
    }

    #[test]
    fn a_tag_that_is_not_a_version_is_not_one() {
        for bad in ["latest", "v1", "v1.2", "1.2.3", "v1.2.3.4", "v1.2.3+meta", "release-1"] {
            assert!(SemVer::parse(bad).is_none(), "{bad} should not parse");
        }
    }

    #[test]
    fn a_major_suffix_selects_its_own_series() {
        let v2 = SemVer::parse("v2.1.0").unwrap();
        let v1 = SemVer::parse("v1.4.0").unwrap();
        let v0 = SemVer::parse("v0.3.0").unwrap();

        // No suffix means v0 and v1 only — v2 lives at a different path.
        assert!(in_series(&v0, None) && in_series(&v1, None) && !in_series(&v2, None));
        assert!(in_series(&v2, Some(2)) && !in_series(&v1, Some(2)));
    }

    #[test]
    fn a_branch_resolves_to_a_pseudo_version_and_never_to_its_own_name() {
        let h = Hash::from_hex(&"21".repeat(32)).unwrap();
        // The bug this covers: answering `{"Version": "main"}` for `@main`,
        // which the toolchain rejects as a non-semver module version.
        assert_eq!(
            canonical_version("main", 1_700_000_000, h),
            "v0.0.0-20231114221320-212121212121"
        );
        // A real version is passed through untouched.
        assert_eq!(canonical_version("v1.2.0", 1_700_000_000, h), "v1.2.0");
    }

    #[test]
    fn a_pseudo_version_gives_back_the_commit_it_names() {
        // And the round trip, which is what `go get repo@branch` does: resolve
        // the branch to a pseudo-version, then ask for that version's zip.
        let h = Hash::from_hex(&"21".repeat(32)).unwrap();
        let v = pseudo_version(1_700_000_000, h);
        assert_eq!(abbreviated_commit(&v), Some("212121212121"));
        assert_eq!(abbreviated_commit(&v).unwrap(), &h.to_hex()[..12]);

        // A tagged version carries no commit, and must not be mistaken for one.
        assert_eq!(abbreviated_commit("v1.2.0"), None);
        assert_eq!(abbreviated_commit("v1.2.0-rc1"), None);
        // Twelve characters that are not hex are not a commit either.
        assert_eq!(abbreviated_commit("v0.0.0-x-zzzzzzzzzzzz"), None);
    }

    #[test]
    fn a_pseudo_version_has_the_shape_the_toolchain_parses() {
        let h = Hash::from_hex(&"ab".repeat(32)).unwrap();
        let v = pseudo_version(1_700_000_000, h);
        assert_eq!(v, "v0.0.0-20231114221320-abababababab");
        // Fourteen digits and twelve hex, exactly.
        let parts: Vec<&str> = v.splitn(3, '-').collect();
        assert_eq!(parts[1].len(), 14);
        assert_eq!(parts[2].len(), 12);
    }

    #[test]
    fn the_proxy_file_names_are_recognised() {
        assert_eq!(split_ext("v1.2.3.info").unwrap(), ("v1.2.3".into(), "info"));
        assert_eq!(split_ext("v1.2.3.mod").unwrap(), ("v1.2.3".into(), "mod"));
        assert_eq!(split_ext("v1.2.3.zip").unwrap(), ("v1.2.3".into(), "zip"));
        assert!(split_ext("v1.2.3.tar").is_err());
    }
}
