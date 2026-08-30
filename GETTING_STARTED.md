# Getting started with fkit

Fifteen minutes, from nothing to a repository on a server. Every command below
is real and was run in this order.

If you already use git, most of this will be familiar. The places where fkit
differs are called out as they come up rather than saved for the end.

## Install

```sh
cargo install --path crates/fkit-cli     # the `fkit` command
```

Check it:

```sh
$ fkit --help
fkit — a content-addressed version control system
```

## Say who you are

Recorded on every commit you make. `--global` writes it once for every
repository on this machine.

```sh
fkit config --global author.name  "Ada Lovelace"
fkit config --global author.email "ada@example.com"
```

Two keys rather than one, deliberately: `author = "Name <email>"` requires
knowing a formatting convention, and getting it wrong bakes the mistake into
commits before anyone notices.

## Your first repository

```sh
mkdir hello && cd hello
fkit init .
```

Write something, and look before you commit:

```sh
$ echo 'print("hi")' > hello.py
$ fkit status
on branch main (no commits yet)

changes not yet committed:
  + hello.py  (12 B)

1 path(s) changed
```

`status` never writes to the object store. Hashing is a pure function of
content, so it computes exactly what a commit *would* store and then throws it
away — asking a question does not change anything.

```sh
$ fkit commit -m "the first thing"
[main a1c23f9d78] the first thing
  tree 07c7a49aba
  2 new object(s), 59 B written; 0 object(s) already stored (0 B deduplicated)
```

There is no staging area. `commit` snapshots the working tree as it is. Git's
index exists largely because hashing every file on every status check was too
slow in 2005; that is no longer true, so the extra concept is not worth the
confusion.

## What just happened

This is the part worth understanding, because everything else follows from it.

```sh
$ fkit log
commit a1c23f9d78ef94b45deb0ecfb7ab195099ecb8fda123436f5c26fe88dd8833e7
author  Ada Lovelace <ada@example.com>
date    2026-08-29 06:20:50 UTC
tree    07c7a49aba

    the first thing
```

A commit points at a tree; the tree points at files; a file points at chunks of
its own content. Every one of those arrows is a hash of what it points at. Walk
it:

```sh
fkit tree                  # what is in this commit
fkit show 07c7a49aba       # one object's structure
fkit merkle 07c7a49aba     # the whole shape, as a picture
```

Because a node's hash covers its children's hashes, one commit hash pins every
byte underneath it. That is what makes the next few things possible.

## Ignoring things

Same idea as `.gitignore`:

```sh
$ cat > .fkitignore <<'EOF'
# build output
target/
*.tmp
!keep.tmp
EOF
```

## Setting work aside

`switch` and `merge` both refuse to run over uncommitted changes, because they
rewrite tracked files and mixing that with your edits makes it impossible to
tell afterwards which change came from where. When you are not ready to commit:

```sh
fkit stash -m "half a parser"   # set it aside, working tree back at HEAD
fkit stash list
fkit stash pop                  # bring it back and forget it
fkit stash apply                # bring it back and keep it
fkit stash drop
```

A stash is an ordinary commit holding the working tree, parented on the HEAD it
was taken from. That parent is the useful part: bringing it back is a three-way
merge against the exact tree it was written on, so anything that landed
meanwhile is kept rather than reverted, and a genuine overlap gets the same
conflict markers `merge` produces.

A stash that comes back with conflicts is not dropped. Otherwise the only clean
copy of the work would be the file full of markers you are standing in.

Stashes are local. They are not pushed, and `fkit gc` treats them as roots, so
housekeeping cannot collect the one thing nothing else points at.

## Branches

```sh
fkit branch feature          # create it at HEAD
fkit switch feature          # move there, updating the working tree
fkit branch                  # list
```

Do some work, then merge it back:

```sh
$ fkit switch main
$ fkit merge feature -m "bring in the feature"
fast-forward main -> de7b2767f8
  1 written, 0 removed
```

A fast-forward says so and writes no merge commit — there is nothing to
combine when your history is entirely contained in theirs.

Trees merge with a single hash comparison wherever a subtree is unchanged on
one side, however large it is. Overlapping edits to the same file become
conflict markers, and a conflicted merge deliberately does **not** commit — the
merged tree lands in your working directory and `MERGE_HEAD` records the other
parent, so the commit you eventually make still records both.

## Tags

Tags do not move, which is the point of them:

```sh
fkit tag v1.0.0            # tag HEAD
fkit tag                   # list
fkit tag -f v1.0.0 <hash>  # move one anyway, when you mean it
```

### Moving one that is already published

Two commands, and neither of them touches anything you did not name:

```sh
fkit tag -f v1.0.0 <hash>           # move it here
fkit push --tag v1.0.0 --force      # move it on the server
```

`--tag` sends that tag and no branch, so repointing a label never means
force-pushing your work as well. Without `--force` the server refuses and says
so, because moving a published tag changes what a name means for everyone who
already has it.

They find out on their next `fkit pull`, which brings tags into line with the
server and prints what changed:

```
  tag v1.0.0 moved 8770ed31f9 -> 8f42675eb3
```

This is the part git leaves out: `git fetch` will not update a tag it already
has, so everyone who cloned before the move keeps resolving the name to the old
commit and nothing tells them. Pass `--no-tags` to opt out. A tag that exists
only on your machine is yours and is never touched.

