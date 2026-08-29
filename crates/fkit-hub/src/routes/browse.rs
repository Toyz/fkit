//! Read-only repository browsing: trees, blobs, history, and diffs.
//!
//! Every handler resolves a *ref* (branch name or commit hash) to a commit, then
//! works from that commit's tree. Resolution is centralised in [`resolve_ref`]
//! so a branch name can never be mistaken for a hash, or vice versa.

use crate::auth::Viewer;
use crate::content;
use fkit_core::archive::EPOCH;
use crate::error::{AppError, AppResult};
use crate::models::RepoRow;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use fkit_core::hash::Hash;
use fkit_core::store::Store;
use serde::{Deserialize, Serialize};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/repos/{owner}/{name}/tree/{ref}", get(tree_root))
        .route("/repos/{owner}/{name}/tree/{ref}/{*path}", get(tree_path))
        .route("/repos/{owner}/{name}/blob/{ref}/{*path}", get(blob))
        .route("/repos/{owner}/{name}/object/{hash}", get(object))
        .route("/repos/{owner}/{name}/commits/{ref}", get(commits))
        .route("/repos/{owner}/{name}/commit/{hash}", get(commit_detail))
        .route("/repos/{owner}/{name}/archive/{spec}", get(archive))
        .route("/repos/{owner}/{name}/readme/{ref}", get(readme))
        .route("/repos/{owner}/{name}/readme/{ref}/{*path}", get(readme_at))
        .route("/repos/{owner}/{name}/lastcommits/{ref}", get(last_commits_root))
        .route("/repos/{owner}/{name}/lastcommits/{ref}/{*path}", get(last_commits_path))
        .route("/repos/{owner}/{name}/raw/{ref}/{*path}", get(raw))
        .route("/repos/{owner}/{name}/patch/{hash}", get(patch))
        // Two spellings on purpose. The path form is the readable one and
        // still serves a plain `main`; the query form is the one that survives
        // a proxy, because a spec like `aria/fkit:main` contains a slash and
        // `%2F` does not reliably reach here intact — a dev server and most
        // reverse proxies normalise it back into a path separator.
        .route("/repos/{owner}/{name}/compare", get(compare_query))
        .route("/repos/{owner}/{name}/compare/{base}/{head}", get(compare))
}

/// Resolve a branch name or hex commit hash to a commit hash.
///
/// Branch names are tried first: if someone names a branch after a valid hex
/// string, the branch is what they meant.
/// Look a ref name up, distinguishing "no such ref" from a database failure.
///
/// Separate from [`resolve_ref`] because widening a ref across a path tries
/// several candidates, and `Err` there has to keep meaning something went
/// wrong — swallowing a connection error as "not found" would turn an outage
/// into a 404.
async fn lookup_ref(state: &AppState, repo: &RepoRow, name: &str) -> AppResult<Option<Hash>> {
    let row: Option<(Vec<u8>,)> =
        sqlx::query_as("SELECT target FROM refs WHERE repo_id = $1 AND name = $2")
            .bind(repo.id)
            .bind(name)
            .fetch_optional(&state.db)
            .await?;

    let Some((bytes,)) = row else { return Ok(None) };
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("corrupt ref target")))?;
    Ok(Some(Hash(arr)))
}

/// Resolve a URL ref — a branch, a tag, or a commit hash.
///
/// Tags are stored prefixed, so a URL saying `v1.0` has to be tried as
/// `tags/v1.0` as well. Branch first: a branch and a tag may share a name, and
/// the branch is the one someone browsing is more likely to mean. The prefixed
/// spelling is also accepted, so a link that already carries it still works.
pub(crate) async fn resolve_ref(state: &AppState, repo: &RepoRow, spec: &str) -> AppResult<Hash> {
    if let Some(h) = try_resolve_ref(state, repo, spec).await? {
        return Ok(h);
    }
    Err(AppError::not_found(format!(
        "no such branch, tag or commit: {spec}"
    )))
}

