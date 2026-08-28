-- Merge requests.
--
-- A compare view is ephemeral — it recomputes from two refs every time. A merge
-- request is the durable version: a proposal with an author, a title, a state,
-- and eventually a record of how it ended. The diff is still computed live from
-- the refs, so a request never goes stale against its branches.

CREATE TABLE merge_requests (
    id            UUID PRIMARY KEY,
    repo_id       UUID        NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    -- Per-repository sequential number, so requests are "#3", not a UUID.
    number        INT         NOT NULL,
    title         TEXT        NOT NULL,
    description   TEXT,
    author_id     UUID        REFERENCES users(id) ON DELETE SET NULL,

    source_branch TEXT        NOT NULL,
    target_branch TEXT        NOT NULL,

    state         TEXT        NOT NULL DEFAULT 'open'
                  CHECK (state IN ('open', 'merged', 'closed')),

    -- Set when state becomes 'merged'.
    merge_commit  BYTEA       CHECK (merge_commit IS NULL OR octet_length(merge_commit) = 32),
    merged_at     TIMESTAMPTZ,
    merged_by     UUID        REFERENCES users(id) ON DELETE SET NULL,

    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT number_per_repo UNIQUE (repo_id, number),
    -- A branch cannot be merged into itself; the comparison would be empty and
    -- the merge a no-op.
    CONSTRAINT distinct_branches CHECK (source_branch <> target_branch)
);

CREATE INDEX mr_repo_state_idx ON merge_requests (repo_id, state, number DESC);

-- Only one open request may propose the same source -> target pair. A second
-- one would show an identical diff and racing merges would be ambiguous.
CREATE UNIQUE INDEX mr_one_open_per_pair
    ON merge_requests (repo_id, source_branch, target_branch)
    WHERE state = 'open';
