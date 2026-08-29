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
mod embed;
mod email;
mod error;
mod models;
mod perms;
mod ratelimit;
mod rules;
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
use tower_http::compression::CompressionLayer;
use tower::Layer;
use axum::http::HeaderMap;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

// fkit spends most of its time allocating: the chunker cuts a stream into
// millions of small buffers, hashes each, and drops nearly all of them again.
// That is the workload general-purpose allocators handle worst and mimalloc
// handles best, and it is thread-local, so the win grows with core count
// rather than contending.
//
// Set here rather than in fkit-core: a library that installs a global
// allocator makes the choice for every binary that ever links it, which is not
// a library's decision to make.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

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

    let (object_cache, cache_backend) = build_object_cache(&cfg);

    let state = AppState {
        db,
        data_dir: cfg.data_dir.clone(),
        secure_cookies: cfg.secure_cookies,
        web_dir: cfg.web_dir.clone(),
        settings,
        max_archive_bytes: cfg.max_archive_bytes,
        limiter: std::sync::Arc::new(ratelimit::MemoryLimiter::default()),
        trust_proxy: cfg.trust_proxy,
        object_cache,
        cache_backend,
    };

    let api = Router::new()
        .merge(routes::session::routes())
        .merge(routes::repos::routes())
        .merge(routes::browse::routes())
        .merge(routes::merges::routes())
        .merge(routes::issues::routes())
        .merge(routes::admin::routes());

    // Read policy for the startup banner before the router takes ownership.
    let policy = state.policy();

    let app = Router::new()
        .nest("/api", api)
        // Top level, not under /api: the base URL is handed to the Go
        // toolchain in a meta tag, and it fetches exactly what it is given.
        .merge(routes::gomod::routes())
        // Top level for the same reason: these URLs are published in the page
        // head and fetched by crawlers exactly as written.
        .merge(routes::social::routes())
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
        .fallback(shell)
        // Before routing, because `?go-get=1` can arrive on any path under a
        // repository and every one of them must answer the same thing.
        .layer(axum::middleware::from_fn(go_get))
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
    tracing::info!(
        "  clients  {}",
        if cfg.trust_proxy { "X-Forwarded-For (proxy trusted)" } else { "peer address" }
    );
    // Secure cookies say a proxy is in front; counting the peer address says
    // there isn't. Together they put the whole instance in one rate-limit
    // bucket, and the symptom never points at the cause.
    if cfg.secure_cookies && !cfg.trust_proxy {
        tracing::warn!(
            "secure_cookies is on but trust_proxy is off: every request will be \
             counted against the proxy's address, so the rate limits apply to the \
             whole instance at once. Set trust_proxy = true in hub.toml if a proxy \
             is in front of this."
        );
    }
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

    // With ConnectInfo, so rate limiting can tell one client from another. On
    // a directly-exposed server this peer address is the only trustworthy
    // identity a request has.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
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
        let p = req.uri().path().to_string();
        return index_html(&state, &p, req.headers()).await;
    }

    let (mut parts, _body) = req.into_parts();
    match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(ws) => sync::handler(ws, State(state), Path(path)).await,
        Err(rejection) => rejection.into_response(),
    }
}

/// Intercept Go's module discovery request.
///
/// `go get` fetches the import path with `?go-get=1` and reads a meta tag out
/// of whatever HTML comes back. The SPA shell would be served here otherwise,
/// and Go would find no tag and fall back to guessing a VCS.
async fn go_get(req: Request, next: axum::middleware::Next) -> Response {
    let wants = req.uri().query().is_some_and(|q| {
        q.split('&').any(|kv| kv == "go-get=1")
    });
    if !wants {
        return next.run(req).await;
    }

    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost")
        .to_string();

    // A proxy that terminated TLS says so; otherwise loopback is plain and
    // anything else is assumed to be behind one. Guessing https for a local
    // server would produce a tag pointing at a port that is not listening.
    let scheme = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let local = host.starts_with("127.0.0.1")
                || host.starts_with("localhost")
                || host.starts_with("[::1]");
            if local { "http".into() } else { "https".into() }
        });

    match routes::gomod::go_import_page(&host, &scheme, req.uri().path()) {
        Some(html) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            html,
        )
            .into_response(),
        None => next.run(req).await,
    }
}