async fn try_resolve_ref(
    state: &AppState,
    repo: &RepoRow,
    spec: &str,
) -> AppResult<Option<Hash>> {
    let tagged = format!("{}{spec}", fkit_core::session::TAG_PREFIX);
    let candidates: [&str; 2] = if fkit_core::session::is_tag(spec) {
        [spec, spec]
    } else {
        [spec, tagged.as_str()]
    };

    for name in candidates {
        if let Some(h) = lookup_ref(state, repo, name).await? {
            return Ok(Some(h));
        }
    }

    Ok(Hash::from_hex(spec))
}

/// Split `<ref>/<path>` when the ref itself contains slashes.
///
/// `/tree/feature/settings-redesign/web` is ambiguous: the branch could be
/// `feature` holding `settings-redesign/web`, or `feature/settings-redesign`
/// holding `web`. The router has to guess, so it guesses shortest and this
/// widens the guess against the refs that actually exist.
///
/// The client percent-encodes the slash, which makes the whole name one path
/// segment and avoids the question — but only as far as the first proxy that
/// normalises `%2F` back into a slash, which a dev server and most reverse
/// proxies do. Resolving it here means the URL works either way.
async fn resolve_ref_in_path(
    state: &AppState,
    repo: &RepoRow,
    spec: &str,
    path: &str,
) -> AppResult<(Hash, String)> {
    // The literal spec wins, so no link that already worked changes meaning.
    if let Some(h) = try_resolve_ref(state, repo, spec).await? {
        return Ok((h, path.to_string()));
    }

    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // Longest first: given both `a/b` and `a/b/c`, the deeper name is the one
    // the URL was more specific about.
    for take in (1..=segs.len()).rev() {
        let candidate = format!("{spec}/{}", segs[..take].join("/"));
        if let Some(h) = lookup_ref(state, repo, &candidate).await? {
            return Ok((h, segs[take..].join("/")));
        }
        let tagged = format!("{}{candidate}", fkit_core::session::TAG_PREFIX);
        if let Some(h) = lookup_ref(state, repo, &tagged).await? {
            return Ok((h, segs[take..].join("/")));
        }
    }

    Err(AppError::not_found(format!(
        "no such branch, tag or commit: {spec}"
    )))
}

/// The preamble for a handler that carries a path the ref may have eaten into.
async fn open_in_path(
    state: &AppState,
    viewer: &Viewer,
    owner: &str,
    name: &str,
    spec: &str,
    path: &str,
) -> AppResult<(Store, Hash, Hash, String)> {
    let (repo, _, _) = super::load_repo(state, viewer, owner, name).await?;
    let (commit_id, rest) = resolve_ref_in_path(state, &repo, spec, path).await?;
    let store = state.store_for_network(repo.network_id).map_err(AppError::Internal)?;
    let commit = content::commit_of(&store, commit_id)?;
    Ok((store, commit_id, commit.tree, rest))
}

/// Open the store and resolve the ref — the common preamble of every handler.
async fn open(
    state: &AppState,
    viewer: &Viewer,
    owner: &str,
    name: &str,
    spec: &str,
) -> AppResult<(Store, Hash, Hash)> {
    let (repo, _, _) = super::load_repo(state, viewer, owner, name).await?;
    let commit_id = resolve_ref(state, &repo, spec).await?;
    let store = state.store_for_network(repo.network_id).map_err(AppError::Internal)?;
    let commit = content::commit_of(&store, commit_id)?;
    Ok((store, commit_id, commit.tree))
}

#[derive(Serialize)]
struct TreeResponse {
    path: String,
    commit: String,
    entries: Vec<content::EntryView>,
}

