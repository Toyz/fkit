# Deploying fkit hub

The server runs a published image. It never needs the source tree, a Rust
toolchain, or Node.

## 1. Publish an image

```sh
make push TAG=v1
```

Builds `linux/amd64` (override with `PLATFORMS=linux/arm64`) and pushes
`ghcr.io/toyz/fkit-hub:v1` and `:latest`. Registry login, once:

```sh
gh auth refresh -s write:packages          # the default token cannot push
gh auth token | docker login ghcr.io -u Toyz --password-stdin
```

GHCR packages are private by default, so the server needs its own login — a
classic PAT with `read:packages`.

`.github/workflows/image.yml` does all of this on a tag push, and is much
faster: it compiles natively with a warm cache and assembles from finished
binaries via `Dockerfile.dist`. `make image` uses the self-contained
`Dockerfile` instead — one command, no toolchain assumptions, cold build under
QEMU.

## 2. Put the files on the server

```sh
scp docker-compose.prod.yml deploy/hub.toml .env.prod.example server:/opt/fkit/
ssh server 'cd /opt/fkit && mv .env.prod.example .env.prod'
```

Settings go in **`hub.toml`** (mounted at `/etc/fkit/hub.toml`), secrets in
**`.env.prod`**. The environment overrides the file.

```sh
POSTGRES_PASSWORD=$(openssl rand -base64 24)   # with --profile bundled-db
# or DATABASE_URL=postgres://...               # and drop that profile
RESEND_API_KEY=                                # optional
```

## 3. Start it

```sh
cd /opt/fkit
docker compose --env-file .env.prod -f docker-compose.prod.yml \
  --profile bundled-db up -d          # drop the profile if you set DATABASE_URL
docker compose -f docker-compose.prod.yml logs -f hub
```

Listens on `BIND_ADDR:HUB_PORT` (`127.0.0.1:7500`), local-only until you expose
it. Migrations run on boot.

### Claim the admin account before you expose it

An empty server lets the **first** registration through even with
`open_registration = false`, and that account becomes the administrator. So
register *before* the proxy goes live, not after:

```sh
ssh -L 7500:127.0.0.1:7500 server      # then register at http://127.0.0.1:7500
```

Otherwise there is a window in which whoever finds the host becomes its admin.
Then close registration in **admin → instance** and invite people from
**admin → invites**.

### The proxy

| requirement | why it bites |
|---|---|
| pass WebSocket upgrades through | sync is WebSocket end to end; mangling `Upgrade` breaks push and clone while the web UI still looks fine |
| set `X-Forwarded-For`, and `trust_proxy = true` | rate limits count per client address; without it the whole instance shares one bucket |

**Leave `trust_proxy` off if the hub is reachable directly.** The header is
client-supplied, so believing it there lets anyone skip the limits entirely.

```
https://fkit.work
fkit clone wss://fkit.work/helba/fkit
```

### Cookies and TLS

`secure_cookies = true` when something terminates TLS in front, `false` over
plain `http://` — a `Secure` cookie on http is dropped by the browser, and the
symptom is a login that appears to work and doesn't.

For a private CA:

```sh
export FKIT_CA_BUNDLE=/etc/fkit/internal-ca.crt
fkit clone wss://hub.internal/helba/fkit
```

There is deliberately no `--insecure`: the sync protocol sends your access
token in its opening frame.

## The object cache

On by default, no setup. It is why the process settles above its idle size —
64 MiB of held objects by default. **Admin → Overview** shows what it holds and
its hit rate, which is how you tell a working cache from a leak.

```toml
[cache]
memory = "64MB"     # or "512kb", "2GB", bytes; 0 disables it
ttl    = "30m"      # or "1800s", "2h", "1d"
```

Sizes are binary (`MB` is 1024²). A value that is not a size is refused at
startup rather than guessed at.

### A shared tier

Several hub processes each pay their own misses; a shared tier in Valkey lets
them answer each other's. **Most servers should not.** On one host with local
storage a round trip costs more than the disk read it replaces — 7 µs local
against 100–500 µs. It pays with several processes, or storage slower than a
local disk.

The published image includes the client. Building it yourself needs the
feature:

```sh
cargo build --release -p fkit-hub --features redis-cache
```

```toml
[cache]
memory    = "64MB"
redis_url = "redis://valkey:6379"
```

```yaml
  valkey:
    image: valkey/valkey:8-alpine
    restart: unless-stopped
    # No volume: this caches content that is already on disk.
    command: ["valkey-server", "--save", "", "--maxmemory", "512mb",
              "--maxmemory-policy", "allkeys-lru"]
```

Memory is always the near tier. If Valkey is unreachable the hub logs a warning
and runs on memory alone — a cache should never be why a repository cannot be
read.

## Mirroring this repository into fkit

`.github/workflows/mirror-to-fkit.yml` pushes every update to `main` into the
hub. Two repository secrets:

```
FKIT_REMOTE   wss://fkit.work/helba/fkit
FKIT_TOKEN    an access token with write access
```

It records what `main` looked like in fkit's object model, with real diffs
between snapshots — not a git history translation.

## Updating

```sh
make push TAG=v2
# on the server
docker compose --env-file .env.prod -f docker-compose.prod.yml pull
docker compose --env-file .env.prod -f docker-compose.prod.yml \
  --profile bundled-db up -d
```

Data lives in the `pgdata` and `repodata` volumes and survives an image swap.
**Back both up together** — the object store alone is repositories with no
accounts, and the database alone is accounts pointing at objects that are gone.
