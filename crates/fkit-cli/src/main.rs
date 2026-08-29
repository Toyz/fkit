//! The `fkit` command-line client.
//!
//! Argument parsing is hand-rolled rather than using clap. For a project whose
//! point is understanding what is going on, one less layer of magic is worth a
//! few dozen lines of matching.

use anyhow::{bail, Context, Result};
use fkit_core::repo::CommitAs;
use fkit_core::checkout::checkout_tree;
use fkit_core::diff as linediff;
use fkit_core::fsck::fsck;
use fkit_core::hash::Hash;
use fkit_core::ingest::{read_entries, read_file};
use fkit_core::object::{EntryKind, Object};
use fkit_core::proto::{fetch_closure, is_ancestor, recv, send, serve_wants, verify_closure, Msg};
use fkit_core::repo::{diff_trees, Change, Head, Repo};
use fkit_core::ws::WebSocket;
use std::io::Write;
use std::path::PathBuf;

const USAGE: &str = "\
fkit — a content-addressed version control system

USAGE:
    fkit <command> [args]

REPOSITORY
    init [path]              create a repository
    config [--global] <key> [value]
                             read or set a config value (author, remote, token)
    config --list            show effective values and where each came from

SNAPSHOTS
    status                   show what changed since the last commit
    commit -m <message>      snapshot the working tree
    log [-n <count>]         show commit history
    diff [<a>] [<b>]         show what changed (defaults: HEAD vs working tree)
        --stat               list changed paths only, no line content
        -U <n>               lines of context (default 3)

BRANCHES
    branch                   list branches
    branch <name>            create a branch at HEAD
    branch -d <name>         delete a branch
    tag                      list tags
    tag <name> [<commit>]    tag a commit (HEAD by default); tags do not move
    tag -f <name> [<commit>] move a tag anyway
    tag -d <name>            delete a tag
    switch <name>            move HEAD to a branch and update the working tree
    merge <branch>           merge another branch into the current one
        -m <message>         message for the merge commit
    checkout <commit>        move HEAD to a specific commit (detached)
        --force              discard uncommitted changes

SUBMODULES
    submodule                list mounted submodules and their state
    submodule add <url> <path> [--branch <b>]
                             mount another repository at <path>
    submodule update [<path>] move the pin to the remote's current tip
    submodule fetch [<path>]  retrieve content for a pin this store lacks
    submodule rm <path>      unmount; the next commit will not carry it
    submodule set-remote <path> <url>
                             fetch this submodule from somewhere else

REMOTES
    remote [<url>]           show or set the remote (ws://host:7420/repo)
    clone <url> [<dir>]      copy a remote repository
        --no-checkout        fetch objects and refs, write no files
    push [<branch>] [--force] [--no-tags]
                             send commits and tags to the remote
    pull [<branch>]          fetch commits from the remote and update

INSPECTION
    show <hash>              print an object's structure
    tree [<commit>]          list the files in a commit
    cat <path> [<commit>]    print a file's contents
    merkle <hash>            visualise an object's Merkle tree
    prove <path> [<commit>]  emit a proof that a path is in a commit
        -o <file>            write to a file instead of stdout
    verify <file> --root <h> check a proof against a commit hash you trust
    pack                     fold loose objects into packed segments
    gc                       delete objects no branch can reach
        --dry-run            report what would go, change nothing
        --prune-all          ignore the age guard (nothing else may be writing)
    verify-tree <dir>        compare HEAD's tree against a directory, byte for byte
    fsck                     verify every object and report unreachable ones
    stats                    storage and deduplication statistics
";

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
    // Rust ignores SIGPIPE by default, which turns `fkit log | head` into a
    // panic on a closed pipe rather than a clean exit. Restoring the default
    // handler makes fkit behave like every other command-line tool.
    #[cfg(unix)]
    unsafe {
        unsafe extern "C" {
            fn signal(sig: i32, handler: usize) -> usize;
        }
        const SIGPIPE: i32 = 13;
        const SIG_DFL: usize = 0;
        signal(SIGPIPE, SIG_DFL);
    }

    if let Err(e) = run() {
        eprintln!("fkit: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(|s| s.as_str()) else {
        print!("{USAGE}");
        return Ok(());
    };
    let rest = &args[1..];

    match cmd {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        "init" => cmd_init(rest),
        "config" => cmd_config(rest),
        "status" => cmd_status(rest),
        "commit" => cmd_commit(rest),
        "log" => cmd_log(rest),
        "diff" => cmd_diff(rest),
        "branch" => cmd_branch(rest),
        "tag" => cmd_tag(rest),
        "switch" => cmd_switch(rest),
        "merge" => cmd_merge(rest),
        "checkout" => cmd_checkout(rest),
        "show" => cmd_show(rest),
        "tree" => cmd_tree(rest),
        "cat" => cmd_cat(rest),
        "merkle" => cmd_merkle(rest),
        "remote" => cmd_remote(rest),
        "clone" => cmd_clone(rest),
        "push" => cmd_push(rest),
        "pull" => cmd_pull(rest),
        "prove" => cmd_prove(rest),
        "verify" => cmd_verify(rest),
        "pack" => cmd_pack(),
        "gc" => cmd_gc(rest),
        "verify-tree" => cmd_verify_tree(rest),
        "fsck" => cmd_fsck(),
        "stats" => cmd_stats(),
        "submodule" => cmd_submodule(rest),
        other => bail!("unknown command '{other}'\n\n{USAGE}"),
    }
}

fn here() -> Result<Repo> {
    Repo::discover(&std::env::current_dir()?)
}

/// Resolve a user-typed revision: `HEAD`, a branch name, or a hash prefix.
fn resolve(repo: &Repo, spec: &str) -> Result<Hash> {
    if spec == "HEAD" {
        return repo
            .head_commit()?
            .context("HEAD does not point at a commit yet (no commits in this repo)");
    }
    if let Some(h) = repo.read_ref(spec)? {
        return Ok(h);
    }
    // Tags resolve after branches: a branch and a tag may share a name, and
    // the branch is the one you are more likely to be working on.
    if let Some(h) = repo.read_tag(spec.strip_prefix(Repo::TAG_PREFIX).unwrap_or(spec))? {
        return Ok(h);
    }
    repo.store.resolve_prefix(spec)
}

fn tree_of(repo: &Repo, commit: Hash) -> Result<Hash> {
    match repo.store.get(commit)? {
        Object::Commit(c) => Ok(c.tree),
        Object::Tree { .. } => Ok(commit), // allow naming a tree directly
        other => bail!("{} is a {}, not a commit", commit.short(), other.kind().name()),
    }
}

// ---- commands -----------------------------------------------------------

fn cmd_init(args: &[String]) -> Result<()> {
    let path = args.first().map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    std::fs::create_dir_all(&path)?;
    let repo = Repo::init(&path)?;
    println!("initialised empty fkit repository in {}", repo.root.join(".fkit").display());
    println!("\nnext steps:");
    println!("  fkit config --global author.name \"Your Name\"");
    println!("  fkit config --global author.email you@example.com");
    println!("  fkit commit -m \"first commit\"");
    Ok(())
}

fn cmd_config(args: &[String]) -> Result<()> {
    use fkit_core::config;

    let global = args.iter().any(|a| a == "--global" || a == "-g");
    let list = args.iter().any(|a| a == "--list" || a == "-l");
    let rest: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();

    if list {
        // Show where each value actually comes from — the whole point of having
        // layers is being able to see which one won.
        println!("user  {}", config::global_path().map(|p| p.display().to_string())
            .unwrap_or_else(|| "(no home directory)".into()));
        for (k, v) in config::global_all() {
            println!("  {k} = {}", redact(&k, &v));
        }
        match here() {
            Ok(repo) => {
                println!("\nrepo  {}", repo.root.join(".fkit/config").display());
                for key in ["author.name", "author.email", "author", "remote", "token"]
                    .iter()
                    .copied()
                {
                    if let Some(v) = repo.config_get_local(key) {
                        println!("  {key} = {}", redact(key, &v));
                    }
                }
                println!("\neffective");
                println!("  author = {}", repo.author());
                if let Some(r) = repo.config_get("remote") {
                    println!("  remote = {r}");
                }
            }
            Err(_) => println!("\n(not inside a repository)"),
        }
        return Ok(());
    }

    // Setting a global value must work outside a repository — that is most of
    // the reason to have one.
    if global {
        return match rest.as_slice() {
            [key] => match config::global_get(key) {
                Some(v) => {
                    println!("{v}");
                    Ok(())
                }
                None => bail!("'{key}' is not set in the user config"),
            },
            [key, value] => {
                let path = config::global_set(key, value)?;
                println!("{key} = {value}");
                println!("  written to {}", path.display());
                Ok(())
            }
            _ => bail!("usage: fkit config --global <key> [value]"),
        };
    }

    let repo = here()?;
    match rest.as_slice() {
        [key] => match repo.config_get(key) {
            Some(v) => println!("{v}"),
            None => bail!("'{key}' is not set (try: fkit config --global {key} <value>)"),
        },
        [key, value] => {
            repo.config_set(key, value)?;
            println!("{key} = {value}");
        }
        _ => bail!("usage: fkit config [--global] <key> [value]"),
    }
    Ok(())
}

/// Never print a stored credential in full.
fn redact(key: &str, value: &str) -> String {
    if key.contains("token") || key.contains("password") {
        let head: String = value.chars().take(13).collect();
        format!("{head}… ({} chars)", value.len())
    } else {
        value.to_string()
    }
}

fn cmd_status(args: &[String]) -> Result<()> {
    // `status` takes no options. Accepting one silently is worse than
    // rejecting it: a script that asked for `--short` and got the long form
    // anyway parses the wrong thing and looks like it worked.
    if let Some(a) = args.first() {
        bail!("status takes no arguments, got '{a}'");
    }
    let repo = here()?;
    match repo.head()? {
        Head::Branch(b) => {
            let tip = repo.read_ref(&b)?;
            match tip {
                Some(h) => println!("on branch {b} ({})", h.short()),
                None => println!("on branch {b} (no commits yet)"),
            }
        }
        Head::Detached(h) => println!("HEAD detached at {}", h.short()),
    }

    let snap = repo.snapshot()?;
    let changes = diff_trees(&repo.view_with(&snap), repo.head_tree()?, Some(snap.hash))?;

    if changes.is_empty() {
        println!("\nworking tree clean");
        return Ok(());
    }

    println!("\nchanges not yet committed:");
    for c in &changes {
        let detail = match c {
            Change::Added { size, .. } => format!("  ({})", human(*size)),
            Change::Removed { size, .. } => format!("  (was {})", human(*size)),
            Change::Modified { old_size, new_size, .. } => {
                format!("  ({} -> {})", human(*old_size), human(*new_size))
            }
            Change::TypeChanged { .. } => String::from("  (type changed)"),
        };
        println!("  {} {}{}", c.sigil(), c.path(), detail);
    }
    println!("\n{} path(s) changed", changes.len());
    Ok(())
}

/// Refuse to record a commit as nobody.
///
/// This used to be a warning, and a warning is the wrong shape for it: the
/// author is baked into the commit and into its hash, so by the time anyone
/// reads the note the history already says `unknown` or whatever $USER
/// happened to be on the machine. It cannot be corrected afterwards without
/// rewriting every commit that followed.
///
/// The legacy single-key `author` still counts, so repositories configured
/// before name and email were split apart keep working.
fn require_author(repo: &Repo) -> Result<()> {
    let set = |k: &str| repo.config_get(k).is_some_and(|v| !v.trim().is_empty());
    let env_set = std::env::var("FKIT_AUTHOR").is_ok_and(|v| !v.trim().is_empty());

    if (set("author.name") && set("author.email")) || set("author") || env_set {
        return Ok(());
    }

    let missing = if set("author.name") {
        "author.email is not set"
    } else if set("author.email") {
        "author.name is not set"
    } else {
        "no author is configured"
    };

    bail!(
        "{missing} — a commit records who made it, and that cannot be fixed later.\n\n\
         \x20 fkit config --global author.name \"Your Name\"\n\
         \x20 fkit config --global author.email you@example.com\n\n\
         Drop --global to set it for this repository only, or pass\n\
         \x20 fkit commit --author \"Name <you@example.com>\"\n\
         for a single commit."
    )
}

/// Unix seconds — what a commit stores, and what `git log --format=%at`
/// prints, so an importer needs no conversion.
///
/// Deliberately not a date parser. Accepting "yesterday" or a local-time string
/// would mean a timezone guess, and a history imported into the wrong timezone
/// is wrong in a way nobody notices until much later. `date -d <whatever> +%s`
/// converts anything else.
fn parse_date(raw: &str) -> Result<i64> {
    raw.trim()
        .parse::<i64>()
        .with_context(|| format!("--date wants unix seconds, not '{raw}' (try: date -d ... +%s)"))
}

fn cmd_commit(args: &[String]) -> Result<()> {
    let mut message: Option<String> = None;
    let mut who = CommitAs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-m" | "--message" => {
                message = Some(args.get(i + 1).context("-m needs a message")?.clone());
                i += 2;
            }
            // Both of these exist for importers replaying a history from
            // somewhere else; see `Repo::commit_as`.
            "--author" => {
                who.author = Some(args.get(i + 1).context("--author needs a value")?.clone());
                i += 2;
            }
            "--date" => {
                let raw = args.get(i + 1).context("--date needs a value")?;
                who.timestamp = Some(parse_date(raw)?);
                i += 2;
            }
            other => bail!("unexpected argument '{other}'"),
        }
    }
    let message = message.context("usage: fkit commit -m <message>")?;

    let repo = here()?;
    // An explicit --author is the answer to "who wrote this", so it satisfies
    // the requirement on its own.
    if who.author.is_none() {
        require_author(&repo)?;
    }
    let res = repo.commit_as(&message, &who)?;
    let branch = match repo.head()? {
        Head::Branch(b) => b,
        Head::Detached(_) => "(detached)".into(),
    };

    // Every writing process opens its own segment (that is what removes the
    // need for locking), so a busy repository accumulates them. Fold them back
    // together once there are enough to matter, rather than making the user
    // remember to run `fkit pack`.
    const SEGMENT_TIDY_THRESHOLD: usize = 24;
    if repo.store.segment_count() > SEGMENT_TIDY_THRESHOLD {
        let folded = repo.store.consolidate(64 * 1024 * 1024)?;
        if folded > 1 {
            println!("  tidied {folded} segments into one");
        }
    }

    println!("[{branch} {}] {message}", res.commit.short());
    println!("  tree {}", res.tree.short());
    let s = res.stats;
    println!(
        "  {} new object(s), {} written; {} object(s) already stored ({} deduplicated)",
        s.objects_written,
        human(s.bytes_written),
        s.objects_deduped,
        human(s.bytes_deduped),
    );
    Ok(())
}