async fn tree_root(
    State(state): State<AppState>,
    viewer: Viewer,
    headers: axum::http::HeaderMap,
    Path((owner, name, r)): Path<(String, String, String)>,
) -> AppResult<Json<TreeResponse>> {
    let (store, commit, tree) = open(&state, &viewer, &owner, &name, &r).await?;
    let mut entries = content::list_dir(&store, tree, "")?;
    link_submodules(&state, &viewer, host_of(&headers), &owner, &name, &mut entries).await;
    Ok(Json(TreeResponse {
        entries,
        path: String::new(),
        commit: commit.to_hex(),
    }))
}

/// The host this request was addressed to, if it said.
fn host_of(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers.get(axum::http::header::HOST).and_then(|v| v.to_str().ok())
}

/// Point a submodule entry at the repository it pins, when that repository is
/// on this hub and the viewer is allowed to know it exists.
///
/// The check is `load_repo`, which already refuses to distinguish a repository
/// that is absent from one that is merely invisible. A pin into a private
/// repository therefore renders as an ordinary pin, and the missing link
/// discloses nothing the viewer could not already have guessed.
///
/// Only relative suggestions are resolved. An absolute URL is shown as written
/// rather than matched against this hub: without knowing our own public host —
/// which is configured for mail, not for this — a same-named repository
/// elsewhere would link to the wrong place, and a wrong link is worse than
/// none.
async fn link_submodules(
    state: &AppState,
    viewer: &Viewer,
    host: Option<&str>,
    owner: &str,
    name: &str,
    entries: &mut [content::EntryView],
) {
    let here = format!("{owner}/{name}");
    for e in entries.iter_mut().filter(|e| e.kind == "submodule") {
        let Some(hint) = e.remote.as_deref() else { continue };

        // Relative to this repository, or absolute at the host this request
        // arrived on. Anything else is a different server and gets no link.
        //
        // Host is client-supplied and so cannot be trusted for authorisation,
        // but it is not being used for any: it only decides whether to attempt
        // a lookup, and the lookup itself is permission-checked below. A forged
        // Host can therefore reveal nothing the viewer could not already see.
        let path = fkit_core::submodule::resolve_relative(&here, hint)
            .or_else(|| host.and_then(|h| fkit_core::submodule::path_on_host(h, hint)));
        let Some(path) = path else { continue };

        let Some((o, n)) = path.split_once('/') else { continue };
        if o.is_empty() || n.is_empty() || n.contains('/') {
            continue;
        }
        if super::load_repo(state, viewer, o, n).await.is_ok() {
            e.target = Some(path);
        }
    }
}

async fn tree_path(
    State(state): State<AppState>,
    viewer: Viewer,
    headers: axum::http::HeaderMap,
    Path((owner, name, r, path)): Path<(String, String, String, String)>,
) -> AppResult<Json<TreeResponse>> {
    let (store, commit, tree, path) =
        open_in_path(&state, &viewer, &owner, &name, &r, &path).await?;
    let mut entries = content::list_dir(&store, tree, &path)?;
    link_submodules(&state, &viewer, host_of(&headers), &owner, &name, &mut entries).await;
    Ok(Json(TreeResponse {
        entries,
        path,
        commit: commit.to_hex(),
    }))
}

#[derive(Serialize)]
struct BlobResponse {
    path: String,
    hash: String,
    size: u64,
    binary: bool,
    truncated: bool,
    /// Absent for binary or oversized files.
    content: Option<String>,
    lines: usize,
    /// Set when the bytes are an image the browser can display, so the client
    /// can show the picture instead of "binary file". The raw endpoint serves
    /// this same type, so the `<img>` actually loads under `nosniff`.
    image: Option<&'static str>,
}