/// The SPA shell.
///
/// Explicitly uncacheable. Asset filenames carry a content hash so they can be
/// cached forever, but the shell is what *points* at them — let a browser
/// heuristically cache it and users keep running an old bundle after every
/// deploy, with no way to tell.
async fn index_html(state: &AppState, path: &str, headers: &HeaderMap) -> Response {
    match tokio::fs::read_to_string(state.web_dir.join("index.html")).await {
        Ok(html) => {
            // Anything not publicly readable gets no preview at all, which is
            // the same nothing a missing page gets. The app still renders it
            // normally for whoever is allowed to see it — this only changes
            // what a crawler is told.
            let base = routes::social::base_url(state, headers);
            let html = match embed::describe(state, path, &base).await {
                Some(meta) => embed::inject(&html, &meta, &base, &state.policy().site_name),
                None => embed::inject_blank(&html),
            };
            (
                [
                    (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    (header::CACHE_CONTROL, "no-store, must-revalidate"),
                ],
                html,
            )
                .into_response()
        }
        Err(_) => (
            StatusCode::NOT_FOUND,
            "web UI not built — run `npm install && npm run build` in web/",
        )
            .into_response(),
    }
}

/// Serve a static file if there is one, otherwise the app shell.
///
/// Client-side routes like `/travis/fkit/blob/main/src/lib.rs` have to reach
/// the SPA rather than 404, and on the way out the shell picks up the metadata
/// for whichever route was asked for — see `embed`. That is the only reason
/// this is a handler and not a `ServeDir`: a static file cannot describe
/// itself to a crawler.
async fn shell(State(state): State<AppState>, req: Request) -> Response {
    use tower::ServiceExt;

    let path = req.uri().path().to_string();
    let headers = req.headers().clone();

    // A real file wins. `ServeDir` refuses paths that climb out of the
    // directory, so this does not need its own traversal check.
    //
    // Directory indexes are off: with them on, `/` is answered with the raw
    // index.html and never reaches the metadata below, so the site's own root
    // was the one page with no preview.
    let served = ServeDir::new(&state.web_dir)
        .append_index_html_on_directories(false)
        .oneshot(req)
        .await;
    if let Ok(res) = served
        && res.status() != StatusCode::NOT_FOUND
    {
        return res.map(axum::body::Body::new);
    }

    index_html(&state, &path, &headers).await
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}

/// Build the object cache this server will use.
///
/// Memory is always the first tier and is never optional: reading a packed
/// object from a local disk costs single-digit microseconds, so a cache that
/// answered over a network *instead* would be slower than no cache at all.
/// A shared tier is added behind it when one is configured, where it saves the
/// miss rather than the read.
///
/// A shared tier that will not connect is reported and skipped rather than
/// fatal. The server is completely functional without it — it is an
/// optimisation, and refusing to start over one would trade the whole service
/// for a faster one.
/// The cache, and a description of where it actually holds things.
///
/// The description is carried rather than inferred: what a caller can see is
/// an `Arc<dyn ObjectCache>`, and by then the configuration that decided
/// between one tier and two is gone.
fn build_object_cache(
    cfg: &config::Config,
) -> (std::sync::Arc<dyn fkit_core::cache::ObjectCache>, String) {
    use fkit_core::cache::{MemoryCache, ObjectCache};
    use std::sync::Arc;
    use std::time::Duration;

    let ttl = Duration::from_secs(cfg.cache_ttl_secs);
    let near: Arc<dyn ObjectCache> = Arc::new(MemoryCache::new(cfg.cache_memory_bytes, ttl));

    let Some(url) = cfg.cache_redis_url.as_deref() else {
        return (near, "memory".into());
    };

    // The URL can carry a password; only the host is worth showing.
    let where_far = url.rsplit('@').next().unwrap_or(url).to_string();

    #[cfg(feature = "redis-cache")]
    {
        match fkit_core::cache::RedisCache::connect(url, "fkit:obj:", ttl) {
            Ok(far) => {
                tracing::info!("object cache: memory + shared at {url}");
                (
                    Arc::new(fkit_core::cache::Tiered::new(near, Arc::new(far))),
                    format!("memory, then {where_far}"),
                )
            }
            Err(e) => {
                tracing::warn!("shared object cache at {url} unavailable ({e}); memory only");
                (near, format!("memory ({where_far} is unreachable)"))
            }
        }
    }

    #[cfg(not(feature = "redis-cache"))]
    {
        tracing::warn!(
            "a shared object cache is configured ({url}) but this binary was built \
             without the `redis-cache` feature; using memory only"
        );
        (near, format!("memory (built without support for {where_far})"))
    }
}