fn cmd_log(args: &[String]) -> Result<()> {
    let mut limit = 20usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => {
                limit = args.get(i + 1).context("-n needs a count")?.parse()?;
                i += 2;
            }
            other => bail!("unexpected argument '{other}'"),
        }
    }

    let repo = here()?;
    let Some(head) = repo.head_commit()? else {
        println!("no commits yet");
        return Ok(());
    };

    for (id, c) in repo.history(head, limit)? {
        println!("commit {id}");
        println!("author  {}", c.author);
        println!("date    {}", fmt_time(c.timestamp));
        println!("tree    {}", c.tree.short());
        if c.parents.len() > 1 {
            let ps: Vec<String> = c.parents.iter().map(|p| p.short()).collect();
            println!("merge   {}", ps.join(" "));
        }
        println!();
        for line in c.message.lines() {
            println!("    {line}");
        }
        println!();
    }
    Ok(())
}

fn cmd_diff(args: &[String]) -> Result<()> {
    let repo = here()?;

    let mut stat = false;
    let mut revs: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--stat" => { stat = true; i += 1 }
            "-U" | "--unified" => {
                // Accepted and validated, but the hunk grouping uses the
                // library default; surfacing a knob we ignore would be worse
                // than saying so.
                let _: usize = args
                    .get(i + 1)
                    .context("-U needs a number")?
                    .parse()
                    .context("-U takes a line count")?;
                bail!("-U is not implemented yet; context is fixed at {}", linediff::CONTEXT);
            }
            other if other.starts_with('-') => bail!("unknown option '{other}'"),
            other => { revs.push(other.to_string()); i += 1 }
        }
    }

    // Only take a working-tree snapshot when the command actually needs one.
    let snap = if revs.len() < 2 { Some(repo.snapshot()?) } else { None };
    let wt = snap.as_ref().map(|s| s.hash);

    let (old, new) = match revs.len() {
        0 => (repo.head_tree()?, wt),
        1 => (Some(tree_of(&repo, resolve(&repo, &revs[0])?)?), wt),
        _ => (
            Some(tree_of(&repo, resolve(&repo, &revs[0])?)?),
            Some(tree_of(&repo, resolve(&repo, &revs[1])?)?),
        ),
    };

    let view = match &snap {
        Some(s) => repo.view_with(s),
        None => repo.view(),
    };
    let changes = diff_trees(&view, old, new)?;

    if changes.is_empty() {
        println!("no differences");
        return Ok(());
    }

    if stat {
        for c in &changes {
            println!("{} {}", c.sigil(), c.path());
        }
        println!("\n{} path(s) changed", changes.len());
        return Ok(());
    }

    // The working tree may hold content that was never written to the store, so
    // read through the same overlay view the diff was computed against.
    let old_files = match old {
        Some(t) => view.walk_tree(t)?,
        None => Default::default(),
    };
    let new_files = match new {
        Some(t) => view.walk_tree(t)?,
        None => Default::default(),
    };

    let (mut added, mut removed) = (0usize, 0usize);

    for c in &changes {
        let path = c.path();
        let before = read_side(&repo, &old_files, path)?;
        let after = read_side(&repo, &new_files, path)?;

        println!("{} {}", c.sigil(), path);

        let d = linediff::diff(&before, &after);
        added += d.added;
        removed += d.removed;

        if d.binary {
            println!("  binary file");
            continue;
        }
        if d.only_line_endings {
            println!("  line endings changed only");
            continue;
        }
        if d.truncated {
            println!("  (files differ too much for a line diff — replaced wholesale)");
        }

        for h in &d.hunks {
            println!("  {}", h.header());
            for l in &h.lines {
                let mark = match l.op {
                    linediff::Op::Equal => ' ',
                    linediff::Op::Delete => '-',
                    linediff::Op::Insert => '+',
                };
                println!("  {mark}{}", l.text);
            }
        }
        if d.old_no_eol != d.new_no_eol {
            println!("  \\ no newline at end of file");
        }
        println!();
    }

    println!(
        "{} path(s) changed, {added} insertion(s), {removed} deletion(s)",
        changes.len()
    );
    Ok(())
}

