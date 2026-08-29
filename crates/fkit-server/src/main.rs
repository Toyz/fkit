//! `fkitd` — the standalone fkit sync daemon.
//!
//! Thread per connection, refs on disk, one optional shared token. Everything
//! reusable lives in the `fkit_server` library beside this file; this binary is
//! argument parsing, the accept loop, and authentication.

use anyhow::{bail, Context, Result};
use fkit_core::session::{read_hello, send_error, send_welcome, serve_session};
use fkit_core::ws::WebSocket;
use fkit_server::{check_exposure, open_or_create, validate_name, Config, DiskHost};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

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

fn main() {
    if let Err(e) = run() {
        eprintln!("fkitd: {e:#}");
        std::process::exit(1);
    }
}

// The daemon's options. A doc comment here would become the help text, and an
// implementation note is not what someone typing --help asked for.
//
// `-d`/`-l` and the long forms are what the hand-rolled parser accepted, so an
// existing unit file keeps working.
#[derive(clap::Parser, Debug)]
#[command(
    name = "fkitd",
    version,
    about = "fkit sync daemon",
    after_help = "ENVIRONMENT:\n  \
                  FKIT_TOKEN          if set, clients must present this token\n\n\
                  For user accounts, per-repo permissions and a web UI, run fkit-hub instead."
)]
struct Args {
    /// Address to listen on.
    #[arg(short, long, default_value = "127.0.0.1:7420")]
    listen: String,

    /// Where repositories live.
    #[arg(short, long, default_value = "./fkit-data")]
    data: std::path::PathBuf,

    /// Reject pushes to repositories that do not exist.
    #[arg(long)]
    no_create: bool,

    /// Allow listening off-loopback with no token.
    #[arg(long)]
    insecure_no_auth: bool,
}

fn run() -> Result<()> {
    use clap::Parser;
    let args = Args::parse();

    let cfg = Config {
        data_dir: args.data,
        listen: args.listen,
        // Deliberately not a flag. A token on the command line is a token in
        // the process list, and in the shell history of whoever started it.
        token: std::env::var("FKIT_TOKEN").ok().filter(|t| !t.is_empty()),
        allow_create: !args.no_create,
        insecure_no_auth: args.insecure_no_auth,
    };

    // Fail before binding, not after — a warning in a container log is not a
    // safety mechanism.
    check_exposure(&cfg)?;

    std::fs::create_dir_all(&cfg.data_dir)?;
    let listener = TcpListener::bind(&cfg.listen).with_context(|| format!("binding {}", cfg.listen))?;

    println!("fkitd listening on ws://{}", cfg.listen);
    println!("  data      {}", cfg.data_dir.canonicalize()?.display());
    println!(
        "  auth      {}",
        if cfg.token.is_some() { "token required" } else { "OPEN — anyone can read and write" }
    );
    if cfg.token.is_none() && !fkit_server::is_loopback_addr(&cfg.listen) {
        println!("  WARNING   running open on a public address (--insecure-no-auth)");
    }

    let cfg = Arc::new(cfg);
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => { eprintln!("accept failed: {e}"); continue }
        };
        let cfg = cfg.clone();
        std::thread::spawn(move || {
            let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
            if let Err(e) = handle(stream, &cfg) {
                eprintln!("[{peer}] {e:#}");
            }
        });
    }
    Ok(())
}

fn handle(stream: TcpStream, cfg: &Config) -> Result<()> {
    stream.set_nodelay(true)?;
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let (mut ws, path) = WebSocket::accept(stream)?;

    let (hello_repo, token) = read_hello(&mut ws)?;

    if let Some(expected) = &cfg.token
        && &token != expected
    {
        send_error(&mut ws, "invalid token")?;
        bail!("[{peer}] rejected: bad token");
    }

    let name = {
        let from_path = path.trim_start_matches('/').trim_end_matches('/');
        if from_path.is_empty() { hello_repo } else { from_path.to_string() }
    };
    validate_name(&name)?;

    let repo = match open_or_create(&cfg.data_dir.join(&name), cfg.allow_create) {
        Ok(r) => r,
        Err(e) => {
            send_error(&mut ws, e.to_string())?;
            return Err(e);
        }
    };

    let host = DiskHost { repo, name: name.clone(), peer: peer.clone(), writable: true };
    let refs = host.refs()?;
    println!("[{peer}] connected to '{name}' ({} branch(es))", refs.len());
    send_welcome(&mut ws, refs)?;

    serve_session(&mut ws, &host)?;
    Ok(())
}

use fkit_core::session::RepoHost;
