//! The command surface, as data.
//!
//! This was a hand-rolled `match` over `&[String]` with a per-command loop that
//! walked flags by index. That works, and it was 300 lines of the same shape
//! written twenty-seven times, with no `--version`, no per-command `--help`,
//! and a different error message for every way of getting an argument wrong.
//!
//! One thing is kept from the old version deliberately. clap 4 has no way to
//! group subcommands in its help, and a flat list of twenty-seven is a worse
//! first page than the grouped one it replaced — so the top-level overview is
//! still written out below, and `every_command_appears_in_the_overview` fails
//! if a command is ever added without appearing in it. Hand-written, but not
//! hand-maintained.

use clap::{Args, Parser, Subcommand};

/// The grouped overview. `{options}` is left to clap so `--help` and
/// `--version` cannot drift out of it.
const OVERVIEW: &str = "\
fkit — a content-addressed version control system

USAGE:
    fkit <command> [args]
    fkit help <command>      what one command takes

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
    stash [-m <message>]     set changes aside and go back to HEAD
    stash list               what has been set aside
    stash pop [<n>]          restore the newest, or <n>, and drop it
    stash apply [<n>]        restore without dropping
    stash drop [<n>]         discard one
    stash push [<n>]         send one to the remote so it follows you
    stash remote             what is parked on the remote
    stash fetch [<hash>]     bring parked work back to this machine

BRANCHES
    branch [<name>]          list branches, or create one at HEAD
    tag [<name>] [<commit>]  list tags, or tag a commit; tags do not move
    switch <name>            move HEAD to a branch and update the working tree
    merge <branch>           merge another branch into the current one
    checkout <commit>        move HEAD to a specific commit (detached)

SUBMODULES
    submodule                list mounted submodules and their state
    submodule add <url> <path>
                             mount another repository at <path>
    submodule update [<path>]
                             move the pin to the remote's current tip
    submodule fetch [<path>] retrieve content for a pin this store lacks
    submodule rm <path>      unmount; the next commit will not carry it
    submodule set-remote <path> <url>
                             fetch this submodule from somewhere else

