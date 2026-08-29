# fkit

A content-addressed store with git-shaped commands on top, for the repositories
git handles badly: the ones with large files in them.

Change one byte of a 12 MB file and it writes **4 objects, 18.5 KiB** — because
a file is a Merkle tree over content-defined chunks, not one opaque blob. Point
it at a 154 GiB tree of build output and disk images and it stores **1.2 GB**,
128× smaller, almost entirely from noticing the same bytes twice.

It is honest about the other direction too: for ordinary source history, git's
delta compression still wins, and by how much is [measured
below](#not-done-yet).

```
fkit init                 fkit commit -m "..."      fkit push
fkit status               fkit log                  fkit pull
fkit switch <branch>      fkit merkle <hash>        fkit clone ws://host/repo
fkit submodule add <url> <path>                     fkit gc
```

## What makes it different from git

Git is *almost* content-addressed: objects are named by hash, but a file is
stored as one whole blob. Change one byte of a 400 MB file and git writes a new
400 MB object, clawing the space back later with delta compression during `gc`.

fkit splits files with **content-defined chunking** before hashing, so a file is
itself a Merkle tree over its chunks:

```
bigdata.bin  (12 MB)
└── file node  level 1
    ├── file node  level 0 ──┬── chunk a3f2…  8.9 KiB
    │                        ├── chunk 91bc…  10.5 KiB
    │                        └── …256 chunks
    └── file node  level 0 ──┴── …
```

Measured on a 12 MB file in this repo's own test suite:

| change | objects written | bytes written |
|---|---|---|
| initial commit | 1582 | 11.5 MiB |
| flip one byte mid-file | **4** | **18.5 KiB** |
| prepend 2 MB at the front | 272 | 2.0 MiB |

The prepend is the interesting one. Every byte offset in the file shifts, yet
only the genuinely new data is stored — because chunk boundaries are chosen by a
rolling hash of the content, not by position. Fixed-size chunking loses ~100% of
reuse on that edit; there is a test asserting exactly that contrast.

## On a real repository

Measured on an operating-system project: 100,484 files across 3,337 directories,
including build trees, vendored dependencies and disk images.

| | |
|---|---|
| logical content read | **154.5 GiB** |
| ingest time | **2 min 09 s** (~1.2 GiB/s, 16 cores) |
| unique objects stored | 310,374 |
| duplicate references | 3,204,369 |
| **store on disk** | **1.2 GB** — 128× smaller |

`du` reports that tree as 9.4 GB because it counts a hard-linked or sparse inode
once. There are 154.5 GiB of bytes reachable through paths, and fkit read every
one of them. Almost all of the reduction is deduplication rather than
compression: build output is the same bytes over and over, and a
content-addressed store notices.

Getting there required fixing two things that only show up at this scale — a
buffer compaction in the chunker that was memmoving 64 KB per 8 KB produced, and
ingest running on one core. Both are described in `chunker.rs` and `ingest.rs`.

## The five object types

Everything in the repository is one of these, and every arrow between them is a
hash rather than a pointer:

```
Commit ──tree──> Tree ──> Entries ──entry──> Tree ──> … ──> File ──> Chunk
  │              (levels)  (a run of          (subdir)      (levels)  (bytes)
  │                         directory entries)
  └──parent──> Commit ──> …
```

A directory entry names a file, a subdirectory, a symlink, or a **submodule** —
in which case the hash it carries is a `Commit` of another repository. That is
the whole of the submodule design, and the section below is about why putting
it there rather than beside the repository is what makes it work.

Note the symmetry: **`Tree`/`Entries` is to a directory exactly what
`File`/`Chunk` is to a file.** Both are interior nodes over content-defined runs
of leaves, and both are cut by hashing the content rather than counting
positions. A directory is *not* a flat list — that is git's design, and it means
adding one file to a directory of 100 000 rewrites all of it. Here it rewrites
one run:

| directory of 4000 files | bytes of listing rewritten |
|---|---|
| flat tree (git-shaped) | the whole listing |
| chunked runs (fkit) | **< 40 KB** |

Because a node's hash covers its children's hashes, one root hash pins every
byte beneath it. That single property buys:

- **Integrity** — `fkit fsck` re-hashes every object and compares it to its own
  name. No external checksum, signature, or trusted party involved.
- **Proofs** — `fkit prove <path>` emits a compact inclusion proof; anyone can
  check that a file belongs to a commit hash **without the repository**. See
  below.
- **Cheap diffs** — equal subtree hashes are provably identical, so `fkit diff`
  skips them without reading a file.
- **Cheap sync** — see below.
- **Safe transfer** — a peer cannot substitute content, because the receiver
  recomputes the hash of everything it is given.

## Proofs

Because a node's hash covers its children's hashes, you can prove a file belongs
to a commit by shipping only the *siblings* along the path from that file to the
root. The verifier recomputes each parent hash and compares the result against a
commit hash they already trust.

```sh
$ fkit prove src/lib.rs -o lib.proof
proof for src/lib.rs
  root    750cfc446cf898ee4ea1133fa6d2d52d36a8805e937be580cc0cf6249be1b0ec
  steps   5 nodes, 605 B

$ cd /somewhere/else          # no repository here
$ fkit verify lib.proof --root 750cfc446cf8...
verified
  src/lib.rs (19 B) is in 750cfc446c
```

**605 bytes** to prove one path in a 3002-file repository. Tampering with any
step, splicing a valid node from another tree, or verifying against the wrong
root are all rejected — the chain, not just each link, is checked.

Git cannot offer this: its trees are flat, so a proof would have to carry every
sibling name in every directory along the path.

## Storage

Objects live in append-only segments with a small index, not one file each:

```sh
$ fkit pack
packing 6026 loose object(s)…
  moved 6026 object(s), 340.0 KiB into segments
  verified: every object still hashes to its own name
```

| 3002-file repository | files on disk | disk used |
|---|---|---|
| loose objects (git-shaped) | 6026 | 24 MB |
| packed segments | **2** | **648 KB** |

Content-defined chunking produces many small objects, and one inode each means
most of them occupy a 4 KiB block to hold a few hundred bytes. Each writer owns
its own segment, so concurrent writers never contend and there is nothing to
lock. A torn index entry from a crash is detected by size and ignored — the
object is simply absent and gets rewritten, never wrongly located.

Objects are zstd-compressed **individually**, so any one is still a single seek,
and the compressed form is kept only when it actually helps — a chunk of random
or already-compressed data would otherwise grow by a few bytes and cost CPU on
every read:

```
$ fkit stats
packed      127 object(s)
  compressed 83
  546.6 KiB of object data stored in 25.2 KiB
```

Compression is a cargo feature. `--no-default-features` keeps `fkit-core` at
exactly blake3 + anyhow.

### Caching decompressed objects

Rendering a page walks the same tree nodes over and over, decompressing each
one every time. A cache in front of the store fixes that, and content
addressing makes it unusually easy: **a hash names one byte sequence forever,
so a cached object can never be stale.** There is no invalidation problem to
get wrong — only eviction, which is a size and a TTL.

```
$ cargo run --release --example cachebench -- .fkit/objects <commit>
objects walked: 1889
uncached  221.35ms for 20 walks
cached     10.56ms for 20 walks
speedup   21.0x
```

`ObjectCache` is a trait. `MemoryCache` is LRU by bytes with a TTL,
`RedisCache` is behind the `redis-cache` feature, and `Tiered` composes them —
memory is always the near tier, because a round trip to Redis costs more than
reading the object off local disk. `NoCache` is the default in the library, so
nothing pays for a cache it did not ask for.

## Garbage collection

```sh
fkit gc --dry-run     # what would go
fkit gc               # collect, keeping anything younger than 24h
fkit gc --prune-all   # ignore the age guard
```

An object is garbage when no ref reaches it. The subtlety is not finding them —
that is a graph walk — but *not deleting one that is about to become reachable*.
A push writes objects and then moves a ref; in between, its objects are
unreachable by definition. Nothing in a content-addressed store records intent,
so the only honest defence is time, and unreachable objects younger than the age
guard are never collected.

Packed objects cannot be unlinked individually — they are bytes inside a shared
segment — so segments are **compacted**: survivors are written and fsynced into a
new segment before any original is removed, which means a crash leaves harmless
duplicates and never a gap. A segment is only rewritten when enough of it is
actually dead to be worth the I/O.

`fkit pack` also folds small segments together, since every writing process
creates its own.

## Merging

```sh
fkit merge feature            # three-way merge, or conflict markers + MERGE_HEAD
```

Merge base is the best common ancestor over the commit DAG. Trees merge with a
single hash comparison wherever a subtree is unchanged on one side, however large
it is. Files that both sides edited go through a diff3 line merge; overlapping
edits become conflict markers.

A conflicted merge deliberately does **not** commit. The merged tree lands in the
working directory and `MERGE_HEAD` records the other parent, so the eventual
`fkit commit` still records two parents rather than pretending the branches never
met.

## Submodules

Another repository, pinned at an exact revision:

```sh
fkit submodule add wss://host/alice/loom vendor/loom
fkit submodule                 # what is mounted, and whether it is current
fkit submodule update          # move the pin to the remote's tip
```

The pin is a tree entry, so it is **inside the parent commit's hash**. Git
splits that one fact across three places — the revision in a gitlink, the URL
in a tracked `.gitmodules`, the effective URL in `.git/config` — which is why
`git submodule sync` and `git submodule init` have to exist at all. Here a
commit names one complete state, submodules included.

Because the pin is an ordinary link in the object graph, everything that walks
links already handles it. None of these needed a line of code:

| | |
|---|---|
| `push` | sends the submodule's content with the commit that references it, so committing a pin you never pushed is not possible |
| `gc` | keeps it, having no way to consider it garbage |
| `fsck` | checks it along with everything else |
| `clone` | brings it down complete — there is no `--recursive` |

Checkout is the case git gets worst: `git checkout` of an older commit leaves
submodules where they were, silently, unless you remember a flag. Here
`walk_tree` expands a pin into its content, so checkout, archive and diff treat
a submodule as ordinary files without knowing it exists — and the objects are
already local, so nothing has to be fetched to make one checkout move
everything.

What is left beside the repository is only what is genuinely not content:
where to fetch from. A project may suggest one in a tracked `.fkit-submodules`,

```
vendor/loom = wss://host/alice/loom@<64 hex>
```

and the revision after the `@` makes a submodule bump an ordinary one-line
diff — a review can see what it moved to, which `.gitmodules` plus a gitlink
cannot show. The commit is still what pins; that line is rewritten whenever the
pin moves, and `fkit submodule` reports it as stale if the two ever disagree.
A relative url (`../loom`) resolves against the repository's own remote, so a
fork on another host fetches from that host rather than sending everyone back
to the original.

Developing a dependency in place is deliberately not supported. A submodule
here is content, not a nested working repository, so git's detached-HEAD
work-eater has nowhere to happen; clone it on its own to work on it.

## Sync

`fkit push` and `fkit pull` speak a small binary protocol over a WebSocket. The
negotiation is four lines of idea:

1. Ask for the tip commit.
2. For each object received, look at the hashes it references.
3. Request only the ones you don't already have.
4. Repeat until nothing is missing.

No inventory is ever exchanged. An unchanged directory is a single hash
comparison and its entire subtree is skipped — not transferred, not enumerated,
not even named. In this repo's tests, a two-file change to a 3 MB repository
syncs as 7 objects and 549 bytes.

## Hosting

There are two servers. They speak the **same protocol** — the session loop lives
once in `fkit-core::session` and each supplies a `RepoHost` for where refs live
and who is allowed to move them.

### `fkit-hub` — the forge

Accounts, per-repo permissions, a web UI, and Postgres-backed refs. The web page
and the sync socket share one port and one URL: `https://hub/you/proj` in a
browser and `ws://hub/you/proj` from the CLI are the same repository, told apart
by the `Upgrade` header.

It carries the usual forge furniture — issues, merge requests, labels, review
comments — with a few places where content addressing changes what is possible:

**Forks share an object store.** A fork records the root of its fork network
and uses that network's store, so forking is O(1) on disk however large the
repository, and a merge request across two forks needs no transfer at all
because both sides' commits already resolve in the same store. Sharing a store
between repositories is safe by construction rather than by convention: an
object's name *is* a digest of its bytes, so two repositories cannot disagree
about what a hash means. Garbage collection is then a network-wide question —
every ref of every repository sharing the store is a root — and runs under a
Postgres advisory lock so two collections cannot overlap.

**Review comments are anchored to content, not to a line number.** A diff is
recomputed live from two branches, so a comment pinned to "line 42 of commit
abc" slides onto an unrelated line the moment anyone pushes. Pinned to the hash
of the file it was written against, it stays where it was put through a rebase,
an amend, or ten unrelated commits — and when the file does change, the comment
is not silently wrong: its blob is absent from the new diff, which is what
"outdated" means and can be shown as such. Unresolved threads block the merge.

**An issue can point at the exact lines it is about.** Select lines while
reading a file and open an issue from them; the issue carries the blob hash and
renders that code on its own page, still correct after the file has been
renamed or rewritten. GitHub offers a permalink to paste into the body, which
names a commit and two line numbers and quietly points at different code the
moment anything above it is edited.

Branches and tags can be deleted, with the guards that implies — not the
default branch, and not one an open merge request still needs — and a merge can
delete the branch it came from, since merging makes those commits ancestors of
the target and removing the branch discards nothing.

```sh
make up          # generates .env with random secrets, then builds and starts
```

Then open <http://localhost:7500>. The first account you register becomes the
server administrator.

```sh
# on the client
fkit remote ws://your-host:7500/you/my-repo
export FKIT_TOKEN=fkit_pat_...      # created under Settings → access tokens
fkit push
```

Configuration is layered — `defaults < fkit-hub.toml < environment < flags`:

```sh
fkit-hub --print-config-template > fkit-hub.toml
```

| knob | effect |
|---|---|
| `server.open_registration` | turn off public sign-up (the first admin account is still allowed) |
| `server.require_auth` | require a login for *everything*, including public repos |
| `server.default_repo_visibility` | what a new repository gets when unspecified |
| `server.secure_cookies` | set behind TLS; **leave off for plain HTTP**, or the browser drops the cookie and login appears to fail silently |

Per-repository visibility is independent of all that: a public instance can host
private repositories, and a public repository on it is readable — and cloneable —
by someone with no account at all, while still refusing their pushes.

### `fkitd` — the minimal daemon

No accounts, no UI, refs as files, one optional shared token. Good for a private
box or CI.

```sh
FKIT_TOKEN=your-secret fkitd --listen 0.0.0.0:7420 --data /var/lib/fkit
```

It **refuses to start** on a non-loopback address without a token unless you pass
`--insecure-no-auth`. A warning in a container log is not a safety mechanism.

### Security notes

* Passwords are Argon2id. Session cookies and access tokens are 256-bit random
  values stored as a BLAKE3 digest — a slow KDF on a high-entropy token buys
  nothing and costs ~15 ms on every request.
* A repository you cannot see returns **404, not 403**, everywhere including the
  sync endpoint, so error codes cannot be used to enumerate private repos.
* Ref updates are fast-forward-only unless forced. In the hub the check and the
  write share a transaction with the ref row locked; `fkitd` can only manage a
  per-process lock, which is the honest limit of file-backed refs.
* Server administrators can read every repository. That is deliberate — it is
  what "administrator" means for operations — so it is disclosed in the UI on
  every such view rather than only in an audit table, and recorded in one.
* `server.trust_proxy` must stay **off** on a directly-exposed server. The
  header it believes is client-supplied, so trusting it there lets anyone mint
  a new identity per request and skip rate limiting entirely.
* There is no TLS. Put the hub behind a terminating proxy and set
  `secure_cookies`.

## Layout

```
crates/
  fkit-core/     the engine — read the modules in this order:
    hash.rs        how things are named (BLAKE3)
    object.rs      the four node types and their canonical encoding
    chunker.rs     content-defined chunking, and why fixed-size fails
    store.rs       the CAS on disk (loose + packed)
    pack.rs        append-only segments, their index, and compression
    cache.rs       the object cache: a trait, memory and Redis behind it
    gc.rs          reachability, the age guard, and segment compaction
    diff.rs        Myers line diff, and why it is line-aware
    merge.rs       merge base, three-way tree and line merge
    proof.rs       Merkle inclusion proofs
    ingest.rs      filesystem -> Merkle DAG, in parallel
    repo.rs        refs, HEAD, commits, diffing
    checkout.rs    Merkle DAG -> filesystem
    submodule.rs   pinning another repository, and where its remote lives
    archive.rs     streaming tar and zip straight from the store
    proto.rs       the sync wire protocol
    session.rs     the server-side session loop, over a RepoHost
    ws.rs          RFC 6455, hand-rolled
    fsck.rs        whole-store verification
  fkit-cli/      the `fkit` binary
  fkit-server/   `fkitd` — lib + thin binary, disk-backed RepoHost
  fkit-hub/      `fkit-hub` — the forge: axum, sqlx, Postgres-backed RepoHost
web/             the UI — Loom web components, no runtime dependencies
```

`fkit-core` has **two dependencies**: `blake3` and `anyhow`. The WebSocket
layer, its SHA-1 and base64, and every binary encoding are written out
longhand — it is meant to be read end to end. The binaries around it take
`clap`, because argument parsing is a solved problem and hand-rolling it bought
nothing but a missing `--version`. The hub is where that stops
being a virtue: it uses axum, tokio and sqlx, because hand-rolling HTTP routing
and the Postgres wire protocol would be effort with no insight at the end of it.

The frontend is [Loom](https://github.com/Toyz/loom) — decorator-driven web
components, JSX compiled to real DOM, no virtual DOM. ~44 KB gzipped total, no
webfonts, and it paints on the first frame. Large files and long histories go
through `<loom-virtual>`, and syntax highlighting is a line-aware tokenizer
(`web/src/highlight.ts`) written specifically so it composes with
virtualization — most off-the-shelf highlighters return one HTML string for a
whole document, which you cannot slice per line. Adding a language is one entry
holding its grammar, its extensions and its exact filenames; a format that is
line-oriented rather than nested gives regex patterns instead and skips the
tokenizer.

## Design decisions worth knowing about

**No staging area.** `commit` snapshots the working tree as it is. Git's index
exists largely because hashing every file on every status check was too slow in
2005; with BLAKE3 and content-defined chunking, re-snapshotting is cheap enough
that the extra concept isn't worth the confusion.

**`status` never writes.** Hashing is a pure function of content, so a dry-run
sink computes exactly the same hashes as a real commit and reports what a commit
*would* store, without touching disk.

**Checkout takes an explicit "from" tree.** It is not inferred from HEAD, because
`clone` and `pull` have both already moved HEAD by the time they check out.
Inferring it there silently produces an empty diff and writes nothing — which is
precisely the bug that shipped here once, and is now covered by two regression
tests in `tests/workflow.rs`.

**Untracked files are never deleted.** Removals are computed from the *tracked*
set only, so `--force` cannot eat a file fkit has never seen.

## Not done yet

- **On-disk pack index.** The index is loaded into memory at open (~48 bytes per
  object). Fine to a few million objects; past that it wants a sorted on-disk
  index with binary search.
- **Recursive merge base.** Criss-cross histories can have several equally-good
  bases. fkit picks one and reports the ambiguity rather than merging them into a
  virtual base the way git does.
- **TLS.** Both servers speak plain `ws://`. Terminate TLS in front.
- **Delta compression.** Objects are stored whole. Replaying this repository's
  own history makes the gap concrete:

  | 61 commits of fkit itself | on disk |
  |---|---|
  | fkit | 2.2 MiB |
  | git, fully repacked | **594 KiB** |

  Deduplication is working — 66.7 MiB of logical content across those commits
  collapses to 5.3 MiB unique and 2.2 MiB stored, 30× — but 62% of the source
  files here are smaller than the 8 KiB average chunk, so a one-line edit
  rewrites the whole file as a new chunk. Content-defined chunking pays off on
  large files with localized edits, which is the opposite of source code. Two
  obvious levers were measured and neither is one: zstd 1→9 gives 2.2 → 2.0
  MiB, and a 2 KiB average chunk cuts unique bytes 23% while leaving the disk
  total unchanged, because smaller frames compress worse and the extra index
  entries eat the rest. Closing it needs deltas between versions of a chunk.

- **Cross-repo dedup beyond a fork network.** Forks share one store, so a
  repository and its forks deduplicate against each other. Two *unrelated*
  repositories do not, and pooling them needs refcounting and care about
  private data.

## Tests

```sh
make test        # cargo test --workspace, then tsc --noEmit for the UI
```

196 Rust tests, zero clippy warnings. The ones that document the actual ideas:

- `chunker::insertion_only_perturbs_local_chunks` — a 10-byte insert into 4 MB
  leaves >95% of chunks untouched
- `chunker::fixed_size_chunking_would_have_failed_the_previous_test` — the
  contrast case
- `chunker::chunk_sizes_are_actually_variable` — catches a chunker that has
  silently degenerated into fixed-size
- `ingest::small_edit_stores_almost_nothing_new` — a 1-byte edit to 8 MB
  re-stores under 2%
- `sync::incremental_sync_transfers_only_the_delta`
- `sync::a_lying_peer_is_rejected`
- `ingest::adding_one_file_to_a_large_directory_rewrites_almost_nothing`
- `proof::a_step_from_a_different_tree_cannot_be_spliced_in`
- `proof::a_proof_is_small_even_for_a_large_repository`
- `pack::a_torn_index_entry_is_ignored_not_fatal`
- `pack::incompressible_objects_are_stored_raw`
- `gc::young_objects_are_spared_even_when_unreachable`
- `gc::a_mostly_live_segment_is_left_alone`
- `merge::a_rebuilt_tree_hashes_the_same_as_an_ingested_one`
- `diff::reconstruction_holds_for_a_range_of_edits`
- `workflow::checkout_into_an_empty_directory_writes_everything`
- `submodules::a_submodule_is_committed_as_a_pin_and_not_as_a_copy` — the
  parent's commit stores none of the submodule's own objects a second time
- `submodules::checking_out_an_older_commit_moves_the_submodule_back` — the
  thing git does not do
- `submodules::gc_keeps_what_a_pin_points_at` — nothing taught gc about
  submodules; this asserts that not teaching it was correct
- `submodules::a_pin_the_store_cannot_resolve_is_refused` — a commit nobody
  could check out is refused at the point it is made
- `submodule::what_is_recorded_relatively_resolves_back_to_where_it_came_from`
- `cache::tests::no_object_may_take_more_than_its_share`
- `cache::tier_tests::a_far_tier_that_is_unreachable_only_costs_hits`

## License

MIT — see [LICENSE](LICENSE).