/// Read one side of a diff, or empty bytes when the path is absent there.
///
/// A working-tree side may reference objects that only exist in the dry-run
/// snapshot, so a missing object is treated as "not present on this side"
/// rather than an error.
fn read_side(
    repo: &Repo,
    files: &std::collections::BTreeMap<String, fkit_core::TreeEntry>,
    path: &str,
) -> Result<Vec<u8>> {
    let Some(entry) = files.get(path) else {
        return Ok(Vec::new());
    };
    if !repo.store.has(entry.hash) {
        // Uncommitted content: read it straight off disk.
        return Ok(std::fs::read(repo.root.join(path)).unwrap_or_default());
    }
    let mut buf = Vec::new();
    read_file(&repo.store, entry.hash, &mut buf)?;
    Ok(buf)
}

/// `fkit tag` — mark a commit with a name.
///
/// Tags are refs, stored beside branches and pushed with them. The difference
/// that matters is that a tag does not move: it is a claim about what a name
/// meant at a moment, and repointing it makes every earlier checkout of that
/// name silently wrong.
fn cmd_tag(args: &[String]) -> Result<()> {
    let repo = here()?;
    let mut force = false;
    let mut delete = false;
    let mut positional: Vec<&String> = Vec::new();
    for a in args {
        match a.as_str() {
            "--force" | "-f" => force = true,
            "--delete" | "-d" => delete = true,
            other if other.starts_with('-') => bail!("unknown option '{other}'"),
            _ => positional.push(a),
        }
    }

    if delete {
        let name = positional.first().context("usage: fkit tag -d <name>")?;
        repo.delete_tag(name)?;
        println!("deleted tag {name}");
        return Ok(());
    }

    let Some(name) = positional.first() else {
        let tags = repo.list_tags()?;
        if tags.is_empty() {
            println!("no tags — create one with: fkit tag v1.0");
            return Ok(());
        }
        for (name, hash) in &tags {
            let summary = match repo.store.get(*hash) {
                Ok(Object::Commit(c)) => c.message.lines().next().unwrap_or_default().to_string(),
                _ => String::new(),
            };
            println!("  {:<20} {}  {summary}", name, hash.short());
        }
        return Ok(());
    };

    if !fkit_core::session::valid_tag(name) {
        bail!(
            "'{name}' is not a valid tag name — letters, digits, dot, underscore \
             and hyphen, starting with a letter or digit"
        );
    }

    // Default to whatever HEAD is on, which is what you almost always mean
    // right after committing a release.
    let target = match positional.get(1) {
        Some(spec) => resolve(&repo, spec)?,
        None => repo.head_commit()?.context("no commits yet — nothing to tag")?,
    };
    // Naming a tree or a chunk as a release would be meaningless.
    match repo.store.get(target)? {
        Object::Commit(_) => {}
        other => bail!("{} is a {}, not a commit", target.short(), other.kind().name()),
    }

    repo.write_tag(name, target, force)?;
    println!("tag {name} -> {}", target.short());
    Ok(())
}

fn cmd_branch(args: &[String]) -> Result<()> {
    let repo = here()?;
    match args {
        [] => {
            let current = match repo.head()? {
                Head::Branch(b) => Some(b),
                Head::Detached(_) => None,
            };
            let refs = repo.list_refs()?;
            if refs.is_empty() {
                println!("no branches yet (the first commit creates one)");
            }
            for (name, h) in refs {
                let marker = if Some(&name) == current.as_ref() { "*" } else { " " };
                println!("{marker} {name}  {}", h.short());
            }
            Ok(())
        }
        [flag, name] if flag == "-d" => {
            if let Head::Branch(b) = repo.head()?
                && &b == name {
                    bail!("cannot delete '{name}': it is the current branch");
                }
            repo.delete_ref(name)?;
            println!("deleted branch {name}");
            Ok(())
        }
        [name] => {
            if !fkit_core::session::valid_new_branch(name) {
                bail!(
                    "'{name}' is not a usable branch name — 'tags/' is reserved for tags, \
                     and a name must start with a letter or digit"
                );
            }
            if repo.read_ref(name)?.is_some() {
                bail!("branch '{name}' already exists");
            }
            let at = repo.head_commit()?.context("cannot branch before the first commit")?;
            repo.write_ref(name, at)?;
            println!("created branch {name} at {}", at.short());
            Ok(())
        }
        _ => bail!("usage: fkit branch [<name> | -d <name>]"),
    }
}

fn cmd_switch(args: &[String]) -> Result<()> {
    let repo = here()?;
    let mut force = false;
    let mut name = None;
    for a in args {
        if a == "--force" || a == "-f" {
            force = true;
        } else {
            name = Some(a.clone());
        }
    }
    let name = name.context("usage: fkit switch <branch>")?;
    let target = repo
        .read_ref(&name)?
        .with_context(|| format!("no such branch: {name}"))?;

    let from = repo.head_tree()?;
    let plan = checkout_tree(&repo, from, tree_of(&repo, target)?, force)?;
    repo.set_head(&Head::Branch(name.clone()))?;
    println!("switched to branch {name} ({})", target.short());
    report_plan(&plan);
    Ok(())
}

fn cmd_merge(args: &[String]) -> Result<()> {
    use fkit_core::merge::{merge_base, merge_trees, ConflictKind};

    let repo = here()?;
    let mut message: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-m" | "--message" => {
                message = Some(args.get(i + 1).context("-m needs a message")?.clone());
                i += 2;
            }
            other if other.starts_with('-') => bail!("unknown option '{other}'"),
            other => { branch = Some(other.to_string()); i += 1 }
        }
    }
    let spec = branch.context("usage: fkit merge <branch> [-m <message>]")?;

    if repo.merge_head()?.is_some() {
        bail!("a merge is already in progress — resolve the conflicts and commit, \
               or reset the working tree");
    }

    let ours = repo.head_commit()?.context("nothing to merge into: no commits yet")?;
    let theirs = resolve(&repo, &spec)?;
    let current = match repo.head()? {
        Head::Branch(b) => b,
        Head::Detached(_) => bail!("HEAD is detached — switch to a branch before merging"),
    };

    // The working tree must be clean: a merge rewrites tracked files, and
    // mixing that with uncommitted edits makes it impossible to tell afterwards
    // which change came from where.
    let snap = repo.snapshot()?;
    if Some(snap.hash) != repo.head_tree()? {
        bail!("you have uncommitted changes — commit or discard them before merging");
    }

    if is_ancestor(&repo.store, theirs, ours)? {
        println!("already up to date");
        return Ok(());
    }

    // Fast-forward: our history is fully contained in theirs, so there is
    // nothing to combine — just move the branch.
    if is_ancestor(&repo.store, ours, theirs)? {
        let from = repo.head_tree()?;
        let plan = checkout_tree(&repo, from, tree_of(&repo, theirs)?, false)?;
        repo.write_ref(&current, theirs)?;
        println!("fast-forward {current} -> {}", theirs.short());
        println!("  {} written, {} removed", plan.written, plan.removed);
        return Ok(());
    }

    let mb = merge_base(&repo.store, ours, theirs)?;
    let base = mb.base;
    match base {
        Some(b) => println!("merge base {}", b.short()),
        None => println!("no common ancestor — merging unrelated histories"),
    }
    if mb.ambiguous {
        println!(
            "  note: several equally-good merge bases exist; using one of them.\n\
             \x20 Review the result carefully."
        );
    }

    let base_tree = match base {
        Some(b) => Some(tree_of(&repo, b)?),
        None => None,
    };
    let outcome = merge_trees(
        &repo.store,
        base_tree,
        tree_of(&repo, ours)?,
        tree_of(&repo, theirs)?,
    )?;

    // Put the merged content on disk either way — a conflicted merge is still
    // most of the work done, and the markers are what you resolve.
    let from = repo.head_tree()?;
    let plan = checkout_tree(&repo, from, outcome.tree, true)?;
    println!("  {} written, {} removed", plan.written, plan.removed);

    if !outcome.clean() {
        repo.set_merge_head(theirs)?;
        println!("\n{} conflict(s):", outcome.conflicts.len());
        for c in &outcome.conflicts {
            let what = match c.kind {
                ConflictKind::Content { regions } => format!("{regions} overlapping region(s)"),
                ConflictKind::Binary => "binary file".into(),
                ConflictKind::DeleteModify => "deleted on one side, modified on the other".into(),
                ConflictKind::TypeChange => "changed to a different kind of entry".into(),
            };
            println!("  {} — {what}", c.path);
        }
        println!(
            "\nResolve them, then run:\n  fkit commit -m \"merge {spec} into {current}\"\n\
             The other parent is recorded, so the merge is still recorded as a merge."
        );
        return Ok(());
    }

    let message = message.unwrap_or_else(|| format!("merge {spec} into {current}"));
    repo.set_merge_head(theirs)?;
    let res = repo.commit(&message)?;
    println!("\n[{current} {}] {message}", res.commit.short());
    println!("  merged cleanly");
    Ok(())
}

