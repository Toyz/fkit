-- Outbound email, and the password reset flow it exists to serve.

ALTER TABLE instance_settings
    -- Stored, never returned by the API. An administrator can replace it or
    -- clear it, but nobody can read it back out of the server.
    ADD COLUMN resend_api_key TEXT,
    -- Must be an address on a domain verified with Resend, or sending fails.
    ADD COLUMN email_from     TEXT NOT NULL DEFAULT '',
    -- The base URL that reset links point at. Derived from the request when
    -- blank, which is wrong behind a proxy that rewrites Host.
    ADD COLUMN public_url     TEXT NOT NULL DEFAULT '';

-- Single-use, short-lived password reset tokens.
--
-- Only a digest is stored, exactly as for sessions and access tokens: a leak of
-- this table must not let anyone reset an account.
CREATE TABLE password_resets (
    id         UUID PRIMARY KEY,
    user_id    UUID        NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash TEXT        NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    -- Set the moment it is spent, so a link cannot be replayed.
    used_at    TIMESTAMPTZ
);

CREATE INDEX password_resets_user_idx ON password_resets (user_id, created_at DESC);
