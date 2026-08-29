-- Which account a commit actually came from.
--
-- A commit's author field is free text. Anyone can write anyone's name and
-- address into it, and every forge that links commits to accounts by matching
-- that address will then show the wrong person's face on the wrong commit.
-- It is a known and unfixable problem with attributing by author string: the
-- string is part of the content, so it is whatever the person who made the
-- content decided it should be.
--
-- The push is different. It is authenticated — by a session or by an access
-- token belonging to exactly one account — so the server does not have to
-- guess who delivered a commit. It knows.
--
-- So this records the thing that is actually known, and the UI says "pushed
-- by" rather than pretending to know who wrote it. The author string is still
-- shown, because it is what the author claimed; the account link beside it is
-- the part that is true.
CREATE TABLE commit_authors (
    -- Keyed globally rather than per repository. A commit hash names one exact
    -- byte sequence, so the same hash in a fork is the same commit — and the
    -- account that first delivered it here is the answer for all of them.
    commit_hash BYTEA PRIMARY KEY CHECK (octet_length(commit_hash) = 32),

    -- Gone with the account. Without a user there is nothing to link to, and
    -- the author string is still there to fall back on.
    user_id     UUID        NOT NULL REFERENCES users(id) ON DELETE CASCADE,

    -- The repository it arrived through, for an audit trail. Not part of the
    -- key: the commit belongs to whoever pushed it first, wherever that was.
    repo_id     UUID        REFERENCES repos(id) ON DELETE SET NULL,
    pushed_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- "What has this person pushed" — the query a profile page makes.
CREATE INDEX commit_authors_user_idx ON commit_authors (user_id, pushed_at DESC);

-- Not every push is the pusher's own work.
--
-- A mirror job pushes somebody else's history using one account's token. Every
-- commit in it would otherwise be attributed to whoever owns that token, which
-- is worse than not attributing at all: a wrong name with a face and a profile
-- link behind it reads as fact.
--
-- So a token can decline to attribute. Give the mirror its own token with this
-- off and the commits it delivers stay flat — the author string, exactly as
-- written, and no account beside it.
ALTER TABLE access_tokens
    ADD COLUMN attributes BOOLEAN NOT NULL DEFAULT TRUE;

COMMENT ON COLUMN access_tokens.attributes IS
    'Link commits pushed with this token to its owner. Off for mirrors.';