fn cmd_checkout(args: &[String]) -> Result<()> {
    let repo = here()?;
    let mut force = false;
    let mut spec = None;
    for a in args {
        if a == "--force" || a == "-f" {
            force = true;
        } else {
            spec = Some(a.clone());
        }
    }
    let spec = spec.context("usage: fkit checkout <commit> [--force]")?;
    let target = resolve(&repo, &spec)?;

    let from = repo.head_tree()?;
    let plan = checkout_tree(&repo, from, tree_of(&repo, target)?, force)?;
    repo.set_head(&Head::Detached(target))?;
    println!("HEAD is now detached at {}", target.short());
    report_plan(&plan);
    Ok(())
}

fn report_plan(plan: &fkit_core::checkout::CheckoutPlan) {
    if plan.touched() == 0 {
        println!("  working tree already matched");
    } else {
        println!("  {} written, {} removed", plan.written, plan.removed);
    }
}

fn cmd_show(args: &[String]) -> Result<()> {
    let repo = here()?;
    let spec = args.first().context("usage: fkit show <hash>")?;
    let id = resolve(&repo, spec)?;
    let obj = repo.store.get_verified(id)?;

    println!("object {id}");
    println!("kind   {}", obj.kind().name());
    println!("size   {} of content", human(obj.content_size()));
    println!();

    match &obj {
        Object::Chunk(d) => {
            println!("{} raw bytes", d.len());
            match std::str::from_utf8(d) {
                Ok(s) if s.chars().all(|c| !c.is_control() || c.is_whitespace()) => {
                    let preview: String = s.chars().take(500).collect();
                    println!("---\n{preview}");
                }
                _ => println!("(binary)"),
            }
        }
        Object::File { level, children } => {
            println!("file node at level {level} with {} child(ren)", children.len());
            let kind = if *level == 0 { "chunk" } else { "file node" };
            for (h, n) in children.iter().take(40) {
                println!("  {kind} {}  {}", h.short(), human(*n));
            }
            if children.len() > 40 {
                println!("  ... and {} more", children.len() - 40);
            }
        }
        Object::Tree { level, children } => {
            let entries = read_entries(&repo.store, id)?;
            println!(
                "directory node at level {level}: {} run(s), {} entr(ies)",
                children.len(),
                entries.len()
            );
            for e in entries {
                let k = match e.kind {
                    EntryKind::Dir => "dir ",
                    EntryKind::Symlink => "link",
                    EntryKind::File { exec: true } => "exec",
                    EntryKind::File { exec: false } => "file",
                    EntryKind::Submodule => "sub ",
                };
                println!("  {k} {}  {:>9}  {}", e.hash.short(), human(e.size), e.name);
            }
        }
        Object::Entries(entries) => {
            println!("a run of {} directory entr(ies)", entries.len());
            for e in entries {
                let k = match e.kind {
                    EntryKind::Dir => "dir ",
                    EntryKind::Symlink => "link",
                    EntryKind::File { exec: true } => "exec",
                    EntryKind::File { exec: false } => "file",
                    EntryKind::Submodule => "sub ",
                };
                println!("  {k} {}  {:>9}  {}", e.hash.short(), human(e.size), e.name);
            }
        }
        Object::Commit(c) => {
            println!("tree      {}", c.tree);
            for p in &c.parents {
                println!("parent    {p}");
            }
            println!("author    {}", c.author);
            println!("date      {}", fmt_time(c.timestamp));
            println!("\n    {}", c.message);
        }
    }
    Ok(())
}

fn cmd_tree(args: &[String]) -> Result<()> {
    let repo = here()?;
    let commit = match args.first() {
        Some(s) => resolve(&repo, s)?,
        None => repo.head_commit()?.context("no commits yet")?,
    };
    let files = repo.walk_tree(tree_of(&repo, commit)?)?;
    let mut total = 0u64;
    for (path, e) in &files {
        println!("{}  {:>9}  {path}", e.hash.short(), human(e.size));
        total += e.size;
    }
    println!("\n{} file(s), {}", files.len(), human(total));
    Ok(())
}

fn cmd_cat(args: &[String]) -> Result<()> {
    let repo = here()?;
    let path = args.first().context("usage: fkit cat <path> [<commit>]")?;
    let commit = match args.get(1) {
        Some(s) => resolve(&repo, s)?,
        None => repo.head_commit()?.context("no commits yet")?,
    };
    let files = repo.walk_tree(tree_of(&repo, commit)?)?;
    let entry = files
        .get(path.as_str())
        .with_context(|| format!("no such path in that commit: {path}"))?;

    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    read_file(&repo.store, entry.hash, &mut out)?;
    out.flush()?;
    Ok(())
}

/// Visualise the Merkle tree under an object. This is the command that makes the
/// data structure legible: you can watch a file decompose into chunks, and see
/// which chunks are shared with other files.
fn cmd_merkle(args: &[String]) -> Result<()> {
    let repo = here()?;
    let spec = args.first().context("usage: fkit merkle <hash>")?;
    let id = resolve(&repo, spec)?;
    let mut seen = std::collections::HashSet::new();
    walk_merkle(&repo, id, "", true, &mut seen, 0)?;
    Ok(())
}

fn walk_merkle(
    repo: &Repo,
    id: Hash,
    prefix: &str,
    last: bool,
    seen: &mut std::collections::HashSet<Hash>,
    depth: usize,
) -> Result<()> {
    let obj = repo.store.get(id)?;
    let branch = if depth == 0 {
        ""
    } else if last {
        "└── "
    } else {
        "├── "
    };

    let label = match &obj {
        Object::Chunk(d) => format!("chunk {}  {}", id.short(), human(d.len() as u64)),
        Object::File { level, children } => format!(
            "file  {}  level {level}, {} children, {}",
            id.short(),
            children.len(),
            human(obj.content_size())
        ),
        Object::Tree { level, children } => format!(
            "tree  {}  level {level}, {} run(s)",
            id.short(),
            children.len()
        ),
        Object::Entries(e) => format!("entries {}  {} entr(ies)", id.short(), e.len()),
        Object::Commit(c) => format!("commit {}  {}", id.short(), c.message.lines().next().unwrap_or("")),
    };

    // A repeated hash means genuine sharing: the same bytes reached by two
    // different paths. Marking it makes deduplication visible.
    if !seen.insert(id) {
        println!("{prefix}{branch}{label}  (shared, already shown)");
        return Ok(());
    }
    println!("{prefix}{branch}{label}");

    if depth > 6 {
        println!("{prefix}    ... (depth limit)");
        return Ok(());
    }

    let child_prefix = if depth == 0 {
        String::new()
    } else {
        format!("{prefix}{}", if last { "    " } else { "│   " })
    };

    let links: Vec<(Hash, Option<String>)> = match &obj {
        Object::Entries(entries) => {
            entries.iter().map(|e| (e.hash, Some(e.name.clone()))).collect()
        }
        other => other.links().into_iter().map(|h| (h, None)).collect(),
    };

    let shown = links.len().min(12);
    for (i, (h, name)) in links.iter().take(shown).enumerate() {
        let is_last = i + 1 == links.len();
        if let Some(n) = name {
            println!("{child_prefix}{} {n}", if is_last { "└──" } else { "├──" });
            walk_merkle(repo, *h, &format!("{child_prefix}{}", if is_last { "    " } else { "│   " }), true, seen, depth + 1)?;
        } else {
            walk_merkle(repo, *h, &child_prefix, is_last, seen, depth + 1)?;
        }
    }
    if links.len() > shown {
        println!("{child_prefix}... and {} more", links.len() - shown);
    }
    Ok(())
}

// ---- remotes --------------------------------------------------------------

/// The last path segment of a ws:// or wss:// URL is the repository name.
fn repo_name_from_url(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("repo")
        .to_string()
}

fn auth_token(repo: Option<&Repo>) -> String {
    std::env::var("FKIT_TOKEN")
        .ok()
        .or_else(|| repo.and_then(|r| r.config_get("token")))
        .unwrap_or_default()
}

/// Connect and complete the Hello/Welcome exchange.
fn connect(url: &str, repo: Option<&Repo>) -> Result<(WebSocket, Vec<(String, Hash)>)> {
    let mut ws = WebSocket::connect(url)?;
    send(&mut ws, &Msg::Hello {
        repo: repo_name_from_url(url),
        token: auth_token(repo),
    })?;
    match recv(&mut ws)? {
        Msg::Welcome { refs } => Ok((ws, refs)),
        other => bail!("unexpected greeting from server: {other:?}"),
    }
}

fn remote_url(repo: &Repo) -> Result<String> {
    repo.config_get("remote")
        .context("no remote configured — run: fkit remote ws://host:7420/your-repo")
}

fn cmd_remote(args: &[String]) -> Result<()> {
    let repo = here()?;
    match args.first() {
        None => match repo.config_get("remote") {
            Some(u) => println!("{u}"),
            None => println!("no remote configured"),
        },
        Some(url) => {
            if !(url.starts_with("ws://") || url.starts_with("wss://")) {
                bail!("remote must be a ws:// or wss:// URL");
            }
            repo.config_set("remote", url)?;
            println!("remote = {url}");
        }
    }
    Ok(())
}