/// A file's content by its own hash, with no ref and no path involved.
///
/// The point of a content-addressed store, expressed as an endpoint: a hash
/// names one byte sequence forever, so this answers the same thing today and
/// in five years regardless of what any branch has done since. It is what lets
/// an issue anchored to code still show the code it was about after the file
/// has moved on.
///
/// Reachability is not checked, and does not need to be. A caller can only ask
/// about a hash it already knows, and a hash is only learnable from content
/// this repository served — so this discloses nothing that the ordinary blob
/// endpoint would not. Permission on the repository is still required, and a
/// hash from a *different* network's store is simply absent here.
async fn object(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, hash)): Path<(String, String, String)>,
) -> AppResult<Json<BlobResponse>> {
    let (repo, _, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    let store = state.store_for_network(repo.network_id).map_err(AppError::Internal)?;

    let id = Hash::from_hex(hash.trim())
        .ok_or_else(|| AppError::bad("that is not a hash"))?;
    let b = content::read_object(&store, id)?;

    let text = if b.binary || b.truncated {
        None
    } else {
        String::from_utf8(b.bytes.clone()).ok()
    };

    Ok(Json(BlobResponse {
        // The hash knows nothing about names; whoever asked has the path.
        path: String::new(),
        hash: b.hash.to_hex(),
        size: b.size,
        binary: b.binary || text.is_none(),
        truncated: b.truncated,
        lines: text.as_deref().map(|t| t.lines().count()).unwrap_or(0),
        content: text,
        image: b.image,
    }))
}

async fn blob(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r, path)): Path<(String, String, String, String)>,
) -> AppResult<Json<BlobResponse>> {
    let (store, _, tree, path) =
        open_in_path(&state, &viewer, &owner, &name, &r, &path).await?;
    let b = content::read_blob(&store, tree, &path)?;

    let text = if b.binary || b.truncated {
        None
    } else {
        String::from_utf8(b.bytes.clone()).ok()
    };

    Ok(Json(BlobResponse {
        path,
        hash: b.hash.to_hex(),
        size: b.size,
        binary: b.binary || text.is_none(),
        truncated: b.truncated,
        lines: text.as_deref().map(|t| t.lines().count()).unwrap_or(0),
        content: text,
        image: b.image,
    }))
}

#[derive(Deserialize)]
struct Page {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    skip: usize,
}
fn default_limit() -> usize {
    50
}

async fn commits(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r)): Path<(String, String, String)>,
    Query(page): Query<Page>,
) -> AppResult<Json<Vec<content::CommitView>>> {
    let (store, commit, _) = open(&state, &viewer, &owner, &name, &r).await?;
    let limit = page.limit.clamp(1, 200);
    let mut views = content::history(&store, commit, limit, page.skip)?;
    content::attach_authors(&state.db, &mut views).await;
    Ok(Json(views))
}

#[derive(Serialize)]
struct CommitDetail {
    #[serde(flatten)]
    commit: content::CommitView,
    changes: Vec<content::ChangeView>,
}

async fn commit_detail(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, hash)): Path<(String, String, String)>,
) -> AppResult<Json<CommitDetail>> {
    let (repo, _, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    let store = state.store_for_network(repo.network_id).map_err(AppError::Internal)?;
    let id = Hash::from_hex(&hash).ok_or_else(|| AppError::bad("not a valid commit hash"))?;
    let c = content::commit_of(&store, id)?;

    Ok(Json(CommitDetail {
        commit: {
            let mut v = [content::to_view(id, &c)];
            content::attach_authors(&state.db, &mut v).await;
            let [one] = v;
            one
        },
        changes: content::commit_diff(&store, id)?,
    }))
}

/// How far back to look for the commit that last touched each entry.
const LAST_COMMIT_SCAN: usize = 500;

async fn last_commits_root(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r)): Path<(String, String, String)>,
) -> AppResult<Json<std::collections::HashMap<String, content::LastCommit>>> {
    let (store, commit, _) = open(&state, &viewer, &owner, &name, &r).await?;
    Ok(Json(content::last_commits(&store, commit, "", LAST_COMMIT_SCAN)?))
}

async fn last_commits_path(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r, path)): Path<(String, String, String, String)>,
) -> AppResult<Json<std::collections::HashMap<String, content::LastCommit>>> {
    let (store, commit, _, path) =
        open_in_path(&state, &viewer, &owner, &name, &r, &path).await?;
    Ok(Json(content::last_commits(&store, commit, &path, LAST_COMMIT_SCAN)?))
}

