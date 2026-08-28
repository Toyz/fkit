-- Invitations: the way into a server with registration closed.
--
-- A closed instance previously had exactly one door — the very first account —
-- and no way to bring anyone else in. An invite is a single-use token that
-- suspends `open_registration` for one person.
--
-- Only a digest is stored, exactly as for sessions, access tokens and password
-- resets: a leak of this table must not let anyone register.
CREATE TABLE invites (
    id         UUID PRIMARY KEY,
    token_hash TEXT        NOT NULL UNIQUE,
    -- Optional. When set, the invite is bound to this address: it can only be
    -- redeemed by someone registering with it, and it is what the invite mail
    -- was sent to. When null the link works for whoever holds it.
    email      TEXT,
    -- Free text shown in the admin list, e.g. "contractor, read-only".
    note       TEXT        NOT NULL DEFAULT '',
    -- Grant administrator rights on redemption. Off by default; an invite that
    -- silently makes an admin is a trap.
    is_admin   BOOLEAN     NOT NULL DEFAULT FALSE,
    created_by UUID        REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    -- Set the moment it is spent, so a link cannot be replayed.
    used_at    TIMESTAMPTZ,
    used_by    UUID        REFERENCES users(id) ON DELETE SET NULL
);

CREATE INDEX invites_open_idx ON invites (created_at DESC) WHERE used_at IS NULL;
