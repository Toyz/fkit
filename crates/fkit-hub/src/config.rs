//! Configuration, resolved from three layers.
//!
//! ```text
//!   built-in defaults  <  fkit-hub.toml  <  environment  <  command-line flags
//! ```
//!
//! Later layers override earlier ones *per field*, so you can keep everything in
//! a file and still override one value for a single run without editing it.
//!
//! The file is the right home for anything with structure or comments; the
//! environment is the right home for secrets (a `DATABASE_URL` in a committed
//! file is a leaked credential); flags are the right home for one-off changes.
//! Nothing has a default that contains a secret.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Mirrors the TOML file. Every field optional: absent means "don't override".
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    server: ServerSection,
    #[serde(default)]
    database: DatabaseSection,
    #[serde(default)]
    storage: StorageSection,
    #[serde(default)]
    limits: LimitsSection,
    #[serde(default)]
    email: EmailSection,
    #[serde(default)]
    cache: CacheSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheSection {
    /// How much object data to hold in this process, in mebibytes.
    memory_mb: Option<usize>,
    /// How long an untouched entry may stay, in seconds.
    ttl_secs: Option<u64>,
    /// A Valkey or Redis URL to share a second tier through.
    ///
    /// Worth setting only when a cache miss is expensive: several hub
    /// processes that would each start cold, or object storage slower than a
    /// local disk. On one host with local storage it is slower than the disk
    /// it would sit in front of, which is why memory is always the first tier
    /// and this is only ever the second.
    redis_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerSection {
    listen: Option<String>,
    web_dir: Option<PathBuf>,
    /// Mark session cookies `Secure`. Set this when behind a TLS proxy.
    secure_cookies: Option<bool>,
    /// Take the client address from `X-Forwarded-For`. Only behind a proxy.
    trust_proxy: Option<bool>,
    /// Allow anyone to register. Turn off for a private instance.
    open_registration: Option<bool>,
    /// Require a signed-in user for *everything*, including repositories marked
    /// public. For an instance that should not be readable by the internet.
    require_auth: Option<bool>,
    /// Visibility applied to a new repository when the request does not say.
    default_repo_visibility: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseSection {
    /// Prefer the DATABASE_URL environment variable — a connection string in a
    /// file is a credential in a file.
    url: Option<String>,
    max_connections: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageSection {
    data_dir: Option<PathBuf>,
}

/// Outbound mail. The API key is deliberately absent: it belongs in the
/// environment, alongside `DATABASE_URL`, not in a file that gets committed.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmailSection {
    /// Sender address, on a domain verified with Resend.
    from: Option<String>,
    /// Base URL for links in outbound mail, e.g. `https://fkit.work`.
    public_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsSection {
    /// Largest file the browser will render inline, in bytes.
    max_inline_blob: Option<u64>,
    /// Reject a push larger than this, in bytes. 0 disables the limit.
    max_push_bytes: Option<u64>,
    /// Refuse an archive whose contents exceed this, in bytes. 0 disables it.
    /// Defaults to 1 GiB — see `Config::default`.
    max_archive_bytes: Option<u64>,
}

/// Mail settings supplied by the environment.
///
/// These differ from every other setting in this file: they are not seeds. A
/// deployment sets `FKIT_PUBLIC_URL` because that *is* where the server lives,
/// and it must not be quietly overridden by a value an administrator typed
/// into a form on a previous host. The same argument holds for the key — one
/// kept in a secret manager stays the single source of truth, and never has to
/// be pasted into a form or written to the database. Each is applied on every
/// read of the settings row, and the admin UI shows the field as fixed rather
/// than accepting an edit that would do nothing.
#[derive(Debug, Clone, Default)]
pub struct EnvEmail {
    pub api_key: Option<String>,
    pub from: Option<String>,
    pub public_url: Option<String>,
}

impl EnvEmail {
    pub fn is_empty(&self) -> bool {
        self.api_key.is_none() && self.from.is_none() && self.public_url.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: String,
    pub database_url: String,
    pub data_dir: PathBuf,
    pub web_dir: PathBuf,
    pub secure_cookies: bool,
    /// Whether `X-Forwarded-For` may be believed when identifying a client for
    /// rate limiting. Off by default: on a directly-exposed server, believing
    /// it lets anyone forge a new identity per request.
    pub trust_proxy: bool,
    pub open_registration: bool,
    pub require_auth: bool,
    pub default_repo_visibility: String,
    pub max_connections: u32,
    pub max_inline_blob: u64,
    pub max_push_bytes: u64,
    pub max_archive_bytes: u64,
    /// Seeds `email_from` on a fresh database; the admin UI wins thereafter.
    pub email_from: Option<String>,
    /// Seeds `public_url` on a fresh database; the admin UI wins thereafter.
    pub public_url: Option<String>,
    /// Mail settings taken from the environment, which are *not* seeds — see
    /// [`EnvEmail`].
    pub env_email: EnvEmail,
    /// How much object data each process holds, in bytes.
    pub cache_memory_bytes: usize,
    /// How long an untouched cache entry may stay.
    pub cache_ttl_secs: u64,
    /// A Valkey/Redis URL for a shared second tier, if one is wanted.
    pub cache_redis_url: Option<String>,
    /// Where the config came from, for the startup banner.
    pub source: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: "127.0.0.1:7500".into(),
            database_url: String::new(),
            data_dir: PathBuf::from("./fkit-hub-data"),
            web_dir: PathBuf::from("web/dist"),
            secure_cookies: false,
            trust_proxy: false,
            cache_memory_bytes: fkit_core::cache::DEFAULT_CAPACITY,
            cache_ttl_secs: fkit_core::cache::DEFAULT_TTL.as_secs(),
            cache_redis_url: None,
            open_registration: true,
            require_auth: false,
            default_repo_visibility: "private".into(),
            max_connections: 16,
            max_inline_blob: 2 * 1024 * 1024,
            max_push_bytes: 0,
            // 1 GiB. Unlike the other limits this defaults to *something*: an
            // archive is built on demand for whoever asks, so an unbounded
            // default lets a stranger point the server at its largest
            // repository repeatedly. The size is known from the tree before
            // any work starts, so exceeding it costs a rejection, not a
            // half-finished download.
            max_archive_bytes: 1024 * 1024 * 1024,
            email_from: None,
            public_url: None,
            env_email: EnvEmail::default(),
            source: None,
        }
    }
}

const DEFAULT_PATHS: &[&str] = &["fkit-hub.toml", "/etc/fkit/hub.toml"];

impl Config {
    pub fn load() -> Result<Config> {
        let args: Vec<String> = std::env::args().skip(1).collect();

        if args.iter().any(|a| a == "-h" || a == "--help") {
            print_help();
            std::process::exit(0);
        }
        if args.iter().any(|a| a == "--print-config-template") {
            print!("{TEMPLATE}");
            std::process::exit(0);
        }

        // An explicit --config must exist; the defaults are only tried if present.
        let explicit = flag_value(&args, "--config");
        let mut cfg = Config::default();

        let path = match &explicit {
            Some(p) => {
                let p = PathBuf::from(p);
                if !p.exists() {
                    bail!("config file not found: {}", p.display());
                }
                Some(p)
            }
            None => DEFAULT_PATHS.iter().map(PathBuf::from).find(|p| p.exists()),
        };

        if let Some(p) = path {
            cfg.apply_file(&p)?;
            cfg.source = Some(p);
        }

        cfg.apply_env();
        cfg.apply_args(&args)?;
        Ok(cfg)
    }

    fn apply_file(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        // `deny_unknown_fields` turns a typo into an error instead of a setting
        // that silently does nothing.
        let f: FileConfig = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;

        if let Some(v) = f.server.listen { self.listen = v }
        if let Some(v) = f.server.web_dir { self.web_dir = v }
        if let Some(v) = f.server.secure_cookies { self.secure_cookies = v }
        if let Some(v) = f.server.trust_proxy { self.trust_proxy = v }
        if let Some(v) = f.cache.memory_mb { self.cache_memory_bytes = v * 1024 * 1024 }
        if let Some(v) = f.cache.ttl_secs { self.cache_ttl_secs = v }
        if let Some(v) = f.cache.redis_url { self.cache_redis_url = Some(v) }
        if let Some(v) = f.server.open_registration { self.open_registration = v }
        if let Some(v) = f.server.require_auth { self.require_auth = v }
        if let Some(v) = f.server.default_repo_visibility {
            if !matches!(v.as_str(), "public" | "private") {
                bail!("default_repo_visibility must be \"public\" or \"private\", got {v:?}");
            }
            self.default_repo_visibility = v;
        }
        if let Some(v) = f.database.url { self.database_url = v }
        if let Some(v) = f.database.max_connections { self.max_connections = v }
        if let Some(v) = f.storage.data_dir { self.data_dir = v }
        if let Some(v) = f.limits.max_inline_blob { self.max_inline_blob = v }
        if let Some(v) = f.limits.max_push_bytes { self.max_push_bytes = v }
        if let Some(v) = f.limits.max_archive_bytes { self.max_archive_bytes = v }
        if let Some(v) = f.email.from { self.email_from = Some(v) }
        if let Some(v) = f.email.public_url { self.public_url = Some(v) }
        Ok(())
    }

    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("DATABASE_URL")
            && !v.is_empty()
        {
            self.database_url = v;
        }
        if let Ok(v) = std::env::var("FKIT_LISTEN") { self.listen = v }
        if let Ok(v) = std::env::var("FKIT_DATA") { self.data_dir = PathBuf::from(v) }
        if let Ok(v) = std::env::var("FKIT_WEB_DIR") { self.web_dir = PathBuf::from(v) }
        // Presence alone used to be enough here, which made
        // `FKIT_SECURE_COOKIES=0` — and, worse, the empty value a compose file
        // passes for an unset variable — turn Secure cookies ON. Over plain
        // http the browser then discards the session cookie and login appears
        // to silently fail. All three read the same way now.
        if let Some(v) = flag_env("FKIT_SECURE_COOKIES") { self.secure_cookies = v }
        if let Some(v) = flag_env("FKIT_TRUST_PROXY") { self.trust_proxy = v }
        // A cache URL is a piece of infrastructure wiring, so the environment
        // is where a container-run server will most naturally set it.
        if let Ok(v) = std::env::var("FKIT_CACHE_REDIS_URL")
            && !v.trim().is_empty()
        {
            self.cache_redis_url = Some(v);
        }
        if let Ok(v) = std::env::var("FKIT_CACHE_MEMORY_MB")
            && let Ok(mb) = v.trim().parse::<usize>()
        {
            self.cache_memory_bytes = mb * 1024 * 1024;
        }
        if let Some(v) = flag_env("FKIT_OPEN_REGISTRATION") { self.open_registration = v }
        if let Some(v) = flag_env("FKIT_REQUIRE_AUTH") { self.require_auth = v }
        // RESEND_API_KEY is what Resend's own documentation and every hosting
        // platform's integration call it; the prefixed name is accepted so the
        // hub's variables can be grouped, and wins if somebody sets both.
        self.env_email = EnvEmail {
            api_key: non_empty_env("FKIT_RESEND_API_KEY").or_else(|| non_empty_env("RESEND_API_KEY")),
            from: non_empty_env("FKIT_EMAIL_FROM"),
            public_url: non_empty_env("FKIT_PUBLIC_URL"),
        };
        // Also seed a fresh database, so a first boot with the environment set
        // lands the same values in the row the admin UI shows.
        if let Some(v) = &self.env_email.from { self.email_from = Some(v.clone()) }
        if let Some(v) = &self.env_email.public_url { self.public_url = Some(v.clone()) }
    }

    fn apply_args(&mut self, args: &[String]) -> Result<()> {
        let mut i = 0;
        while i < args.len() {
            let need = |i: usize| -> Result<String> {
                args.get(i + 1)
                    .cloned()
                    .with_context(|| format!("{} needs a value", args[i]))
            };
            match args[i].as_str() {
                "--config" => i += 2, // already handled
                "--listen" | "-l" => { self.listen = need(i)?; i += 2 }
                "--data" | "-d" => { self.data_dir = PathBuf::from(need(i)?); i += 2 }
                "--web" => { self.web_dir = PathBuf::from(need(i)?); i += 2 }
                "--database-url" => { self.database_url = need(i)?; i += 2 }
                "--secure-cookies" => { self.secure_cookies = true; i += 1 }
                "--trust-proxy" => { self.trust_proxy = true; i += 1 }
                "--closed-registration" => { self.open_registration = false; i += 1 }
                "--require-auth" => { self.require_auth = true; i += 1 }
                other => bail!("unknown option '{other}' (try --help)"),
            }
        }
        Ok(())
    }

    pub fn require_database_url(&self) -> Result<()> {
        if !self.database_url.is_empty() {
            return Ok(());
        }
        bail!(
            "no database configured.\n\
             \n\
             Set one of:\n\
             \x20  DATABASE_URL=postgres://user:pass@host/fkit_hub\n\
             \x20  --database-url postgres://...\n\
             \x20  [database] url = \"postgres://...\"   in fkit-hub.toml\n\
             \n\
             With Docker, `make up` generates .env and wires this for you.\n\
             Run `fkit-hub --print-config-template > fkit-hub.toml` for a starting file."
        )
    }
}

/// An environment variable set to the empty string is how a shell says
/// "unset" in practice — `RESEND_API_KEY=` in a .env file, a secret that
/// failed to inject — and must not read as a configured value.
/// A boolean environment variable. `None` when unset or empty, so a compose
/// file passing through a variable nobody set does not count as a decision.
fn flag_env(name: &str) -> Option<bool> {
    let v = std::env::var(name).ok()?;
    let v = v.trim().to_ascii_lowercase();
    if v.is_empty() {
        return None;
    }
    Some(!matches!(v.as_str(), "0" | "false" | "no" | "off"))
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn print_help() {
    println!(
        "fkit-hub — Postgres-backed forge for fkit\n\n\
         USAGE:\n    fkit-hub [OPTIONS]\n\n\
         OPTIONS:\n\
         \x20       --config FILE          config file (default: ./fkit-hub.toml, /etc/fkit/hub.toml)\n\
         \x20   -l, --listen ADDR          address to bind\n\
         \x20   -d, --data DIR             object stores directory\n\
         \x20       --web DIR              built frontend directory\n\
         \x20       --database-url URL     postgres connection string\n\
         \x20       --secure-cookies       mark session cookies Secure (use behind TLS)\n\
         \x20       --trust-proxy          take the client IP from X-Forwarded-For\n\
         \x20       --closed-registration  disable public sign-up\n\
         \x20       --require-auth         require a login for everything, even public repos\n\
         \x20       --print-config-template  write a commented fkit-hub.toml to stdout\n\n\
         CONFIG PRECEDENCE:\n\
         \x20   defaults < fkit-hub.toml < environment < flags\n\n\
         ENVIRONMENT:\n\
         \x20   DATABASE_URL (required), FKIT_LISTEN, FKIT_DATA, FKIT_WEB_DIR,\n\
         \x20   FKIT_SECURE_COOKIES, FKIT_TRUST_PROXY,\n\
         \x20   FKIT_OPEN_REGISTRATION, RUST_LOG\n\
         \x20   RESEND_API_KEY, FKIT_EMAIL_FROM, FKIT_PUBLIC_URL\n\n\
         MAIL:\n\
         \x20   RESEND_API_KEY overrides the key stored in the database, so a\n\
         \x20   key kept in a secret manager never has to be pasted into a form.\n"
    );
}

pub const TEMPLATE: &str = r#"# fkit-hub configuration.
#
# Precedence: defaults < this file < environment variables < command-line flags.
# Every key is optional; omit one to keep the built-in default.

[server]
# Bind address. Use 0.0.0.0 to accept connections from other machines.
listen = "127.0.0.1:7500"

# Where the built web UI lives.
web_dir = "web/dist"

# Mark session cookies `Secure`. Turn this ON behind a TLS proxy, and leave it
# OFF for plain-HTTP local use — a Secure cookie over http:// is discarded by
# the browser, which looks exactly like a login that silently fails.
secure_cookies = false

# Believe `X-Forwarded-For` when identifying a client for rate limiting. Turn
# this ON behind a reverse proxy, where every request otherwise appears to come
# from the proxy's address and one client's limit would be everyone's. Leave it
# OFF on a directly-exposed server: the header is client-supplied, so believing
# it there lets anyone mint a new identity per request and skip the limit.
trust_proxy = false

[cache]
# Object bytes held in this process. The store is content-addressed, so a
# cached object can never be stale — only unwanted.
memory_mb = 64
ttl_secs = 1800
# A shared second tier. Leave unset on a single host with local storage: a
# round trip to Redis costs more than reading the object off disk, so it only
# pays when a miss is expensive — several hub processes, or slow storage.
# redis_url = "redis://valkey:6379"

# Set false to run a private instance where only an admin can create accounts.
# The very first account is always allowed, so a fresh server is never locked
# out of itself.
open_registration = true

# Require a signed-in user for everything, including repositories marked public.
# Use this for an instance that should not be readable by the internet at all.
require_auth = false

# Visibility given to a new repository when the request does not specify one.
default_repo_visibility = "private"

[database]
# Prefer the DATABASE_URL environment variable. A connection string here is a
# credential in a file that tends to end up committed.
# url = "postgres://fkit:password@localhost:5432/fkit_hub"
max_connections = 16

[storage]
# Content-addressed object stores, one directory per repository.
data_dir = "./fkit-hub-data"

[email]
# Password resets go out through Resend. The API key is NOT set here — put it
# in the RESEND_API_KEY environment variable, or paste it into Settings →
# Email in the web UI, which stores it in the database. The environment wins.
#
# Sender address. The domain must be verified with Resend.
# from = "hub@fkit.work"

# Base URL used to build links in outbound mail. Without it the hub guesses
# from the request, which is wrong behind a proxy that rewrites Host.
# public_url = "https://fkit.work"

[limits]
# Largest file rendered inline in the browser (bytes).
max_inline_blob = 2097152

# Reject pushes larger than this (bytes). 0 disables the limit.
max_push_bytes = 0

# Refuse to build an archive of more than this many bytes of content (bytes).
# The size is known from the tree before any file is read, so an oversized
# request is refused immediately rather than part-way through a download, and
# the web UI stops offering the buttons rather than handing out a link that
# only errors. 0 disables the limit. Default 1 GiB.
max_archive_bytes = 1073741824
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_or_negative_flag_is_not_a_yes() {
        // SAFETY: single-threaded test, and the variable is removed after.
        unsafe {
            for (set, want) in [
                ("", None),
                ("   ", None),
                ("0", Some(false)),
                ("false", Some(false)),
                ("NO", Some(false)),
                ("off", Some(false)),
                ("1", Some(true)),
                ("true", Some(true)),
            ] {
                std::env::set_var("FKIT_TEST_FLAG", set);
                assert_eq!(flag_env("FKIT_TEST_FLAG"), want, "for {set:?}");
            }
            std::env::remove_var("FKIT_TEST_FLAG");
        }
        assert_eq!(flag_env("FKIT_TEST_FLAG"), None, "unset is not a decision");
    }

    #[test]
    fn the_template_parses_and_round_trips_to_the_defaults_it_documents() {
        let f: FileConfig = toml::from_str(TEMPLATE).expect("template must be valid TOML");
        assert_eq!(f.server.listen.as_deref(), Some("127.0.0.1:7500"));
        assert_eq!(f.server.secure_cookies, Some(false));
        assert_eq!(f.server.require_auth, Some(false));
        assert_eq!(f.server.default_repo_visibility.as_deref(), Some("private"));
        assert_eq!(f.storage.data_dir, Some(PathBuf::from("./fkit-hub-data")));
        assert_eq!(f.limits.max_inline_blob, Some(2 * 1024 * 1024));
        // The template is documentation, so it has to state the real default
        // rather than a round number somebody liked.
        assert_eq!(f.limits.max_archive_bytes, Some(Config::default().max_archive_bytes));
    }

    #[test]
    fn a_typo_is_an_error_not_a_silent_no_op() {
        let err = toml::from_str::<FileConfig>("[server]\nlistn = \"0.0.0.0:1\"\n").unwrap_err();
        assert!(err.to_string().contains("listn"), "got: {err}");
    }

    #[test]
    fn later_layers_override_earlier_ones_per_field() {
        let mut cfg = Config::default();
        assert_eq!(cfg.listen, "127.0.0.1:7500");

        let dir = std::env::temp_dir().join(format!("fkit-cfg-{}.toml", std::process::id()));
        std::fs::write(&dir, "[server]\nlisten = \"0.0.0.0:9000\"\n[storage]\ndata_dir = \"/srv/x\"\n").unwrap();
        cfg.apply_file(&dir).unwrap();
        assert_eq!(cfg.listen, "0.0.0.0:9000");
        assert_eq!(cfg.data_dir, PathBuf::from("/srv/x"));

        // A flag beats the file, and only for the field it names.
        cfg.apply_args(&["--listen".into(), "127.0.0.1:1234".into()]).unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:1234");
        assert_eq!(cfg.data_dir, PathBuf::from("/srv/x"), "unrelated field must survive");

        std::fs::remove_file(dir).ok();
    }

    #[test]
    fn an_invalid_default_visibility_is_rejected() {
        let mut cfg = Config::default();
        let f = std::env::temp_dir().join(format!("fkit-vis-{}.toml", std::process::id()));
        std::fs::write(&f, "[server]\ndefault_repo_visibility = \"secret\"\n").unwrap();
        let err = cfg.apply_file(&f).unwrap_err().to_string();
        assert!(err.contains("default_repo_visibility"), "got: {err}");
        std::fs::remove_file(f).ok();
    }

    #[test]
    fn a_missing_database_url_explains_itself() {
        let cfg = Config::default();
        let err = cfg.require_database_url().unwrap_err().to_string();
        assert!(err.contains("DATABASE_URL"));
        assert!(err.contains("fkit-hub.toml"));
    }
}
