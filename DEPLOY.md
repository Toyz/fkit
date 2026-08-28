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

The hub listens on `BIND_ADDR:HUB_PORT` (`127.0.0.1:7500` by default) — point
your proxy at that. It must pass WebSocket upgrades through: the sync protocol
is WebSocket end to end, so a proxy that mangles `Upgrade` breaks push and
clone while leaving the web UI looking perfectly healthy.

Migrations run on boot. The first account you register becomes the administrator — do that
immediately, then close registration in **admin → instance** and bring anyone
else in from **admin → invites**.

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
