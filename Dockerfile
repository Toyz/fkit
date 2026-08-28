# syntax=docker/dockerfile:1.7

# ---------------------------------------------------------------------------
# Stage 1 — build the web UI
#
# Separate from the Rust build so a change to a .rs file does not re-run npm,
# and vice versa.
# ---------------------------------------------------------------------------
FROM node:22-alpine AS web
WORKDIR /web

# Dependencies first: this layer is cached until package.json actually changes.
COPY web/package.json web/package-lock.json* ./
RUN npm ci --no-audit --no-fund 2>/dev/null || npm install --no-audit --no-fund

COPY web/ ./
RUN npm run build


# ---------------------------------------------------------------------------
# Stage 2 — build the Rust binaries
# ---------------------------------------------------------------------------
FROM rust:1-alpine AS rust
RUN apk add --no-cache musl-dev

WORKDIR /src

# Prime the dependency cache with manifests and stub sources, so editing real
# code recompiles only this workspace and not the whole crates.io tree.
COPY Cargo.toml Cargo.lock ./
COPY crates/fkit-core/Cargo.toml   crates/fkit-core/
COPY crates/fkit-cli/Cargo.toml    crates/fkit-cli/
COPY crates/fkit-server/Cargo.toml crates/fkit-server/
COPY crates/fkit-hub/Cargo.toml    crates/fkit-hub/
RUN mkdir -p crates/fkit-core/src crates/fkit-cli/src crates/fkit-server/src crates/fkit-hub/src \
 && echo 'fn main(){}'  > crates/fkit-cli/src/main.rs \
 && echo 'fn main(){}'  > crates/fkit-server/src/main.rs \
 && echo 'fn main(){}'  > crates/fkit-hub/src/main.rs \
 && touch crates/fkit-core/src/lib.rs crates/fkit-server/src/lib.rs \
 && cargo build --release --workspace 2>/dev/null || true

# Now the real sources.
COPY crates/ crates/
# Bust the stub fingerprints so cargo rebuilds these crates with real code.
RUN find crates -name '*.rs' -exec touch {} + \
 && cargo build --release --workspace \
 && strip target/release/fkit target/release/fkitd target/release/fkit-hub


# ---------------------------------------------------------------------------
# Stage 3 — runtime
# ---------------------------------------------------------------------------
FROM alpine:3.21 AS runtime
RUN apk add --no-cache ca-certificates tini \
 && adduser -D -u 10001 -h /var/lib/fkit fkit

COPY --from=rust /src/target/release/fkit-hub /usr/local/bin/fkit-hub
COPY --from=rust /src/target/release/fkitd    /usr/local/bin/fkitd
COPY --from=rust /src/target/release/fkit     /usr/local/bin/fkit
COPY --from=web  /web/dist                    /srv/web

# Repositories and object stores live here — mount a volume over it.
RUN mkdir -p /var/lib/fkit/data && chown -R fkit:fkit /var/lib/fkit
USER fkit
WORKDIR /var/lib/fkit

ENV FKIT_DATA=/var/lib/fkit/data \
    FKIT_WEB_DIR=/srv/web \
    FKIT_LISTEN=0.0.0.0:7500

EXPOSE 7500 7420

# tini reaps zombies and forwards signals, so graceful shutdown actually works.
ENTRYPOINT ["/sbin/tini", "--"]
CMD ["fkit-hub"]

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD wget -qO- http://127.0.0.1:7500/_health || exit 1