/// Raw file bytes.
///
/// Always served as `text/plain` (or an opaque octet-stream) with `nosniff` and
/// a restrictive CSP, **never** with the file's apparent type. Repository
/// content is attacker-controlled: handing back a pushed `.html` as
/// `text/html` on this origin would execute it with the viewer's session
/// cookie. GitHub serves raw content from a separate domain for exactly this
/// reason; we do not have one, so the headers have to carry the weight.
async fn raw(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r, path)): Path<(String, String, String, String)>,
) -> AppResult<axum::response::Response> {
    use axum::http::header;
    use axum::response::IntoResponse;

    let (store, _, tree, path) =
        open_in_path(&state, &viewer, &owner, &name, &r, &path).await?;
    let (bytes, _size) = content::raw_blob(&store, tree, &path)?;

    // An image may be typed honestly: the formats below are decoded as pixels
    // and cannot execute anything. Everything else keeps the blanket
    // text/plain-or-octet-stream treatment described above — including SVG,
    // which is a scriptable document wearing an image's file extension.
    let is_text = std::str::from_utf8(&bytes).is_ok()
        && !bytes.iter().take(8192).any(|b| *b == 0);
    let ctype = match content::image_mime(&bytes) {
        Some(mime) => mime,
        None if is_text => "text/plain; charset=utf-8",
        None => "application/octet-stream",
    };

    Ok((
        [
            (header::CONTENT_TYPE, ctype),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; sandbox; style-src 'unsafe-inline'",
            ),
            (header::CACHE_CONTROL, "max-age=300, private"),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Serialize)]
struct PatchResponse {
    files: Vec<content::FileDiff>,
    /// More files changed than were diffed.
    truncated: bool,
}

async fn patch(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, hash)): Path<(String, String, String)>,
) -> AppResult<Json<PatchResponse>> {
    let (repo, _, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    let store = state.store_for_network(repo.network_id).map_err(AppError::Internal)?;
    let id = Hash::from_hex(&hash).ok_or_else(|| AppError::bad("not a valid commit hash"))?;
    let (files, truncated) = content::commit_patch(&store, id)?;
    Ok(Json(PatchResponse { files, truncated }))
}

/// Compare two refs — the merge preview.
/// Resolve one side of a comparison, which may name another fork.
///
/// `main` is a ref in this repository. `aria/fkit:main` is a ref in that one —
/// the spelling a fork's page needs in order to say how far it has drifted, or
/// to propose its own branch. Only within the network: two repositories that
/// never shared a history do not share a store, so the objects on one side
/// would simply not be there.
async fn resolve_side(
    state: &AppState,
    viewer: &Viewer,
    here: &RepoRow,
    spec: &str,
) -> AppResult<Hash> {
    // A colon separates the repository from the ref. Branch names may contain
    // slashes but not colons, so this split is unambiguous.
    let Some((repo_spec, r)) = spec.split_once(':') else {
        return resolve_ref(state, here, spec).await;
    };
    let Some((o, n)) = repo_spec.split_once('/') else {
        return Err(AppError::BadRequest(
            "a cross-repository ref is owner/name:branch".into(),
        ));
    };

    let (other, access, _) = super::load_repo(state, viewer, o, n).await?;
    crate::perms::require_read(access, o, n)?;
    if other.network_id != here.network_id {
        return Err(AppError::BadRequest(format!(
            "{o}/{n} is not a fork of this repository"
        )));
    }
    resolve_ref(state, &other, r).await
}

#[derive(Deserialize)]
struct CompareQuery {
    base: String,
    head: String,
}

async fn compare_query(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<CompareQuery>,
) -> AppResult<Json<content::Comparison>> {
    compare(
        State(state),
        viewer,
        Path((owner, name, q.base, q.head)),
    )
    .await
}

async fn compare(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, base, head)): Path<(String, String, String, String)>,
) -> AppResult<Json<content::Comparison>> {
    let (repo, _, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    let store = state.store_for_network(repo.network_id).map_err(AppError::Internal)?;

    let base_id = resolve_side(&state, &viewer, &repo, &base).await?;
    let head_id = resolve_side(&state, &viewer, &repo, &head).await?;

    let mut cmp = content::compare(&store, &base, base_id, &head, head_id)?;
    content::attach_authors(&state.db, &mut cmp.commits).await;
    Ok(Json(cmp))
}

#[derive(Serialize)]
struct ReadmeResponse {
    name: String,
    content: String,
}

async fn readme(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r)): Path<(String, String, String)>,
) -> AppResult<Json<Option<ReadmeResponse>>> {
    let (store, _, tree) = open(&state, &viewer, &owner, &name, &r).await?;
    Ok(Json(
        content::find_readme(&store, tree).map(|(n, c)| ReadmeResponse { name: n, content: c }),
    ))
}

