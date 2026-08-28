-- fkit hub schema.
--
-- Division of labour: Postgres owns everything mutable and relational — who
-- exists, who may do what, and where each branch currently points. The
-- content-addressed object store on disk owns everything immutable. Objects are
-- never referenced by a foreign key because they are not rows; they are named by
-- the hash of their own bytes and shared by construction.

CREATE TABLE users (
    id             UUID PRIMARY KEY,
    -- Stored lower-case; the app normalises before insert so lookups are exact
    -- and we avoid a citext dependency.
    username       TEXT        NOT NULL UNIQUE,
    email          TEXT        NOT NULL UNIQUE,
    -- Argon2id PHC string. Never a plaintext or reversible value.
    password_hash  TEXT        NOT NULL,
    display_name   TEXT,
    is_admin       BOOLEAN     NOT NULL DEFAULT FALSE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT username_shape CHECK (username ~ '^[a-z0-9][a-z0-9._-]{0,38}$')
);

CREATE TABLE repos (
    id              UUID PRIMARY KEY,
    owner_id        UUID        NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name            TEXT        NOT NULL,
    description     TEXT,
    -- 'public' is readable by anonymous visitors; 'private' requires an explicit
    -- grant. Enforced in one place: perms::resolve.
    visibility      TEXT        NOT NULL DEFAULT 'private'
                    CHECK (visibility IN ('public', 'private')),
    default_branch  TEXT        NOT NULL DEFAULT 'main',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT repo_name_shape CHECK (name ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    CONSTRAINT one_name_per_owner UNIQUE (owner_id, name)
);

CREATE INDEX repos_owner_idx      ON repos (owner_id);
CREATE INDEX repos_public_idx     ON repos (updated_at DESC) WHERE visibility = 'public';

-- Explicit grants. The owner is NOT listed here; ownership is implied by
-- repos.owner_id and always outranks any row in this table.
CREATE TABLE collaborators (
    repo_id     UUID        NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    user_id     UUID        NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role        TEXT        NOT NULL CHECK (role IN ('read', 'write', 'admin')),
    granted_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    granted_by  UUID        REFERENCES users(id) ON DELETE SET NULL,

    PRIMARY KEY (repo_id, user_id)
);

CREATE INDEX collaborators_user_idx ON collaborators (user_id);

-- Branch tips. This is the one piece of repository state that moves, so it
-- lives in Postgres where a fast-forward check and the write that depends on it
-- can share a transaction. On disk, a ref is a file and that check is racy.
CREATE TABLE refs (
    repo_id     UUID        NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    name        TEXT        NOT NULL,
    -- A raw 32-byte BLAKE3 hash, not hex: half the storage and it makes an
    -- accidental encoding mismatch a type error rather than a silent miss.
    target      BYTEA       NOT NULL CHECK (octet_length(target) = 32),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by  UUID        REFERENCES users(id) ON DELETE SET NULL,

    PRIMARY KEY (repo_id, name),
    CONSTRAINT ref_name_shape CHECK (name ~ '^[A-Za-z0-9][A-Za-z0-9._/-]{0,127}$')
);

-- Browser sessions. The cookie carries a random secret; only its hash is
-- stored, so a database leak cannot be replayed as a login.
CREATE TABLE sessions (
    id          UUID PRIMARY KEY,
    user_id     UUID        NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash  TEXT        NOT NULL UNIQUE,
    user_agent  TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL
);

CREATE INDEX sessions_user_idx    ON sessions (user_id);
CREATE INDEX sessions_expiry_idx  ON sessions (expires_at);

-- Personal access tokens for the CLI. Same rule as sessions: the secret is
-- shown once at creation and never stored.
--
-- `prefix` is the public, non-secret leading segment. It exists so a lookup is
-- an indexed equality match on one row rather than a scan that verifies an
-- Argon2 hash against every token in the table.
CREATE TABLE access_tokens (
    id            UUID PRIMARY KEY,
    user_id       UUID        NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name          TEXT        NOT NULL,
    prefix        TEXT        NOT NULL UNIQUE,
    token_hash    TEXT        NOT NULL,
    can_write     BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at  TIMESTAMPTZ,
    expires_at    TIMESTAMPTZ
);

CREATE INDEX access_tokens_user_idx ON access_tokens (user_id);

-- Append-only record of things worth being able to reconstruct later:
-- ref moves, permission grants, repo creation and deletion.
CREATE TABLE audit_log (
    id          BIGSERIAL PRIMARY KEY,
    actor_id    UUID        REFERENCES users(id) ON DELETE SET NULL,
    repo_id     UUID        REFERENCES repos(id) ON DELETE SET NULL,
    action      TEXT        NOT NULL,
    detail      JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX audit_repo_idx  ON audit_log (repo_id, created_at DESC);
CREATE INDEX audit_actor_idx ON audit_log (actor_id, created_at DESC);