REMOTES
    remote [<url>]           show or set the remote (ws://host:7420/repo)
    clone <url> [<dir>]      copy a remote repository
    push [<branch>]          send commits and tags to the remote
    pull [<branch>]          fetch commits from the remote and update

INSPECTION
    show <hash>              print an object's structure
    tree [<commit>]          list the files in a commit
    cat <path> [<commit>]    print a file's contents
    merkle <hash>            visualise an object's Merkle tree
    prove <path> [<commit>]  emit a proof that a path is in a commit
    verify <file> --root <h> check a proof against a commit hash you trust
    pack                     fold loose objects into packed segments
    gc                       delete objects no branch can reach
    verify-tree <dir>        compare HEAD's tree against a directory, byte for byte
    fsck                     verify every object and report unreachable ones
    stats                    storage and deduplication statistics

{options}";

#[derive(Parser, Debug)]
#[command(
    name = "fkit",
    version,
    about = "a content-addressed version control system",
    help_template = OVERVIEW,
    // Every subcommand gets clap's ordinary help; only the top level is
    // replaced, because only the top level has something clap cannot express.
    subcommand_help_heading = "COMMANDS",
    disable_help_subcommand = false,
    arg_required_else_help = false
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a repository.
    Init {
        /// Where to create it. Defaults to the current directory.
        path: Option<String>,
    },

    /// Read or set a config value (author.name, author.email, remote, token).
    Config {
        /// Write to the user-level file rather than this repository.
        #[arg(short, long)]
        global: bool,
        /// Show every effective value and where each came from.
        #[arg(short, long, conflicts_with_all = ["key", "value"])]
        list: bool,
        key: Option<String>,
        value: Option<String>,
    },

    /// Show what changed since the last commit.
    Status,

    /// Snapshot the working tree.
    Commit {
        #[arg(short, long)]
        message: Option<String>,
        /// Record a different author. For importers replaying a history.
        #[arg(long)]
        author: Option<String>,
        /// Record a different timestamp, as unix seconds or RFC 3339.
        #[arg(long)]
        date: Option<String>,
    },

    /// Show commit history.
    Log {
        /// How many commits to show.
        #[arg(short = 'n', long = "max-count")]
        count: Option<usize>,
    },

    /// Show what changed. Defaults to HEAD against the working tree.
    Diff {
        /// List changed paths only, without line content.
        #[arg(long)]
        stat: bool,
        /// Lines of context around each change. Not implemented — the
        /// grouping uses the library default, and saying so beats a knob that
        /// is accepted and ignored.
        #[arg(short = 'U', long)]
        unified: Option<usize>,
        a: Option<String>,
        b: Option<String>,
    },

    /// List branches, or create one at HEAD.
    Branch {
        /// Delete a branch.
        #[arg(short = 'd', long = "delete")]
        delete: bool,
        name: Option<String>,
    },

    /// List tags, or tag a commit. A tag does not move.
    Tag {
        /// Delete a tag.
        #[arg(short = 'd', long = "delete", conflicts_with = "force")]
        delete: bool,
        /// Move a tag that already exists.
        #[arg(short, long)]
        force: bool,
        name: Option<String>,
        commit: Option<String>,
    },

    /// Move HEAD to a branch and update the working tree.
    Switch {
        name: String,
        /// Discard uncommitted changes.
        #[arg(short, long)]
        force: bool,
    },

    /// Set uncommitted changes aside, or bring them back.
    Stash {
        #[command(subcommand)]
        command: Option<StashCommand>,
        /// What the changes are, for the list.
        #[arg(short, long)]
        message: Option<String>,
    },

    /// Merge another branch into the current one.
    Merge {
        branch: String,
        #[arg(short, long)]
        message: Option<String>,
    },

    /// Move HEAD to a specific commit, detached.
    Checkout {
        commit: String,
        /// Discard uncommitted changes.
        #[arg(short, long)]
        force: bool,
    },

    /// Print an object's structure.
    Show { hash: String },

    /// List the files in a commit.
    Tree { commit: Option<String> },

    /// Print a file's contents.
    Cat {
        path: String,
        commit: Option<String>,
    },

    /// Visualise an object's Merkle tree.
    Merkle { hash: String },

    /// Another repository, pinned at an exact revision.
    Submodule {
        #[command(subcommand)]
        command: Option<SubmoduleCommand>,
    },

    /// Show or set the remote.
    Remote { url: Option<String> },

    /// Copy a remote repository.
    Clone {
        url: String,
        dir: Option<String>,
        /// Fetch objects and refs, write no files.
        #[arg(long, alias = "bare")]
        no_checkout: bool,
    },

    /// Send commits and tags to the remote.
    Push {
        branch: Option<String>,
        /// Move the remote ref even if it is not a fast-forward.
        #[arg(short, long)]
        force: bool,
        /// Send commits only.
        #[arg(long, conflicts_with = "tag")]
        no_tags: bool,
        /// Push only this tag, and no branch. Repeatable.
        #[arg(long = "tag", value_name = "NAME")]
        tag: Vec<String>,
    },

    /// Fetch commits from the remote and update.
    Pull {
        branch: Option<String>,
        /// Leave local tags alone, even where the remote's have moved.
        #[arg(long)]
        no_tags: bool,
    },

    /// Emit a proof that a path is in a commit.
    Prove {
        path: String,
        commit: Option<String>,
        /// Write to a file instead of stdout.
        #[arg(short, long)]
        output: Option<String>,
    },

    /// Check a proof against a commit hash you already trust.
    Verify {
        file: String,
        /// The commit hash to check against.
        #[arg(short, long)]
        root: String,
    },

    /// Fold loose objects into packed segments.
    Pack,

    /// Delete objects no branch can reach.
    Gc(GcArgs),

    /// Compare HEAD's tree against a directory, byte for byte.
    VerifyTree {
        /// Defaults to the repository root.
        dir: Option<String>,
    },

    /// Verify every object and report unreachable ones.
    Fsck,

    /// Storage and deduplication statistics.
    Stats,
}

#[derive(Args, Debug)]
pub struct GcArgs {
    /// Report what would go; change nothing.
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    /// Ignore the age guard. Nothing else may be writing.
    #[arg(long)]
    pub prune_all: bool,
}

/// What can be done with set-aside work.
#[derive(Subcommand, Debug)]
pub enum StashCommand {
    /// Show what has been set aside, newest first.
    List,
    /// Restore a stash and drop it.
    Pop { which: Option<usize> },
    /// Restore a stash, keeping it in the list.
    Apply { which: Option<usize> },
    /// Discard a stash without restoring it.
    Drop { which: Option<usize> },

    /// Send a stash to the remote, so it can be picked up elsewhere.
    ///
    /// Never automatic: `fkit push` sends commits and tags, and unfinished
    /// work is not something to upload without being asked.
    Push { which: Option<usize> },

    /// List what this account has parked on the remote.
    Remote,

    /// Bring one back. Without a hash, everything the remote is holding.
    Fetch { commit: Option<String> },

    /// Remove one from the remote. The local copy is untouched.
    Forget { commit: String },
}

