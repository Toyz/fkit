# Deploying fkit hub

The server runs a published image. It never needs the source tree, a Rust
toolchain, or Node — you build once and the server pulls.

## 1. Publish an image

```sh
make push TAG=v1
```

Builds for `linux/amd64` (override with `PLATFORMS=linux/arm64`) and pushes
`ghcr.io/toyz/fkit-hub:v1` and `:latest`. A registry login with package-write
rights is needed once:

```sh
gh auth refresh -s write:packages          # the default token cannot push
gh auth token | docker login ghcr.io -u Toyz --password-stdin
```

GHCR packages are private by default, so the server needs its own login — a
classic PAT with `read:packages` is enough.

Once this repository is on GitHub, `.github/workflows/image.yml` does this on
every tag push and it is much faster than the local path: it compiles natively
with a warm cargo cache and assembles the image from the finished binaries via
`Dockerfile.dist`, so a source-only change rebuilds four crates rather than the
whole dependency tree, and the image build itself takes about a second. The
local `make image` keeps the self-contained `Dockerfile` — one command, no
toolchain assumptions — at the cost of a cold build under QEMU.

## 2. Put the deployment files on the server

```sh
scp docker-compose.prod.yml deploy/hub.toml .env.prod.example server:/opt/fkit/
ssh server 'cd /opt/fkit && mv .env.prod.example .env.prod'
```

Settings live in **`hub.toml`**, mounted at `/etc/fkit/hub.toml`. Secrets live
in **`.env.prod`** — a connection string and an API key have no business in a
file that gets committed. Note the environment overrides the file, so the
compose passes only those two through and nothing else.

Fill in `.env.prod`:

```sh
POSTGRES_PASSWORD=$(openssl rand -base64 24)   # with --profile bundled-db
# or DATABASE_URL=postgres://...               # and drop that profile
RESEND_API_KEY=                                # optional
```

Migrations run on boot either way — the hub needs a database it can create
tables in, not a specific one.

## 3. Start it

```sh
cd /opt/fkit
docker compose --env-file .env.prod -f docker-compose.prod.yml \
  --profile bundled-db up -d          # drop the profile if you set DATABASE_URL
docker compose -f docker-compose.prod.yml logs -f hub
```

The hub listens on `BIND_ADDR:HUB_PORT` (`127.0.0.1:7500` by default), which is
local-only until you deliberately expose it. Migrations run on boot.

### Claim the administrator account before you expose it

An empty server lets the *first* registration through even with
`open_registration = false` — otherwise an instance shipped with registration
closed could never be set up. That account becomes the administrator.

Which means the order matters, and it is the reverse of what feels natural:

```sh
# 1. still on 127.0.0.1, from the server itself
ssh -L 7500:127.0.0.1:7500 server      # or curl it locally
#    register your account at http://127.0.0.1:7500
# 2. only then point the proxy at it
```

Expose it first and there is a window between the proxy going live and you
signing up in which whoever finds the host becomes its administrator. The
default `BIND_ADDR` protects you right up until the moment you put a proxy in
front, and that is exactly the step this guide is about.

Then close registration in **admin → instance** and bring people in from
**admin → invites**.

### The proxy

It must pass WebSocket upgrades through: the sync protocol is WebSocket end to
end, so a proxy that mangles `Upgrade` breaks push and clone while leaving the
web UI looking perfectly healthy.

It must also set `X-Forwarded-For`, and `hub.toml` must say `trust_proxy = true`
to believe it. Rate limits are counted per client address; behind a proxy every
request appears to come from the proxy, so with `trust_proxy` off the whole
instance shares a single bucket — ten sign-ins a minute between everyone, and
the symptom is your users being told to try again later because somebody else
just logged in.

Leave `trust_proxy` **off** if the hub is reachable directly. The header is
client-supplied there, so believing it lets anyone present a new address per
request and skip the limits entirely.

Then:

```
https://fkit.work
fkit clone wss://fkit.work/helba/fkit
```