fn cmd_push(args: &[String]) -> Result<()> {
    let repo = here()?;
    let mut force = false;
    let mut with_tags = true;
    let mut branch = None;
    for a in args {
        match a.as_str() {
            "--force" | "-f" => force = true,
            // Tags ride along by default: a release tag left behind on the
            // machine that made it is a tag nobody else can act on.
            "--no-tags" => with_tags = false,
            other => branch = Some(other.to_string()),
        }
    }
    let branch = match branch {
        Some(b) => b,
        None => match repo.head()? {
            Head::Branch(b) => b,
            Head::Detached(_) => bail!("HEAD is detached — name a branch to push"),
        },
    };
    let tip = repo
        .read_ref(&branch)?
        .with_context(|| format!("branch '{branch}' has no commits"))?;

    let url = remote_url(&repo)?;
    println!("pushing {branch} ({}) to {url}", tip.short());

    let (mut ws, _refs) = connect(&url, Some(&repo))?;
    send(&mut ws, &Msg::PushRef { branch: branch.clone(), tip, force })?;

    // The server drives: it asks for what it lacks, we answer.
    let stats = serve_wants(&repo.store, &mut ws)?;

    match recv(&mut ws)? {
        Msg::Ok { message } => {
            println!(
                "  sent {} object(s), {} in {} round trip(s)",
                stats.objects, human(stats.bytes), stats.round_trips
            );
            println!("  {message}");
        }
        other => bail!("unexpected reply: {other:?}"),
    }

    if with_tags {
        push_tags(&repo, &mut ws)?;
    }
    ws.close();
    Ok(())
}

/// The first line of an error chain, for a one-line report.
fn first_line(e: &anyhow::Error) -> String {
    let s = e.to_string();
    s.lines().next().unwrap_or(&s).trim_start_matches("remote error: ").to_string()
}

/// Send every local tag the remote does not already have at the same commit.
///
/// Done after the branch, on the same connection, so the commits a tag names
/// are already there. A tag whose commit is not reachable from any branch is
/// still sent — the server asks for what it lacks either way.
fn push_tags(repo: &Repo, ws: &mut WebSocket) -> Result<()> {
    let tags = repo.list_tags()?;
    if tags.is_empty() {
        return Ok(());
    }

    let mut sent = 0usize;
    let mut objects = 0usize;
    let mut skipped = Vec::new();
    for (name, tip) in &tags {
        let remote_name = format!("{}{name}", Repo::TAG_PREFIX);
        send(ws, &Msg::PushRef { branch: remote_name, tip: *tip, force: false })?;

        // A rejection arrives as an Error, which `recv` raises rather than
        // returns — so a conflicting tag would otherwise abort the whole push
        // and take the tags after it with it. The server stays in its command
        // loop after refusing, so the connection is fine to keep using.
        match serve_wants(&repo.store, ws).and_then(|stats| {
            objects += stats.objects as usize;
            recv(ws)
        }) {
            Ok(Msg::Ok { .. }) => sent += 1,
            Ok(other) => bail!("unexpected reply pushing tag {name}: {other:?}"),
            Err(e) => skipped.push(format!("{name}: {}", first_line(&e))),
        }
    }

    match objects {
        0 => println!("  {sent} tag(s) already current"),
        n => println!("  {sent} tag(s), {n} new object(s)"),
    }
    for s in &skipped {
        println!("  tag not pushed — {s}");
    }
    Ok(())
}

fn cmd_pull(args: &[String]) -> Result<()> {
    let repo = here()?;
    let branch = match args.first() {
        Some(b) => b.clone(),
        None => match repo.head()? {
            Head::Branch(b) => b,
            Head::Detached(_) => bail!("HEAD is detached — name a branch to pull"),
        },
    };

    let url = remote_url(&repo)?;
    let on_this_branch = repo.head()? == Head::Branch(branch.clone());
    // Capture the tree the working directory currently reflects, *before*
    // pull_branch advances the ref out from under us.
    let before = repo.head_tree()?;

    let (mut ws, _) = connect(&url, Some(&repo))?;
    let stats = pull_branch(&repo, &mut ws, &branch)?;
    ws.close();

    let tip = repo.read_ref(&branch)?.context("pull produced no tip")?;
    println!(
        "  received {} object(s), {} in {} round trip(s)",
        stats.objects, human(stats.bytes), stats.round_trips
    );

    // Update the working tree only if we are actually on this branch.
    if on_this_branch {
        match checkout_tree(&repo, before, tree_of(&repo, tip)?, false) {
            Ok(plan) => println!("  {} written, {} removed in the working tree", plan.written, plan.removed),
            Err(e) => println!("  note: working tree not updated — {e}"),
        }
    }
    println!("{branch} is now at {}", tip.short());
    Ok(())
}

/// Fetch one branch into `repo`, enforcing fast-forward on the local ref.
fn pull_branch(
    repo: &Repo,
    ws: &mut WebSocket,
    branch: &str,
) -> Result<fkit_core::proto::TransferStats> {
    send(ws, &Msg::PullRef { branch: branch.to_string() })?;
    let tip = match recv(ws)? {
        Msg::RefIs { tip: Some(t), .. } => t,
        Msg::RefIs { tip: None, .. } => bail!("remote has no branch '{branch}'"),
        other => bail!("unexpected reply: {other:?}"),
    };

    let stats = fetch_closure(&repo.store, ws, &[tip])?;
    verify_closure(&repo.store, tip)?;

    if let Some(tag) = branch.strip_prefix(Repo::TAG_PREFIX) {
        // A tag has no history to fast-forward. If the remote's differs from
        // ours, one of them is a lie about what the name meant; say so rather
        // than picking a winner.
        match repo.read_tag(tag)? {
            Some(old) if old != tip => bail!(
                "refusing to pull: tag '{tag}' is {} here and {} on the remote — \
                 delete the local tag if the remote's is the one you want",
                old.short(),
                tip.short()
            ),
            Some(_) => {}
            None => repo.write_tag(tag, tip, false)?,
        }
    } else {
        if let Some(old) = repo.read_ref(branch)?
            && old != tip && !is_ancestor(&repo.store, old, tip)? {
                bail!(
                    "refusing to pull: your local '{branch}' ({}) is not an ancestor of the \
                     remote's ({}) — histories have diverged",
                    old.short(),
                    tip.short()
                );
            }
        repo.write_ref(branch, tip)?;
    }

    // Drain the server's trailing Ok.
    let _ = recv(ws);
    Ok(stats)
}


// ---- submodules ---------------------------------------------------------
//
// A submodule is a commit of another repository, pinned by a tree entry in
// this one. Everything below is a thin layer over that: the interesting work
// is done by `walk_tree`, which expands a pin into content, and by
// `checkout_tree`, which keeps `.fkit/submodules/` in step with whatever tree
// it just wrote. Neither needed to be taught what a submodule is.

fn cmd_submodule(args: &[String]) -> Result<()> {
    let repo = Repo::discover(&std::env::current_dir()?)?;
    match args.first().map(|s| s.as_str()) {
        None | Some("list") => sub_list(&repo),
        Some("add") => sub_add(&repo, &args[1..]),
        Some("update") => sub_update(&repo, &args[1..]),
        Some("fetch") => sub_fetch(&repo, &args[1..]),
        Some("rm") | Some("remove") => sub_rm(&repo, &args[1..]),
        Some("set-remote") => sub_set_remote(&repo, &args[1..]),
        Some(other) => bail!(
            "unknown submodule command '{other}'\n\
             try: list, add, update, fetch, rm"
        ),
    }
}

/// The pin recorded by HEAD, if there is a HEAD.
fn head_pins(repo: &Repo) -> Result<std::collections::BTreeMap<String, Hash>> {
    match repo.head_tree()? {
        Some(t) => repo.view().submodules(t),
        None => Ok(Default::default()),
    }
}

fn sub_list(repo: &Repo) -> Result<()> {
    let mounts = fkit_core::submodule::list(repo)?;
    if mounts.is_empty() {
        println!("no submodules");
        return Ok(());
    }
    let committed = head_pins(repo)?;
    let declared = fkit_core::submodule::hints(repo);

    for (path, m) in &mounts {
        // Three things can be true of a submodule, and they are worth keeping
        // apart: what the last commit pinned, what is recorded here, and
        // whether the content is actually present to check out.
        let state = if !repo.store.has(m.pin) {
            "missing — run `fkit submodule fetch`"
        } else {
            match committed.get(path) {
                Some(c) if *c == m.pin => "clean",
                Some(_) => "moved — commit to record it",
                None => "new — commit to record it",
            }
        };
        println!("{}  {}  {}", m.pin.short(), path, state);
        if !m.remote.is_empty() {
            println!("    from {}", m.remote);
        }
        // The manifest is a declaration that travels with the tree; the tree
        // is what actually pins. Say so rather than letting a stale line be
        // read as fact.
        if let Some(sug) = declared.get(path)
            && let Some(want) = sug.pin
            && Some(&want) != committed.get(path)
            && want != m.pin
        {
            println!(
                "    note: {} declares {} — the commit is what pins, so that line is stale",
                fkit_core::submodule::HINTS_FILE,
                want.short()
            );
        }
    }
    Ok(())
}

