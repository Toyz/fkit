-- Instance policy, and the ability to turn accounts off.
--
-- Registration and visibility policy previously lived only in fkit-hub.toml,
-- which meant changing them required shell access and a restart. An
-- administrator should be able to close registration from the web the moment
-- they need to. The config file still seeds these on first boot, so a fresh
-- deployment behaves exactly as its file says; after that the database is the
-- source of truth and the file is the default.

CREATE TABLE instance_settings (
    -- Exactly one row, enforced by the primary key and the check together.
    id                      BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),

    site_name               TEXT        NOT NULL DEFAULT 'fkit hub',
    open_registration       BOOLEAN     NOT NULL DEFAULT TRUE,
    require_auth            BOOLEAN     NOT NULL DEFAULT FALSE,
    default_repo_visibility TEXT        NOT NULL DEFAULT 'private'
                            CHECK (default_repo_visibility IN ('public', 'private')),
    -- When set, only these email domains may register. Empty means any.
    allowed_email_domains   TEXT[]      NOT NULL DEFAULT '{}',

    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by              UUID        REFERENCES users(id) ON DELETE SET NULL
);

-- Disabling an account is reversible; deleting one destroys their repositories.
-- Both need to exist, and the reversible one should be the easy path.
ALTER TABLE users ADD COLUMN is_active BOOLEAN NOT NULL DEFAULT TRUE;

CREATE INDEX users_active_idx ON users (username) WHERE is_active;
