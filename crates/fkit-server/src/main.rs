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

fn main() {
    if let Err(e) = run() {
        eprintln!("fkitd: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cfg = Config {
        data_dir: std::path::PathBuf::from("./fkit-data"),
        listen: "127.0.0.1:7420".to_string(),
        token: std::env::var("FKIT_TOKEN").ok().filter(|t| !t.is_empty()),
        allow_create: true,
        insecure_no_auth: false,
    };

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data" | "-d" => { cfg.data_dir = std::path::PathBuf::from(&args[i + 1]); i += 2 }
            "--listen" | "-l" => { cfg.listen = args[i + 1].clone(); i += 2 }
            "--no-create" => { cfg.allow_create = false; i += 1 }
            "--insecure-no-auth" => { cfg.insecure_no_auth = true; i += 1 }
            "-h" | "--help" => {
                println!(
                    "fkitd — fkit sync daemon\n\n\
                     USAGE:\n    fkitd [--listen ADDR] [--data DIR] [--no-create]\n\n\
                     OPTIONS:\n\
                     \x20   -l, --listen ADDR   default 127.0.0.1:7420\n\
                     \x20   -d, --data DIR      where repositories live (default ./fkit-data)\n\
                     \x20   --no-create         reject pushes to repositories that do not exist\n\
                     \x20   --insecure-no-auth  allow listening off-loopback with no token\n\n\
                     ENVIRONMENT:\n\
                     \x20   FKIT_TOKEN          if set, clients must present this token\n\n\
                     For user accounts, per-repo permissions and a web UI, run fkit-hub instead.\n"
                );
                return Ok(());
            }
            other => bail!("unknown option '{other}' (try --help)"),
        }
    }

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