fn sub_add(repo: &Repo, args: &[String]) -> Result<()> {
    let branch_flag = flag_value(args, "--branch");
    let pos: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let (url, path) = match (pos.first(), pos.get(1)) {
        (Some(u), Some(p)) => (u.as_str(), p.as_str()),
        _ => bail!("usage: fkit submodule add <wss://host/owner/repo> <path> [--branch <b>]"),
    };
    // `--branch main` leaves "main" in the positional list unless it is taken
    // out, which would otherwise be read as the path.
    let path = if Some(path.to_string()) == branch_flag {
        pos.get(2).map(|p| p.as_str()).context("usage: fkit submodule add <url> <path>")?
    } else {
        path
    };

    fkit_core::submodule::valid_path(path)?;
    if fkit_core::submodule::read(repo, path)?.is_some() {
        bail!("{path} is already a submodule");
    }
    let dest = repo.root.join(path);
    if dest.exists() && std::fs::read_dir(&dest)?.next().is_some() {
        bail!("{path} already exists and is not empty");
    }

    let (mut ws, refs) = connect(url, None)?;
    let branch = pick_branch(&refs, branch_flag.as_deref())?;
    println!("mounting {url} ({branch}) at {path}");

    // Objects land in *this* repository's store. There is no second store and
    // no nested repository: the submodule's content is this repository's
    // content, which is what makes checkout of it local and immediate.
    let tip = fetch_ref(repo, &mut ws, &branch)?;
    let stats = fkit_core::proto::fetch_closure(&repo.store, &mut ws, &[tip])?;
    fkit_core::proto::verify_closure(&repo.store, tip)?;
    let _ = send(&mut ws, &Msg::Done);

    fkit_core::submodule::write(repo, &fkit_core::submodule::Mount {
        path: path.to_string(),
        remote: url.to_string(),
        pin: tip,
    })?;

    // The URL exactly as given, and the revision it is pinned at. Written in
    // full rather than relative: a reader should be able to see where this
    // comes from without first working out what it is relative to. A relative
    // url still works if one is written by hand.
    let hint = url.to_string();
    fkit_core::submodule::set_hint(
        repo,
        path,
        Some(fkit_core::submodule::Suggestion { url: hint.clone(), pin: Some(tip) }),
    )?;

    let n = fkit_core::checkout::materialize(repo, tree_of(repo, tip)?, &dest)?;
    println!("  {} object(s), {n} file(s) at {}", stats.objects, tip.short());
    // Say what went into the tracked file and what it means, so the `../`
    // is not something to work out later from a file with no context.
    println!("  {} now declares {hint}@{tip}", fkit_core::submodule::HINTS_FILE);
    println!("commit to record it");
    Ok(())
}

fn sub_update(repo: &Repo, args: &[String]) -> Result<()> {
    let branch_flag = flag_value(args, "--branch");
    let only = args.iter().find(|a| !a.starts_with('-')).cloned();
    let mounts = fkit_core::submodule::list(repo)?;
    if mounts.is_empty() {
        bail!("no submodules to update");
    }

    let targets: Vec<_> = match &only {
        Some(p) => vec![mounts.get(p).cloned().with_context(|| format!("{p} is not a submodule"))?],
        None => mounts.values().cloned().collect(),
    };

    for m in targets {
        if m.remote.is_empty() {
            println!("{}: no remote recorded, skipping", m.path);
            continue;
        }
        let (mut ws, refs) = connect(&m.remote, None)?;
        let branch = pick_branch(&refs, branch_flag.as_deref())?;
        let tip = fetch_ref(repo, &mut ws, &branch)?;
        if tip == m.pin {
            println!("{}: already at {}", m.path, tip.short());
            let _ = send(&mut ws, &Msg::Done);
            continue;
        }
        let stats = fkit_core::proto::fetch_closure(&repo.store, &mut ws, &[tip])?;
        fkit_core::proto::verify_closure(&repo.store, tip)?;
        let _ = send(&mut ws, &Msg::Done);

        fkit_core::submodule::set_pin(repo, &m.path, tip)?;
        println!(
            "{}: {} -> {} ({} object(s))",
            m.path,
            m.pin.short(),
            tip.short(),
            stats.objects
        );
    }
    println!("commit to record the new pin(s)");
    Ok(())
}

fn sub_fetch(repo: &Repo, args: &[String]) -> Result<()> {
    let only = args.iter().find(|a| !a.starts_with('-')).cloned();
    let mounts = fkit_core::submodule::list(repo)?;
    let mut missing = 0;
    for (path, m) in &mounts {
        if only.as_ref().is_some_and(|p| p != path) {
            continue;
        }
        if repo.store.has(m.pin) {
            continue;
        }
        missing += 1;
        if m.remote.is_empty() {
            println!("{path}: pinned at {} with no remote recorded", m.pin.short());
            continue;
        }
        // Ask for the pinned commit by hash. A branch would be the wrong
        // question: the pin may be an older commit, or on no branch at all.
        let (mut ws, _) = connect(&m.remote, None)?;
        let stats = fkit_core::proto::fetch_closure(&repo.store, &mut ws, &[m.pin])?;
        fkit_core::proto::verify_closure(&repo.store, m.pin)?;
        let _ = send(&mut ws, &Msg::Done);
        fkit_core::submodule::set_pin(repo, path, m.pin)?;
        println!("{path}: {} object(s) at {}", stats.objects, m.pin.short());
    }
    if missing == 0 {
        println!("every submodule's content is already here");
    }
    Ok(())
}

fn sub_rm(repo: &Repo, args: &[String]) -> Result<()> {
    let path = args.first().context("usage: fkit submodule rm <path>")?;
    let m = fkit_core::submodule::read(repo, path)?
        .with_context(|| format!("{path} is not a submodule"))?;

    // Take the files away as well as the record. Leaving them would make the
    // next commit ingest the submodule's content as this repository's own,
    // which is the quiet way a reference turns into a fork.
    let dest = repo.root.join(&m.path);
    if repo.store.has(m.pin) {
        for rel in repo.view().walk_tree(tree_of(repo, m.pin)?)?.keys() {
            let _ = std::fs::remove_file(dest.join(rel));
        }
    }
    let _ = std::fs::remove_dir_all(&dest);
    fkit_core::submodule::remove(repo, &m.path)?;
    fkit_core::submodule::set_hint(repo, &m.path, None)?;
    println!("unmounted {}", m.path);
    println!("commit to record the removal");
    Ok(())
}

/// Point one submodule at a different remote, for this machine only.
///
/// The project's suggestion in `.fkit-submodules` is left alone: this is the
/// override, and overrides that edit the thing they override are how you end
/// up needing a command like `git submodule sync`.
fn sub_set_remote(repo: &Repo, args: &[String]) -> Result<()> {
    let (path, url) = match (args.first(), args.get(1)) {
        (Some(p), Some(u)) => (p, u),
        _ => bail!("usage: fkit submodule set-remote <path> <url>"),
    };
    let m = fkit_core::submodule::read(repo, path)?
        .with_context(|| format!("{path} is not a submodule"))?;
    fkit_core::submodule::write(repo, &fkit_core::submodule::Mount {
        remote: url.to_string(),
        ..m
    })?;
    println!("{path} now fetches from {url}");
    Ok(())
}