#[derive(Subcommand, Debug)]
pub enum SubmoduleCommand {
    /// List mounted submodules and their state.
    List,
    /// Mount another repository at a path.
    Add {
        url: String,
        path: String,
        /// Which branch of the remote to pin. Defaults to main.
        #[arg(long)]
        branch: Option<String>,
    },
    /// Move the pin to the remote's current tip.
    Update {
        path: Option<String>,
        #[arg(long)]
        branch: Option<String>,
    },
    /// Retrieve content for a pin this store lacks.
    Fetch { path: Option<String> },
    /// Unmount; the next commit will not carry it.
    #[command(alias = "remove")]
    Rm { path: String },
    /// Fetch this submodule from somewhere else, on this machine only.
    SetRemote { path: String, url: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// The overview is written by hand because clap cannot group subcommands.
    /// This is what stops it going stale: add a command without listing it and
    /// the build fails here rather than the command being invisible.
    #[test]
    fn every_command_appears_in_the_overview() {
        let missing: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .filter(|n| n != "help")
            .filter(|n| !OVERVIEW.contains(n.as_str()))
            .collect();
        assert!(missing.is_empty(), "not in the overview: {missing:?}");
    }

    /// Conversely, the overview must not promise something that is gone.
    #[test]
    fn the_overview_promises_nothing_that_does_not_exist() {
        let names: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();

        // The command word is the first on a line indented by four spaces.
        for line in OVERVIEW.lines() {
            let Some(rest) = line.strip_prefix("    ") else { continue };
            if rest.starts_with(char::is_whitespace) {
                continue;
            }
            let Some(word) = rest.split_whitespace().next() else { continue };
            if word == "fkit" {
                continue;
            }
            assert!(names.iter().any(|n| n == word), "{word} is listed but does not exist");
        }
    }

    #[test]
    fn the_surface_that_existed_before_still_parses() {
        // Every spelling the hand-rolled parser accepted, so a script written
        // against it keeps working.
        let ok = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("fkit").chain(args.iter().copied()))
                .unwrap_or_else(|e| panic!("{args:?} should parse: {e}"))
        };

        ok(&["init"]);
        ok(&["init", "some/dir"]);
        ok(&["config", "--list"]);
        ok(&["config", "-l"]);
        ok(&["config", "--global", "author.name", "Ada"]);
        ok(&["config", "-g", "token", "x"]);
        ok(&["commit", "-m", "hi"]);
        ok(&["commit", "--message", "hi", "--author", "A <a@b>", "--date", "1700000000"]);
        ok(&["log", "-n", "5"]);
        ok(&["diff", "--stat"]);
        ok(&["diff", "-U", "7", "a", "b"]);
        ok(&["branch"]);
        ok(&["branch", "-d", "old"]);
        ok(&["tag", "-f", "v1", "abc"]);
        ok(&["tag", "-d", "v1"]);
        ok(&["switch", "main"]);
        ok(&["switch", "main", "--force"]);
        ok(&["switch", "-f", "main"]);
        ok(&["merge", "feature", "-m", "msg"]);
        ok(&["checkout", "abc", "--force"]);
        ok(&["checkout", "-f", "abc"]);
        ok(&["push", "-f"]);
        ok(&["clone", "ws://h/r", "dir", "--no-checkout"]);
        ok(&["clone", "ws://h/r", "--bare"]);
        ok(&["push", "main", "--force", "--no-tags"]);
        ok(&["pull"]);
        ok(&["prove", "a.txt", "-o", "p.bin"]);
        ok(&["verify", "p.bin", "--root", "abc"]);
        ok(&["gc", "--dry-run"]);
        ok(&["gc", "--prune-all"]);
        ok(&["verify-tree", "some/dir"]);
        ok(&["verify-tree"]);
        ok(&["verify", "p.bin", "-r", "abc"]);
        ok(&["gc", "-n"]);
        ok(&["submodule"]);
        ok(&["submodule", "add", "ws://h/r", "vendor/x"]);
        ok(&["submodule", "add", "ws://h/r", "vendor/x", "--branch", "main"]);
        ok(&["submodule", "update"]);
        ok(&["submodule", "rm", "vendor/x"]);
        ok(&["submodule", "remove", "vendor/x"]);
        ok(&["submodule", "set-remote", "vendor/x", "ws://h/r"]);
        ok(&["fsck"]);
        ok(&["stats"]);
        ok(&["pack"]);
    }

    #[test]
    fn a_required_argument_that_is_missing_is_an_error_not_a_panic() {
        for bad in [
            vec!["switch"],
            vec!["verify", "p.bin"], // --root is required
            vec!["submodule", "add", "ws://h/r"],
            vec!["cat"],
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("fkit").chain(bad.iter().copied())).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn clap_verifies_its_own_definition() {
        Cli::command().debug_assert();
    }
}
