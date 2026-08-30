#!/bin/sh
# Run the hub on this machine with live reload, against Postgres in Docker.
#
# The equivalent of `air` for the Go projects: edit a file under crates/, the
# hub rebuilds and restarts. Everything it needs is derived from .env, so there
# is nothing to configure before the first run.
#
# The frontend is a separate process on purpose — `make web` runs Vite, which
# has its own hot reload and proxies /api here. Two watchers, each fast at its
# own job, beats one that rebuilds Rust because a stylesheet changed.
set -eu

cd "$(dirname "$0")/.."

COMPOSE="docker compose -f docker-compose.yml -f docker-compose.dev.yml"

# --- debug or release ------------------------------------------------------
# Debug by default, because the point of this script is that a change to a .rs
# file is running seconds later, and an optimized rebuild is not.
#
# It is worth knowing what that costs, though, because it is not small and it
# does not look like a build setting when you meet it. An unoptimized hub is
# several times slower at the work a sync actually does -- hashing every object
# it receives and decompressing every object it sends -- so a push of a large
# history against `make dev` can take five times what the same push takes
# against a release binary. That reads as "the protocol is slow" rather than
# "this is a debug build", which is the sort of thing worth being told once.
#
#   RELEASE=1 make dev     optimized, slower to rebuild, fast to push to
if [ "${RELEASE:-0}" = "1" ]; then
  CARGO_ARGS="run --release -p fkit-hub"
  echo "  build    release (RELEASE=1) — rebuilds are slower, the hub is not"
else
  CARGO_ARGS="run -p fkit-hub"
  echo "  build    debug — fast to rebuild; use RELEASE=1 for a fast hub"
fi

# --- the watcher -----------------------------------------------------------
# cargo-watch is the direct analogue of air. watchexec does the same job if it
# is what you already have, so either is accepted rather than insisted on.
if command -v cargo-watch >/dev/null 2>&1; then
  WATCH="cargo watch -c -w crates -x '$CARGO_ARGS'"
elif command -v watchexec >/dev/null 2>&1; then
  WATCH="watchexec -c -r -w crates -e rs -- cargo $CARGO_ARGS"
else
  cat >&2 <<'MSG'
No file watcher found. Install one of:

  cargo install cargo-watch     # what this script prefers
  cargo install watchexec-cli   # also fine

Or run the hub once, without reloading:

  make dev-db && cargo run -p fkit-hub
MSG
  exit 1
fi

# --- secrets ---------------------------------------------------------------
[ -f .env ] || scripts/setup-env.sh
# shellcheck disable=SC1091
. ./.env

: "${POSTGRES_PASSWORD:?POSTGRES_PASSWORD is empty — delete .env and re-run scripts/setup-env.sh}"

DB_PORT="${DEV_DB_PORT:-55432}"
PORT="${HUB_PORT:-7500}"

# --- the port --------------------------------------------------------------
# Whatever already holds the port, the bind failure a minute from now is a
# confusing way to find out about it. A container we can stop; another process
# is someone else's, so say whose it is rather than killing it.
if [ -n "$($COMPOSE ps -q hub 2>/dev/null)" ]; then
  echo "Stopping the containerised hub — this script runs one on the host instead."
  $COMPOSE stop hub >/dev/null
fi

holder=$(ss -ltnp 2>/dev/null | grep ":${HUB_PORT:-7500} " || true)
if [ -n "$holder" ]; then
  echo "Something is already listening on port ${HUB_PORT:-7500}:" >&2
  echo "  $holder" >&2
  echo >&2
  echo "Another 'make dev' in a different terminal is the usual answer. Stop it," >&2
  echo "or set HUB_PORT to something else for this one:" >&2
  echo "  HUB_PORT=7600 make dev" >&2
  exit 1
fi

echo "Starting Postgres on 127.0.0.1:$DB_PORT ..."
# --wait blocks until the healthcheck passes, so the hub's migrations do not
# race the database on a cold start.
$COMPOSE up -d --wait postgres

# --- the hub ---------------------------------------------------------------
# Local data stays out of the Docker volumes: `make nuke` should not be able to
# take a development tree with it, and vice versa.
export DATABASE_URL="postgres://fkit:${POSTGRES_PASSWORD}@127.0.0.1:${DB_PORT}/fkit_hub"
export FKIT_LISTEN="127.0.0.1:${PORT}"
export FKIT_DATA="${FKIT_DEV_DATA:-.dev/data}"
export FKIT_WEB_DIR="${FKIT_WEB_DIR:-web/dist}"
# Plain HTTP locally, so a Secure cookie would be dropped by the browser and
# every login would look like it silently failed.
export FKIT_SECURE_COOKIES=0
export RUST_LOG="${RUST_LOG:-fkit_hub=debug,tower_http=info}"

mkdir -p "$FKIT_DATA"

cat <<MSG

  hub        http://127.0.0.1:$PORT
  database   127.0.0.1:$DB_PORT  (user fkit, db fkit_hub)
  data       $FKIT_DATA
  reloads    on any change under crates/

  The UI is served from $FKIT_WEB_DIR — run 'make web' in another terminal for
  hot reload, or 'cd web && npm run build' once to have the hub serve it.

MSG

exec sh -c "$WATCH"