/// Which branch of the remote to pin. `main` unless told otherwise, because
/// guessing from ref order would make the result depend on map iteration.
fn pick_branch(refs: &[(String, Hash)], want: Option<&str>) -> Result<String> {
    if let Some(b) = want {
        return Ok(b.to_string());
    }
    let branches: Vec<&String> =
        refs.iter().map(|(n, _)| n).filter(|n| !n.starts_with(Repo::TAG_PREFIX)).collect();
    if branches.iter().any(|n| n.as_str() == "main") {
        return Ok("main".to_string());
    }
    match branches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("remote has no branches"),
        many => bail!(
            "remote has no 'main' branch — pick one with --branch: {}",
            many.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Ask the remote what a branch points at, without touching local refs.
///
/// A submodule's branches are not this repository's branches, so `pull_branch`
/// would be wrong here: it writes the name into our own ref namespace.
fn fetch_ref(_repo: &Repo, ws: &mut WebSocket, branch: &str) -> Result<Hash> {
    send(ws, &Msg::PullRef { branch: branch.to_string() })?;
    match recv(ws)? {
        Msg::RefIs { tip: Some(t), .. } => Ok(t),
        Msg::RefIs { tip: None, .. } => bail!("remote has no branch '{branch}'"),
        other => bail!("unexpected reply: {other:?}"),
    }
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

fn cmd_clone(args: &[String]) -> Result<()> {
    // Flags may appear anywhere, so pick the positionals out rather than
    // assuming the URL is argv[1].
    let no_checkout = args.iter().any(|a| a == "--no-checkout" || a == "--bare");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();

    let url = positional
        .first()
        .copied()
        .context("usage: fkit clone [--no-checkout] <wss://host/owner/repo> [dir]")?;
    if !(url.starts_with("ws://") || url.starts_with("wss://")) {
        bail!("clone needs a ws:// or wss:// URL, got '{url}'");
    }
    let dir = positional
        .get(1)
        .map(|d| PathBuf::from(d.as_str()))
        .unwrap_or_else(|| PathBuf::from(repo_name_from_url(url)));

    if dir.exists() && std::fs::read_dir(&dir)?.next().is_some() {
        bail!("{} already exists and is not empty", dir.display());
    }

    // Peek at the remote before creating anything locally.
    let (mut ws, refs) = connect(url, None)?;
    if refs.is_empty() {
        bail!("remote repository is empty — nothing to clone");
    }

    std::fs::create_dir_all(&dir)?;
    let repo = Repo::init(&dir)?;
    repo.config_set("remote", url)?;

    println!("cloning {url} into {}", dir.display());
    let mut total = fkit_core::proto::TransferStats::default();
    for (name, _) in &refs {
        let s = pull_branch(&repo, &mut ws, name)?;
        match name.strip_prefix(Repo::TAG_PREFIX) {
            Some(tag) => println!("  tag {tag}: {} object(s)", s.objects),
            None => println!("  branch {name}: {} object(s)", s.objects),
        }
        total.objects += s.objects;
        total.bytes += s.bytes;
        total.round_trips += s.round_trips;
    }
    ws.close();

    // Prefer 'main', else the first branch. A tag is not a place to stand:
    // checking one out would leave HEAD detached on a fresh clone.
    let branches: Vec<&String> =
        refs.iter().map(|(n, _)| n).filter(|n| !n.starts_with(Repo::TAG_PREFIX)).collect();
    let primary = if branches.iter().any(|n| *n == "main") {
        "main".to_string()
    } else {
        branches
            .first()
            .map(|n| (*n).clone())
            .context("remote has tags but no branches — nothing to check out")?
    };
    repo.set_head(&Head::Branch(primary.clone()))?;
    let tip = repo.read_ref(&primary)?.context("missing tip after clone")?;
    let plan = if no_checkout {
        fkit_core::checkout::CheckoutPlan::default()
    } else {
        checkout_tree(&repo, None, tree_of(&repo, tip)?, true)?
    };

    println!(
        "received {} object(s), {} in {} round trip(s)",
        total.objects, human(total.bytes), total.round_trips
    );
    if no_checkout {
        println!("{primary} is at {} (no files written)", tip.short());
    } else {
        println!("checked out {primary} at {} ({} file(s))", tip.short(), plan.written);
    }
    Ok(())
}

fn cmd_prove(args: &[String]) -> Result<()> {
    let repo = here()?;
    let mut out_file: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--output" => {
                out_file = Some(args.get(i + 1).context("-o needs a file")?.clone());
                i += 2;
            }
            other if other.starts_with('-') => bail!("unknown option '{other}'"),
            other => { positional.push(other.to_string()); i += 1 }
        }
    }
    let path = positional.first().context("usage: fkit prove <path> [<commit>]")?;
    let commit = match positional.get(1) {
        Some(spec) => resolve(&repo, spec)?,
        None => repo.head_commit()?.context("no commits yet")?,
    };

    let proof = fkit_core::proof::prove(&repo.store, commit, path)?;
    let bytes = fkit_core::proof::encode(&proof);

    match out_file {
        Some(f) => {
            std::fs::write(&f, &bytes)?;
            println!("proof for {path}");
            println!("  root    {commit}");
            println!("  target  {}", proof.target.short());
            println!("  size    {}", human(proof.size));
            println!("  steps   {} nodes, {}", proof.steps.len(), human(bytes.len() as u64));
            println!("  written to {f}");
            println!("\nVerify anywhere, without this repository:");
            println!("  fkit verify {f} --root {commit}");
        }
        None => {
            use std::io::Write;
            std::io::stdout().write_all(&bytes)?;
        }
    }
    Ok(())
}

fn cmd_verify(args: &[String]) -> Result<()> {
    let mut root: Option<String> = None;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" | "-r" => {
                root = Some(args.get(i + 1).context("--root needs a commit hash")?.clone());
                i += 2;
            }
            other if other.starts_with('-') => bail!("unknown option '{other}'"),
            other => { file = Some(other.to_string()); i += 1 }
        }
    }
    let file = file.context("usage: fkit verify <proof-file> --root <commit-hash>")?;
    let root = root.context("--root is required: a proof is only meaningful against a hash you already trust")?;
    let root = Hash::from_hex(&root).context("--root must be a full 64-character commit hash")?;

    let bytes = std::fs::read(&file).with_context(|| format!("reading {file}"))?;
    let proof = fkit_core::proof::decode(&bytes)?;

    // Note: no repository is opened anywhere in this function. That is the point.
    match fkit_core::proof::verify(&proof, root) {
        Ok(v) => {
            println!("verified");
            println!("  {} ({}) is in {}", v.path, human(v.size), root.short());
            println!("  content hash {}", v.target.short());
            Ok(())
        }
        Err(e) => {
            eprintln!("PROOF REJECTED: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_pack() -> Result<()> {
    let repo = here()?;
    let before = repo.store.loose_ids()?.len();

    if before > 0 {
        println!("packing {before} loose object(s)…");
        let (moved, bytes) = repo.store.pack_loose()?;
        println!("  moved {moved} object(s), {} into segments", human(bytes));
    }

    // Each writing process owns its own segment, so a repository collects one
    // per commit. Fold the small ones together while we are here.
    let folded = repo.store.consolidate(16 * 1024 * 1024)?;
    if folded > 0 {
        println!("  consolidated {folded} small segment(s)");
    }

    let (on_disk, raw) = repo.store.packed_bytes();
    println!("  {} object(s) packed", repo.store.packed_count());
    if raw > 0 {
        println!(
            "  {} on disk, {} uncompressed ({:.1}x smaller)",
            human(on_disk),
            human(raw),
            raw as f64 / on_disk.max(1) as f64
        );
    }

    // Verify before claiming success: packing deletes the loose originals, so a
    // silent mistake here would be data loss.
    let report = fsck(&repo)?;
    if report.is_healthy() {
        println!("  verified: every object still hashes to its own name");
    } else {
        bail!("pack left the store damaged — the loose copies are gone, restore from a remote");
    }
    Ok(())
}

fn cmd_gc(args: &[String]) -> Result<()> {
    use fkit_core::gc;

    let repo = here()?;
    let mut opts = gc::Options::default();
    for a in args {
        match a.as_str() {
            "--dry-run" | "-n" => opts.dry_run = true,
            "--prune-all" => opts.min_age = std::time::Duration::ZERO,
            other => bail!("unknown option '{other}'"),
        }
    }

    // Roots: every branch, plus HEAD and any merge in progress. Missing
    // MERGE_HEAD here would collect the other side of a conflict the user is
    // still resolving.
    // all_refs, not list_refs: a tag is a root. A release tagged on a branch
    // that was since deleted is exactly the history someone still needs, and
    // collecting it would destroy the only thing pointing at it.
    let mut roots: Vec<Hash> = repo.all_refs()?.into_values().collect();
    if let Some(h) = repo.head_commit()? {
        roots.push(h);
    }
    if let Some(h) = repo.merge_head()? {
        roots.push(h);
    }

    if roots.is_empty() {
        println!("no branches — nothing is reachable, so nothing is collected");
        println!("(gc never deletes everything just because there are no refs)");
        return Ok(());
    }

    let r = gc::collect(&repo.store, &roots, opts)?;

    println!("{} object(s): {} reachable, {} not", r.total, r.reachable, r.unreachable);
    if r.too_young > 0 {
        println!(
            "  {} kept: newer than the {}h age guard (use --prune-all to override)",
            r.too_young,
            gc::DEFAULT_MIN_AGE.as_secs() / 3600
        );
    }
    if opts.dry_run {
        println!(
            "  would remove {} loose and {} packed object(s), reclaiming {}",
            r.loose_removed,
            r.packed_dropped,
            human(r.bytes_reclaimed)
        );
        println!("  (dry run — nothing changed)");
        return Ok(());
    }

    println!("  removed {} loose object(s)", r.loose_removed);
    if r.segments_compacted > 0 {
        println!(
            "  compacted {} segment(s), dropping {} packed object(s)",
            r.segments_compacted, r.packed_dropped
        );
    }
    println!("  reclaimed {}", human(r.bytes_reclaimed));

    // Deleting objects is the one operation that can quietly break a repository,
    // so prove it did not.
    let report = fsck(&repo)?;
    if report.is_healthy() {
        println!("  verified: every remaining object is intact and complete");
    } else {
        bail!("gc left the repository damaged — restore from a remote before pushing");
    }
    Ok(())
}

/// A `Write` sink that compares what is written to it against a reader.
///
/// `read_file` streams into any `Write`, which is the whole reason it takes one.
/// Handing it a `Vec<u8>` instead buffers the entire file — and doing that on
/// sixteen threads across a tree containing multi-gigabyte images is how you
/// exhaust a machine's memory. This holds one scratch buffer the size of the
/// current chunk, whatever the file's size.
struct CompareSink<R: std::io::Read> {
    src: R,
    scratch: Vec<u8>,
    offset: u64,
    /// Byte offset of the first difference, once one is found.
    diff: Option<u64>,
}

impl<R: std::io::Read> CompareSink<R> {
    fn new(src: R) -> Self {
        CompareSink { src, scratch: Vec::new(), offset: 0, diff: None }
    }

    /// Confirm the reader is exhausted — a disk file longer than the committed
    /// one differs even though every byte compared so far matched.
    fn finish(mut self) -> Option<u64> {
        if self.diff.is_some() {
            return self.diff;
        }
        let mut extra = [0u8; 1];
        match self.src.read(&mut extra) {
            Ok(0) => None,
            Ok(_) => Some(self.offset),
            Err(_) => Some(self.offset),
        }
    }
}

impl<R: std::io::Read> std::io::Write for CompareSink<R> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Once a difference is found there is nothing to learn from the rest;
        // keep accepting bytes so the producer finishes cleanly.
        if self.diff.is_some() {
            return Ok(buf.len());
        }
        if self.scratch.len() < buf.len() {
            self.scratch.resize(buf.len(), 0);
        }
        let want = &mut self.scratch[..buf.len()];

        let mut filled = 0;
        while filled < want.len() {
            match self.src.read(&mut want[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if filled < buf.len() {
            // Disk file ended early.
            self.diff = Some(self.offset + filled as u64);
            return Ok(buf.len());
        }
        if want != buf {
            let at = buf.iter().zip(want.iter()).position(|(a, b)| a != b).unwrap_or(0);
            self.diff = Some(self.offset + at as u64);
        }
        self.offset += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Compare the committed tree against a directory on disk, byte for byte.
///
/// This is the honest end-to-end check for a repository too large to check out:
/// content is streamed back out of the object store and compared to the
/// original in place, so verifying 150 GiB needs no free disk at all.
fn cmd_verify_tree(args: &[String]) -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let repo = here()?;
    let against = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| repo.root.clone());
    if !against.is_dir() {
        bail!("{} is not a directory", against.display());
    }

    let tip = repo.head_commit()?.context("no commits yet")?;
    let files = repo.walk_tree(tree_of(&repo, tip)?)?;
    let paths: Vec<(&String, &fkit_core::TreeEntry)> = files.iter().collect();

    println!(
        "comparing {} committed path(s) against {}",
        paths.len(),
        against.display()
    );

    // A working tree is live. Editors, build tools and language servers rewrite
    // files continuously, so "differs from the commit" and "is broken" are two
    // different findings and reporting them as one is useless. Anything whose
    // mtime is newer than the commit is the former.
    let commit_time = match repo.store.get(tip)? {
        Object::Commit(c) => c.timestamp,
        _ => 0,
    };

    let checked = AtomicUsize::new(0);
    let bytes = std::sync::atomic::AtomicU64::new(0);
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);

    let results: Vec<Vec<(bool, String)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (next, checked, bytes) = (&next, &checked, &bytes);
                let (paths, against, repo) = (&paths, &against, &repo);
                scope.spawn(move || {
                    // (changed_since_commit, message)
                    let mut bad: Vec<(bool, String)> = Vec::new();
                    let touched_after = |p: &std::path::Path| -> bool {
                        std::fs::symlink_metadata(p)
                            .and_then(|m| m.modified())
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64 > commit_time)
                            .unwrap_or(false)
                    };
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some((path, entry)) = paths.get(i) else { break };
                        let disk = against.join(path);

                        match entry.kind {
                            fkit_core::EntryKind::Symlink => {
                                let mut want = Vec::new();
                                if read_file(&repo.store, entry.hash, &mut want).is_err() {
                                    bad.push((false, format!("{path}: unreadable from the store")));
                                    continue;
                                }
                                match std::fs::read_link(&disk) {
                                    Ok(t) if t.to_string_lossy().as_bytes() == want.as_slice() => {}
                                    Ok(t) => bad.push((
                                        touched_after(&disk),
                                        format!(
                                            "{path}: symlink points at {:?}, committed {:?}",
                                            t,
                                            String::from_utf8_lossy(&want)
                                        ),
                                    )),
                                    Err(e) => bad.push((true, format!("{path}: {e}"))),
                                }
                            }
                            _ => {
                                // Stream both sides past each other; neither the
                                // committed content nor the file on disk is ever
                                // held whole in memory.
                                let file = match std::fs::File::open(&disk) {
                                    Ok(f) => std::io::BufReader::with_capacity(256 * 1024, f),
                                    Err(e) => {
                                        // Gone entirely: deleted since the commit.
                                        bad.push((true, format!("{path}: {e}")));
                                        continue;
                                    }
                                };
                                let mut sink = CompareSink::new(file);
                                if let Err(e) = read_file(&repo.store, entry.hash, &mut sink) {
                                    bad.push((false, format!("{path}: {e}")));
                                    continue;
                                }
                                bytes.fetch_add(entry.size, Ordering::Relaxed);
                                if let Some(at) = sink.finish() {
                                    bad.push((
                                        touched_after(&disk),
                                        format!("{path}: differs at byte {at}"),
                                    ));
                                }
                            }
                        }
                        checked.fetch_add(1, Ordering::Relaxed);
                    }
                    bad
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("verify worker panicked")).collect()
    });

    let all: Vec<(bool, String)> = results.into_iter().flatten().collect();
    let (stale, wrong): (Vec<_>, Vec<_>) = all.into_iter().partition(|(changed, _)| *changed);

    println!(
        "  compared {} path(s), {}",
        checked.load(Ordering::Relaxed),
        human(bytes.load(Ordering::Relaxed))
    );

    if !stale.is_empty() {
        println!(
            "\n  {} path(s) changed on disk after the commit (not a mismatch):",
            stale.len()
        );
        for (_, m) in stale.iter().take(10) {
            println!("    {m}");
        }
        if stale.len() > 10 {
            println!("    … and {} more", stale.len() - 10);
        }
    }

    if wrong.is_empty() {
        println!("\n  VERIFIED — every committed byte matches the original");
        Ok(())
    } else {
        println!("\n  {} GENUINE MISMATCH(ES):", wrong.len());
        for (_, m) in wrong.iter().take(25) {
            println!("    {m}");
        }
        if wrong.len() > 25 {
            println!("    … and {} more", wrong.len() - 25);
        }
        bail!("committed content does not match {}", against.display())
    }
}