### Cookies and TLS

`secure_cookies` in `hub.toml` is the one to get right. Leave it `true` when
something terminates TLS in front. Set it `false` if you reach the hub over
plain `http://` — a `Secure` cookie sent over http is discarded by the browser,
and the symptom is a login that appears to succeed and then isn't.

For a private CA or a certificate minted for a name that only exists on an
overlay network, point clients at the root:

```sh
export FKIT_CA_BUNDLE=/etc/fkit/internal-ca.crt
fkit clone wss://hub.internal/helba/fkit
```

There is deliberately no `--insecure`. Adding a root is a decision about who you
trust; skipping verification is a decision to trust the network, and the sync
protocol sends your access token in its opening frame.

## The object cache

The hub holds decompressed objects in memory so a hot one is not read and
inflated twice. **This is on by default and needs no setup**, and it is why a
busy server settles well above its idle size — 64 MiB of cache by default, on
top of the process itself. Admin → Overview reports what it is holding and its
hit rate, so you can tell a working cache from a leak without guessing.

Content addressing is what makes this simple: a key is a digest of its value,
so a cached object can never be stale. There is no invalidation, only eviction,
which is a size and an age:

```toml
[cache]
memory_mb = 64      # 0 disables the cache entirely
ttl_secs  = 1800
```

### A shared tier, when it is worth one

Several hub processes each keep their own memory cache, and each pays its own
misses. A shared tier in Valkey or Redis lets them answer each other's.

**Most servers should not do this.** On one host with local storage, a round
trip to Redis costs more than the disk read it would replace — measured at
7 µs to read and inflate an object locally against 100–500 µs for the round
trip. It pays when a miss is genuinely expensive: several processes, or object
storage slower than a local disk. Memory is always the near tier either way, so
a shared one only ever answers what memory missed.

It is **off at compile time**, so the client is not linked into a binary that
will not use it:

```sh
cargo build --release -p fkit-hub --features redis-cache
```

Then point it at one:

```toml
[cache]
memory_mb = 64
ttl_secs  = 1800
redis_url = "redis://valkey:6379"
```

With `docker-compose`, that is one more service:

```yaml
  valkey:
    image: valkey/valkey:8-alpine
    restart: unless-stopped
    # No volume: everything in here is a cache of content that is already on
    # disk, so losing it costs one slow request and nothing else.
    command: ["valkey-server", "--save", "", "--maxmemory", "512mb",
              "--maxmemory-policy", "allkeys-lru"]
```

`--save ""` turns off persistence and `allkeys-lru` lets it evict under
pressure. Both are right for a cache and wrong for a database — nothing here is
the only copy of anything.

The startup log says which tier it got, and the admin panel says it too:

```
INFO fkit_hub: object cache: memory + shared at redis://valkey:6379
```

If the server cannot reach it, that is a warning and not a failure — the hub
starts on memory alone. A cache that is unavailable should never be the reason
a repository cannot be read.

## Mirroring this repository into fkit

`.github/workflows/mirror-to-fkit.yml` pushes every update to `main` into the
hub, so fkit hosts its own source. It needs two repository secrets:

```
FKIT_REMOTE   wss://fkit.work/helba/fkit
FKIT_TOKEN    an access token with write access (Settings → access tokens)
```

It clones the hub repository first so each run's commit has a parent, replaces
the working tree, and commits once per push to main. It is not a git history
translation — it is a record of what `main` looked like, in fkit's own object
model, with real diffs between snapshots.

## Updating

```sh
make push TAG=v2
# on the server
docker compose --env-file .env.prod -f docker-compose.prod.yml pull
docker compose --env-file .env.prod -f docker-compose.prod.yml \
  --profile bundled-db up -d
```

Data lives in the `pgdata` and `repodata` volumes and survives an image swap.
Back both up together: the object store alone is repositories with no accounts
or permissions attached, and the database alone is accounts pointing at objects
that are gone.
