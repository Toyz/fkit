-- Work in progress, parked on the server so it can be picked up elsewhere.
--
-- Deliberately not a ref. A branch or a tag is a claim about the project;
-- a stash is a note to yourself that happens to name objects in this
-- repository, and nobody else will ever act on it. Storing it as a ref would
-- put it in the one namespace every listing, the branch picker, the compare
-- view and the sync protocol's greeting already read from — and privacy would
-- then depend on every one of those remembering to filter, including the ones
-- not written yet. A separate table cannot leak through paths it is not in.
CREATE TABLE stashes (
    id      UUID PRIMARY KEY,

    -- Whose it is. Only this account may see it, restore it or delete it, and
    -- that includes site administrators: they can read every repository by
    -- design, which is what "administrator" means for operations, but somebody
    -- else's unfinished work is a different promise.
    user_id UUID NOT NULL REFERENCES users(id)  ON DELETE CASCADE,

    -- Which repository it belongs to. A stash cannot cross repositories: it is
    -- only meaningful against the history it was taken from.
    --
    -- Note this is the repository, not the store. Forks share one object store,
    -- so the bytes of a stash sit in a place the whole fork tree can reach,
    -- and it is this column — not the store — that decides a fork does not
    -- inherit its parent's stashes.
    repo_id UUID NOT NULL REFERENCES repos(id)  ON DELETE CASCADE,

    -- The stash itself, and the commit it was taken from.
    --
    -- `base` is the stash commit's own first parent, so it is recoverable from
    -- the object; it is stored because reading it is then one row rather than
    -- one object read, and because the diff a viewer wants is exactly
    -- base..commit — which is what every commit page already renders, since it
    -- diffs a commit against its first parent.
    commit_hash BYTEA NOT NULL CHECK (octet_length(commit_hash) = 32),
    base_hash   BYTEA NOT NULL CHECK (octet_length(base_hash)   = 32),

    message TEXT NOT NULL,
    -- Bytes this stash added to the store, for the per-account quota.
    bytes   BIGINT NOT NULL DEFAULT 0,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Stashes expire, and that is a feature rather than an apology. The point
    -- of one is to carry work to another machine, which takes hours; a stash
    -- older than a month is not following anybody, it is unreviewed sensitive
    -- content nobody remembers leaving on a server.
    expires_at TIMESTAMPTZ NOT NULL,

    -- Pushing the same stash twice is the same stash.
    UNIQUE (user_id, repo_id, commit_hash)
);

-- "What have I got parked here", the only listing that exists.
CREATE INDEX stashes_owner_idx ON stashes (user_id, repo_id, created_at DESC);

-- The sweeper, and the guard on the commit route: a commit that belongs to
-- somebody else's stash must not render just because the hash was known.
CREATE INDEX stashes_commit_idx  ON stashes (commit_hash);
CREATE INDEX stashes_expiry_idx  ON stashes (expires_at);
