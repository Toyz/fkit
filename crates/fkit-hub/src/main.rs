//! `fkit-hub` — a Postgres-backed forge for fkit repositories.
//!
//! Two surfaces on one port:
//!
//! * a JSON API under `/api` for the web UI
//! * a WebSocket sync endpoint at `/{owner}/{repo}` for the `fkit` CLI
//!
//! They share a URL space deliberately: `ws://hub/travis/fkit` for the client
//! and `https://hub/travis/fkit` in a browser are the same repository, told
//! apart by the `Upgrade` header rather than by a different path.

mod auth;
mod config;
mod content;
mod email;
mod error;
mod models;
mod perms;
mod routes;
mod settings;
mod state;
mod sync;

use anyhow::{Context, Result};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, Path, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use config::Config;
use sqlx::postgres::PgPoolOptions;
use state::AppState;
use std::path::PathBuf;
use tower_http::compression::CompressionLayer;
use tower::Layer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "fkit_hub=info,tower_http=warn".into()),
        )
        .init();

    let cfg = Config::load()?;

    cfg.require_database_url()?;

    let db = PgPoolOptions::new()
        .max_connections(cfg.max_connections)
        .connect(&cfg.database_url)
        .await
        .with_context(|| "connecting to Postgres (is DATABASE_URL correct?)")?;

    sqlx::migrate!("./migrations")
        .run(&db)
        .await
        .context("running database migrations")?;

    std::fs::create_dir_all(cfg.data_dir.join("repos"))?;

    // The config file seeds instance policy on a fresh database and is the
    // default thereafter; the row an administrator edits from the web wins.
    let settings = settings::Settings::load(
        &db,
        settings::Instance {
            site_name: "fkit hub".into(),
            open_registration: cfg.open_registration,
            require_auth: cfg.require_auth,
            default_repo_visibility: cfg.default_repo_visibility.clone(),
            email_from: cfg.email_from.clone().unwrap_or_default(),
            public_url: cfg.public_url.clone().unwrap_or_default(),
            ..Default::default()
        },
        cfg.env_email.clone(),
    )
    .await
    .context("loading instance settings")?;

    let state = AppState {
        db,
        data_dir: cfg.data_dir.clone(),
        secure_cookies: cfg.secure_cookies,
        web_dir: cfg.web_dir.clone(),
        settings,
    };

    let api = Router::new()
        .merge(routes::session::routes())
        .merge(routes::repos::routes())
        .merge(routes::browse::routes())
        .merge(routes::merges::routes())
        .merge(routes::admin::routes());

    // Read policy for the startup banner before the router takes ownership.
    let policy = state.policy();

    let app = Router::new()
        .nest("/api", api)
        .route("/_health", get(health))
        // Built assets MUST be mounted before the repo route. `/assets/app.js`
        // has exactly two segments, so it also matches `/{owner}/{repo}` — and a
        // fallback that serves index.html would happily return HTML with a
        // JavaScript content type. A static prefix wins over a parameterised
        // segment, so this ordering resolves it. ("assets" is also a reserved
        // username, so no real repository can live here.)
        // Asset names contain a content hash, so a change is a new URL and the
        // old one can be cached indefinitely.
        .nest_service(
            "/assets",
            tower_http::set_header::SetResponseHeaderLayer::overriding(
                header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("public, max-age=31536000, immutable"),
            )
            .layer(ServeDir::new(cfg.web_dir.join("assets"))),
        )
        // Same path serves the web page and the sync socket; the Upgrade header
        // decides which.
        .route("/{owner}/{repo}", get(repo_entrypoint))
        .fallback_service(spa(&cfg.web_dir))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.listen)
        .await
        .with_context(|| format!("binding {}", cfg.listen))?;

    tracing::info!("fkit-hub listening on http://{}", cfg.listen);
    match &cfg.source {
        Some(p) => tracing::info!("  config   {}", p.display()),
        None => tracing::info!("  config   defaults (no fkit-hub.toml found)"),
    }
    tracing::info!("  data     {}", cfg.data_dir.display());
    tracing::info!("  web      {}", cfg.web_dir.display());
    tracing::info!("  signup   {}", if policy.open_registration { "open" } else { "closed" });
    tracing::info!(
        "  access   {}",
        if policy.require_auth { "login required for everything" } else { "public repos readable anonymously" }
    );
    tracing::info!("  (both editable at /admin by an administrator)");
    tracing::info!("  cookies  {}", if cfg.secure_cookies { "Secure" } else { "not Secure (dev)" });
    // Mail is silent when it is missing — the sign-in page simply omits the
    // reset link — so the one place it can be noticed is here.
    match (policy.email_configured(), cfg.env_email.is_empty()) {
        (true, false) => tracing::info!("  email    {} (from the environment)", policy.email_from),
        (true, true) => tracing::info!("  email    {}", policy.email_from),
        (false, _) => tracing::info!("  email    not configured — no password resets"),
    }
    if !cfg.secure_cookies && !cfg.listen.starts_with("127.") {
        tracing::warn!(
            "serving a public address without secure_cookies — session cookies will \
             travel in clear text. Set secure_cookies once TLS is in front."
        );
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// `/{owner}/{repo}` serves two things.
///
/// A browser gets the SPA shell; the `fkit` CLI gets a sync socket. They are told
/// apart by the `Upgrade` header, which is why both can share one URL and one
/// port — `https://hub/travis/fkit` and `ws://hub/travis/fkit` name the same
/// repository, as they should.
///
/// axum 0.8 has no `Option<WebSocketUpgrade>` extractor, so the header check and
/// the extraction are done explicitly.
async fn repo_entrypoint(
    State(state): State<AppState>,
    Path(path): Path<(String, String)>,
    req: Request,
) -> Response {
    let is_upgrade = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));

    if !is_upgrade {
        return index_html(&state).await;
    }

    let (mut parts, _body) = req.into_parts();
    match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(ws) => sync::handler(ws, State(state), Path(path)).await,
        Err(rejection) => rejection.into_response(),
    }
}

/// The SPA shell.
///
/// Explicitly uncacheable. Asset filenames carry a content hash so they can be
/// cached forever, but the shell is what *points* at them — let a browser
/// heuristically cache it and users keep running an old bundle after every
/// deploy, with no way to tell.
async fn index_html(state: &AppState) -> Response {
    match tokio::fs::read_to_string(state.web_dir.join("index.html")).await {
        Ok(html) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store, must-revalidate"),
            ],
            html,
        )
            .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            "web UI not built — run `npm install && npm run build` in web/",
        )
            .into_response(),
    }
}

/// Serve built assets, falling back to `index.html` so client-side routes like
/// `/travis/fkit/blob/main/src/lib.rs` reach the SPA instead of 404ing.
fn spa(dir: &PathBuf) -> ServeDir<tower_http::services::fs::ServeFile> {
    ServeDir::new(dir).fallback(tower_http::services::ServeFile::new(dir.join("index.html")))
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