/// The README of a subdirectory.
///
/// `find_readme` already works on any tree — it was only ever called with the
/// root — so browsing into a directory that documents itself now shows that
/// documentation, the same as the top level does.
async fn readme_at(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, r, path)): Path<(String, String, String, String)>,
) -> AppResult<Json<Option<ReadmeResponse>>> {
    let (store, _, tree, path) =
        open_in_path(&state, &viewer, &owner, &name, &r, &path).await?;
    let dir = content::resolve_dir(&store, tree, &path)?;
    Ok(Json(
        content::find_readme(&store, dir).map(|(n, c)| ReadmeResponse { name: n, content: c }),
    ))
}


// ---- archives -----------------------------------------------------------

/// Stream a `.tar`, `.tar.gz` or `.zip` of a tree.
///
/// The URL carries the format as an extension — `archive/main.zip` — so the
/// browser gets a sensible filename without a header having to argue for one.
///
/// Nothing is buffered. The writer runs on a blocking thread and pushes into a
/// bounded channel that the response body drains, so a slow client applies
/// backpressure instead of filling memory, and a client that disappears kills
/// the walk on its next write rather than reading a repository into a closed
/// socket.
async fn archive(
    State(state): State<AppState>,
    viewer: Viewer,
    headers: axum::http::HeaderMap,
    Path((owner, name, spec)): Path<(String, String, String)>,
) -> AppResult<axum::response::Response> {
    use axum::http::header;
    use axum::response::IntoResponse;

    // Longest extension first: "main.tar.gz" is a tarball, not a tar named
    // "main.tar" with a stray suffix.
    let (r, format) = if let Some(base) = spec.strip_suffix(".tar.gz") {
        (base, Format::TarGz)
    } else if let Some(base) = spec.strip_suffix(".tgz") {
        (base, Format::TarGz)
    } else if let Some(base) = spec.strip_suffix(".tar") {
        (base, Format::Tar)
    } else if let Some(base) = spec.strip_suffix(".zip") {
        (base, Format::Zip)
    } else {
        return Err(AppError::bad(
            "name the format in the extension: .zip, .tar or .tar.gz",
        ));
    };
    if r.is_empty() {
        return Err(AppError::bad("no branch, tag or commit named"));
    }

    let (repo, _, _) = super::load_repo(&state, &viewer, &owner, &name).await?;
    let commit_id = resolve_ref(&state, &repo, r).await?;
    let store = state.store_for_network(repo.network_id).map_err(AppError::Internal)?;
    let tree = content::commit_of(&store, commit_id)?.tree;

    // The archive of a tree is a pure function of the tree and the format, and
    // the writers are deterministic — so this tag is not a heuristic, it is
    // exact, and it can never need revalidating. A repeat visit costs a 304.
    let etag = format!("\"{}-{}\"", tree.to_hex(), format.ext());
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        return Ok((axum::http::StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    // Directory objects only: the size is known before a byte of content is
    // read, so an oversized request is refused now rather than half way
    // through a download.
    let plan = fkit_core::archive::plan(&store, tree, "").map_err(AppError::Internal)?;
    let limit = state.max_archive_bytes;
    if limit > 0 && plan.bytes > limit {
        return Err(AppError::bad(format!(
            "that archive would hold {} of content, and this server's limit is {}",
            human(plan.bytes),
            human(limit)
        )));
    }

    // `<repo>-<ref>`, with anything path-shaped flattened: a ref may contain a
    // slash, and a Content-Disposition filename may not.
    let stem = format!("{}-{}", repo.name, r.replace(['/', '\\'], "-"));
    let filename = format!("{stem}.{}", format.ext());
    let root = stem.clone();

    // Bounded, so a slow reader stops the writer instead of queueing the whole
    // repository in memory. Eight buffers is enough to keep the socket fed.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
    let tar_len = plan.tar_size();

    tokio::task::spawn_blocking(move || {
        let mut sink = ChannelWriter { tx: tx.clone(), buf: Vec::with_capacity(64 * 1024) };
        let result = match format {
            Format::Tar => fkit_core::archive::write_tar(&store, &plan, &root, EPOCH, &mut sink)
                .and_then(|()| sink.finish().map_err(Into::into)),
            Format::TarGz => {
                let mut gz = flate2::write::GzEncoder::new(sink, flate2::Compression::fast());
                fkit_core::archive::write_tar(&store, &plan, &root, EPOCH, &mut gz)
                    .and_then(|()| gz.finish()?.finish().map_err(Into::into))
            }
            Format::Zip => fkit_core::archive::write_zip(&store, &plan, &root, &mut sink)
                .and_then(|()| sink.finish().map_err(Into::into)),
        };
        // A failure part way through cannot un-send what has already gone, so
        // the body is truncated and the error is logged. The client sees a
        // short archive, which every extractor reports as corrupt — the honest
        // outcome, and the reason the size checks above happen up front.
        if let Err(e) = result {
            tracing::warn!("archive of {owner}/{name} failed part way: {e:#}");
            let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
        }
    });

    let body = axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));

    let mut res = axum::response::Response::builder()
        .header(header::CONTENT_TYPE, format.mime())
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .header(header::ETAG, etag)
        // Immutable: the tree hash is in the tag, so this body can never change.
        .header(header::CACHE_CONTROL, "private, max-age=31536000, immutable");

    // Only tar has a predictable length. Compressing or deflating would make
    // any number here a guess, and a wrong Content-Length is worse than none.
    if let Format::Tar = format {
        res = res.header(header::CONTENT_LENGTH, tar_len);
    }

    res.body(body).map_err(|e| AppError::Internal(anyhow::anyhow!(e)))
}

#[derive(Clone, Copy)]
enum Format {
    Tar,
    TarGz,
    Zip,
}

impl Format {
    fn ext(self) -> &'static str {
        match self {
            Format::Tar => "tar",
            Format::TarGz => "tar.gz",
            Format::Zip => "zip",
        }
    }
    fn mime(self) -> &'static str {
        match self {
            Format::Tar => "application/x-tar",
            Format::TarGz => "application/gzip",
            Format::Zip => "application/zip",
        }
    }
}

/// A `Write` that forwards into the response channel.
///
/// Batched: the archive writers make many small writes — a 512-byte tar header,
/// then a chunk — and one channel message each would be all overhead. A send
/// that fails means the client is gone, which is reported as a broken pipe so
/// the walk stops immediately.
struct ChannelWriter {
    tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    buf: Vec<u8>,
}

impl ChannelWriter {
    fn push(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = bytes::Bytes::from(std::mem::take(&mut self.buf));
        self.buf = Vec::with_capacity(64 * 1024);
        self.tx
            .blocking_send(Ok(chunk))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client went away"))
    }

    fn finish(mut self) -> std::io::Result<()> {
        self.push()
    }
}

impl std::io::Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= 64 * 1024 {
            self.push()?;
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.push()
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}