fn cmd_fsck() -> Result<()> {
    let repo = here()?;
    let r = fsck(&repo)?;

    println!("checked {} object(s), {} on disk", r.checked, human(r.total_bytes));

    if !r.corrupt.is_empty() {
        println!("\nCORRUPT ({}):", r.corrupt.len());
        for (h, why) in &r.corrupt {
            println!("  {} — {why}", h.short());
        }
    }
    if !r.missing.is_empty() {
        println!("\nMISSING ({}):", r.missing.len());
        for (from, to) in r.missing.iter().take(20) {
            println!("  {} references absent {}", from.short(), to.short());
        }
    }
    if !r.unreachable.is_empty() {
        println!(
            "\n{} unreachable object(s) — not referenced by any branch (safe to garbage collect)",
            r.unreachable.len()
        );
    }
    if r.is_healthy() {
        println!("\nok — every object hashes to its own name");
    } else {
        bail!("repository is damaged");
    }
    Ok(())
}

fn cmd_stats() -> Result<()> {
    let repo = here()?;
    let ids = repo.store.iter_ids()?;

    let mut counts = [0usize; 8];
    // Object bytes as the store would hand them back, before compression.
    let mut object_bytes = 0u64;

    for id in &ids {
        let raw = repo.store.get_raw(*id)?;
        object_bytes += raw.len() as u64;
        if let Ok(obj) = fkit_core::store::Store::decode_framed(&raw) {
            counts[obj.kind().tag() as usize] += 1;
        }
    }

    // What the filesystem actually holds. Summing object sizes would ignore
    // both compression and the per-file slack that packing exists to remove,
    // and report a number that is wrong in two directions at once.
    let on_disk = dir_size(&repo.root.join(".fkit").join("objects"));

    // Logical size = what a naive checkout of every commit would occupy.
    let mut logical = 0u64;
    for (_, commit) in repo.all_refs()? {
        for (id, _) in repo.history(commit, usize::MAX)? {
            if let Ok(Object::Commit(c)) = repo.store.get(id) {
                let files = repo.walk_tree(c.tree)?;
                logical += files.values().map(|e| e.size).sum::<u64>();
            }
        }
    }

    println!("objects     {}", ids.len());
    println!("  chunks    {}", counts[1]);
    println!("  files     {}", counts[2]);
    println!("  trees     {}", counts[3]);
    println!("  commits   {}", counts[4]);
    println!("  entries   {}", counts[5]);
    println!();

    if repo.store.is_packed() {
        let (packed, raw) = repo.store.packed_bytes();
        println!("packed      {} object(s)", repo.store.packed_count());
        println!("  compressed {}", repo.store.packed_compressed());
        if raw > 0 {
            println!("  {} of object data stored in {}", human(raw), human(packed));
        }
    }
    let loose = repo.store.loose_ids()?.len();
    if loose > 0 {
        println!("loose       {loose} object(s) — run `fkit pack`");
    }
    println!();

    println!("on disk     {}", human(on_disk));
    println!("objects     {}  (uncompressed)", human(object_bytes));
    println!("logical     {}  (sum of every file in every commit)", human(logical));
    if on_disk > 0 && logical > on_disk {
        println!("saved       {:.1}x overall", logical as f64 / on_disk as f64);
    }
    Ok(())
}

/// Bytes a directory tree actually occupies.
fn dir_size(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            _ => e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .sum()
}

// ---- formatting ---------------------------------------------------------

fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Minimal UTC formatter — avoids pulling in `chrono` for one line of output.
fn fmt_time(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);

    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };

    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}
