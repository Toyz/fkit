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

# --- the watcher -----------------------------------------------------------
# cargo-watch is the direct analogue of air. watchexec does the same job if it
# is what you already have, so either is accepted rather than insisted on.
if command -v cargo-watch >/dev/null 2>&1; then
  WATCH="cargo watch -c -w crates -x 'run -p fkit-hub'"
elif command -v watchexec >/dev/null 2>&1; then
  WATCH="watchexec -c -r -w crates -e rs -- cargo run -p fkit-hub"
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

# --- the database ----------------------------------------------------------
# A hub container from `make up` would already hold this port, and the bind
# failure that follows is a confusing way to find that out.
if [ -n "$($COMPOSE ps -q hub 2>/dev/null)" ]; then
  echo "Stopping the containerised hub — this script runs one on the host instead."
  $COMPOSE stop hub >/dev/null
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
