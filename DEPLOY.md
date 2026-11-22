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

`.github/workflows/image.yml` does all of this without being asked, and is much
faster: it compiles natively with a warm cache and assembles from finished
binaries via `Dockerfile.dist`. `make image` uses the self-contained
`Dockerfile` instead — one command, no toolchain assumptions, cold build under
QEMU.

| pushed | tags published |
|---|---|
| a commit to `main` | `:main`, `:latest` |
| a tag `v1.2.3` | `:1.2.3`, `:1.2`, `:latest` |

Every build moves `:latest`, so a server that pins nothing follows `main`. Set
`FKIT_TAG` in `.env.prod` to pin a version instead.

## 2. Put the files on the server

```sh
scp docker-compose.prod.yml deploy/hub.toml .env.prod.example server:/opt/fkit/
ssh server 'cd /opt/fkit && mv .env.prod.example .env.prod'
```

Settings go in **`hub.toml`** (mounted at `/etc/fkit/hub.toml`), secrets in
**`.env.prod`**. The environment overrides the file.

The file is commented and short. Four lines in it are about *where* this server
is, and are the ones to get right:

| setting | set it to | or the symptom is |
|---|---|---|
| `[server] secure_cookies` | `true` behind TLS | see [cookies](#cookies-and-tls) |
| `[server] trust_proxy` | `true` behind a proxy | see [the proxy](#the-proxy) |
| `[email] public_url` | the address people use, `https://…` | reset and invite links, and every link preview, point at whatever host the request happened to name |
| `[email] from` | an address on a domain verified with Resend | no mail leaves, and the sign-in page says there is no reset rather than offering one that cannot work |

`public_url` and `from` only *seed* a new database: after the first boot the
values in **admin → email** are the ones in force, so editing the file later
changes nothing. `FKIT_PUBLIC_URL` and `FKIT_EMAIL_FROM` in the hub's
environment are different — they always win, and the admin page shows them as
fixed. The compose file does not pass those two through; add them under the
`hub` service's `environment:` if you want them pinned that way.

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
| an idle timeout above 20 seconds | the hub pings a quiet sync connection every 20s, so the usual 60s default is fine; a proxy set lower than that closes a push while the server is still verifying it, and the client reports only `peer disconnected` |

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

The second job in `.github/workflows/image.yml` replays `main` into the hub
after each build, using the `fkit` binary out of the image that build just
published. Two repository secrets:

```
FKIT_REMOTE   wss://fkit.work/helba/fkit
FKIT_TOKEN    an access token with write access
```

One fkit commit per git commit, with the original author, date and message, and
the git hash recorded in a trailer. That trailer is also the bookmark: the next
run reads the newest one out of the hub's log and resumes after it, so a run
costs what changed. If the recorded commit is no longer in the history — a
force-push — the whole history is replayed, because picking up from the tip
would silently skip whatever came between.

It will not send the token over plain `ws://` to anything but localhost. A
manual run can skip the mirror, which is for the day the protocol on `main` is
ahead of what the deployed hub speaks: update the server first, then mirror.

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