If your project is a Go module, tags are what `go get` reads — see
[Go modules](#go-modules) below. Moving a published one there is worth thinking
twice about: the Go module proxy records a checksum per version, and a version
that changes underneath it is a verification failure for anyone who fetched the
old one.

## Putting it on a server

Start a hub, or use one someone else runs:

```sh
make up          # generates .env with random secrets, builds, starts
```

Open <http://localhost:7500>. **The first account you register becomes the
server administrator**, so register yours before anyone else does.

Then create a repository in the web UI, and point your local one at it:

```sh
fkit remote ws://localhost:7500/ada/hello
```

You need a token to push. Settings → Access tokens → generate one with **allow
push** ticked, then either:

```sh
fkit config --global token fkit_pat_...   # every repository on this machine
fkit config token fkit_pat_...            # just this one, and it wins
export FKIT_TOKEN=fkit_pat_...            # this shell only, and it wins over both
```

One token for a whole server is usually what you want, so set it globally and
forget it. A repository-level token overrides the global one, and `FKIT_TOKEN`
overrides both — useful in CI, where the token should not be written to disk at
all.

A read-only token cannot push, even to your own repositories — narrowing is the
only thing a token can do.

### Whose commits are these

A commit's author name and email are just text you typed; anyone can put anyone
in there. So the hub does not attribute commits by matching that email against
accounts. It records who *pushed* them, which it knows for certain, because the
push was authenticated by your token.

Your account link therefore follows your token, not your `author.name`. Change
your author string, push from a different machine, use a different email — the
commits still land on your profile. The author string is still shown exactly as
written; the account link sits beside it.

Untick **link commits to my account** when generating a token if that token is
going to push somebody else's history — a mirror of a GitHub repository, say.
Commits pushed with it stay flat: author string only, no account behind it.

```sh
$ fkit push
pushing main (de7b2767f8) to ws://localhost:7500/ada/hello
  sent 15 object(s), 1005 B in 6 round trip(s)
  main -> de7b2767f8 (15 objects received)
  1 tag(s) already current
```

Fifteen objects, because that is all it had to send. No inventory is exchanged: the
server names what it is missing, and an unchanged directory is one hash
comparison whose entire subtree is skipped — not transferred, not enumerated,
not even named.

## Getting it back

```sh
$ fkit clone ws://localhost:7500/ada/hello
cloning ws://localhost:7500/ada/hello into hello
  branch main: 15 object(s)
  tag v1.0.0: 0 object(s)
received 15 object(s), 1005 B in 6 round trip(s)
checked out main at de7b2767f8 (2 file(s))

$ cd hello && fkit pull
main is now at de7b2767f8
```

Tags come with it. The tag cost nothing to transfer, because it names a commit
that had already arrived.

## Depending on another repository

```sh
fkit submodule add ws://localhost:7500/ada/loom vendor/loom
fkit commit -m "vendor loom"
```

The pinned revision lives inside your commit's hash, so a commit names one
complete state. Practically, that means:

```sh
fkit checkout <an older commit>   # the submodule follows, in the same command
```

No `--recursive` on clone, no second command after checkout, and pushing a
commit that pins a revision you never pushed is not possible — the objects
travel with the commit that references them.

To move the pin:

```sh
fkit submodule update      # to the remote's tip
fkit submodule             # what is mounted, and whether it is current
```

## Go modules

A repository on a hub can be `go get`-ed without git being involved:

```sh
go env -w GOPRIVATE=your-host.example/*
go get your-host.example/ada/hello@v1.0.0
```

`GOPRIVATE` matters: without it the toolchain checks every module against
`sum.golang.org`, which has never heard of your server, and the failure looks
like the server is broken.

Untagged repositories work too — `@main` resolves to a pseudo-version naming
the commit.

## Housekeeping

```sh
fkit pack        # fold loose objects into packed segments
fkit gc          # delete objects no branch can reach
fkit fsck        # re-hash every object and check it against its own name
fkit stats       # what is stored, and what deduplication saved
```

```sh
$ fkit gc --dry-run
15 object(s): 15 reachable, 0 not
  would remove 0 loose and 0 packed object(s), reclaiming 0 B
  (dry run — nothing changed)
```

`gc` keeps anything younger than 24 hours even if it is unreachable. Nothing in
a content-addressed store records intent, so between "write the objects" and
"move the ref" a push's objects are unreachable by definition — time is the
only honest defence. `--prune-all` skips the guard when you know nothing else
is writing.

## Proving something without the repository

```sh
$ fkit prove hello.py -o hello.proof
proof for hello.py
  root    de7b2767f84575aa1a4f547c4ab0b55fbb731ee16e58cf0da9f1a05b5db349fa
  target  89f5576965
  size    26 B
  steps   3 nodes, 487 B
  written to hello.proof

$ cd /somewhere/with/no/repository
$ fkit verify hello.proof --root de7b2767f845…
verified
  hello.py (26 B) is in de7b2767f8
  content hash 89f5576965
```

A few hundred bytes prove one file belongs to a commit hash you already trust,
with no access to the repository at all. Git cannot do this: its directories are
flat lists, so a proof would have to carry every filename in every directory
along the path.

## Where to go next

- [README](README.md) — how it works, and the measurements behind the claims
- [DEPLOY.md](DEPLOY.md) — running a hub properly
- `fkit <command> --help` — every command takes it

If something here did not work, that is a bug in this guide. Open an issue with
the command you ran and what it printed.
