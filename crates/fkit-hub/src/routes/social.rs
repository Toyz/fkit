//! The endpoints a link preview actually fetches: the card image and oEmbed.
//!
//! Both go through `embed::describe`, so a page's picture and its oEmbed
//! document can never disagree with the meta tags in its `<head>` — and, more
//! to the point, neither can describe something the meta tags would refuse to.

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::collections::HashMap;

use crate::embed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/og/{*path}", get(card))
        .route("/oembed", get(oembed))
}

/// Where this server is, as an absolute URL.
///
/// The configured public URL wins. Falling back to the request's own `Host` is
/// a guess — the header is client-supplied — but the only thing it can affect
/// is which host a preview fetches the card from, and a forged value simply
/// sends the forger's own preview somewhere useless.
pub fn base_url(state: &AppState, headers: &HeaderMap) -> String {
    let configured = state.policy().public_url;
    if !configured.trim().is_empty() {
        return configured.trim_end_matches('/').to_string();
    }
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let scheme = if state.secure_cookies { "https" } else { "http" };
    format!("{scheme}://{host}")
}

// ---- the card image ------------------------------------------------------

/// How long a rendered card is reused. Short, because a card carries the tip
/// hash and the description, and a stale one is a wrong one.
const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

async fn card(State(state): State<AppState>, headers: HeaderMap, Path(path): Path<String>) -> Response {
    let base = base_url(&state, &headers);

    // `/og/<something>.png` describes the page at `/<something>`.
    let Some(page) = path.strip_suffix(".png") else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // Shared where a shared cache is configured, so a second hub does not
    // re-render what the first already drew, and a busy channel's fan-out of
    // fetches costs one render between all of them rather than one each.
    let key = format!("og:{page}");
    if let Some(hit) = state.blob_cache.get(&key) {
        return png(hit.as_ref().clone());
    }

    // "site" is the one card with no page behind it.
    let meta = if page == "site" {
        Some(embed::site_meta(&base))
    } else {
        embed::describe(&state, &format!("/{page}"), &base).await
    };

    // No card for anything not publicly readable. Substituting the site card
    // would put a picture behind a link that should not have a preview at all,
    // and 404 gives nothing away: a private repository and a repository that
    // does not exist answer identically, which is the distinction every other
    // endpoint refuses to make.
    let Some(card) = meta.and_then(|m| m.card) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let Some(bytes) = embed::render_png(&card) else {
        tracing::warn!("could not render the card for /{page}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    state.blob_cache.put(&key, std::sync::Arc::new(bytes.clone()), CACHE_TTL);
    png(bytes)
}

fn png(bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            // Long enough that a chat client is not re-fetching on every paste,
            // short enough that a renamed repository fixes itself the same day.
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        bytes,
    )
        .into_response()
}

// ---- oEmbed --------------------------------------------------------------

#[derive(serde::Serialize)]
struct OEmbed {
    version: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
    provider_name: String,
    provider_url: String,
    /// Discord draws this as the small line above the embed.
    author_name: String,
    author_url: String,
    title: String,
}

/// Answer the document linked from every page's `<head>`.
///
/// Only the `url` parameter is honoured, and only for a URL on this server:
/// this endpoint must not become a way to ask the hub to describe someone
/// else's site under the hub's name.
async fn oembed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let base = base_url(&state, &headers);
    let Some(target) = q.get("url") else {
        return (StatusCode::BAD_REQUEST, "url parameter required").into_response();
    };

    let Some(path) = target.strip_prefix(&base) else {
        return (StatusCode::NOT_FOUND, "not a URL on this server").into_response();
    };
    let path = if path.is_empty() { "/" } else { path };

    // Same rule as the card: nothing public to describe, nothing to answer.
    let Some(meta) = embed::describe(&state, path, &base).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let site = state.policy().site_name;

    // The author line is what a reader most wants above a repository card, so
    // it is the owner where there is one, and the site otherwise.
    let (author_name, author_url) = match path.trim_matches('/').split('/').next() {
        Some(owner) if !owner.is_empty() => (owner.to_string(), format!("{base}/{owner}")),
        _ => (site.clone(), base.clone()),
    };

    Json(OEmbed {
        version: "1.0",
        kind: "link",
        provider_name: site,
        provider_url: base.clone(),
        author_name,
        author_url,
        title: meta.title,
    })
    .into_response()
}
