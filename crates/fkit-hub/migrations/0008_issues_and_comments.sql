-- Issues, and the comments both they and merge requests hang off.
--
-- Comments are one table rather than two because the thing a comment *is* does
-- not change with what it is attached to: an author, a body, a time. Two tables
-- would mean two of every query, and the line-anchoring below written twice.

CREATE TABLE issues (
    id         UUID PRIMARY KEY,
    repo_id    UUID        NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    -- Shared with merge_requests, not a sequence of its own: "#4" has to mean
    -- one thing in a repository, or every cross-reference is ambiguous.
    number     INT         NOT NULL,
    title      TEXT        NOT NULL,
    body       TEXT,
    author_id  UUID        REFERENCES users(id) ON DELETE SET NULL,

    state      TEXT        NOT NULL DEFAULT 'open'
               CHECK (state IN ('open', 'closed')),
    closed_at  TIMESTAMPTZ,
    closed_by  UUID        REFERENCES users(id) ON DELETE SET NULL,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT issue_number_per_repo UNIQUE (repo_id, number)
);

CREATE INDEX issue_repo_state_idx ON issues (repo_id, state, number DESC);

CREATE TABLE comments (
    id               UUID PRIMARY KEY,
    repo_id          UUID        NOT NULL REFERENCES repos(id) ON DELETE CASCADE,

    -- Exactly one subject, enforced below.
    issue_id         UUID        REFERENCES issues(id) ON DELETE CASCADE,
    merge_request_id UUID        REFERENCES merge_requests(id) ON DELETE CASCADE,

    author_id        UUID        REFERENCES users(id) ON DELETE SET NULL,
    body             TEXT        NOT NULL,

    -- ---- line anchoring, all null on a plain conversation comment ----
    --
    -- Anchored to the *content* rather than to a commit. A diff here is
    -- recomputed live from two branches, so a comment pinned to "line 42 of
    -- commit abc" slides onto an unrelated line the moment anyone pushes.
    -- Pinned to the hash of the file it was written against, it stays exactly
    -- where it was put for as long as that file's content is unchanged —
    -- through a rebase, an amend, or ten unrelated commits — because identical
    -- content is the same hash, and line 42 of identical content is the same
    -- line. When the file does change, the comment is not silently wrong: its
    -- blob is simply absent from the new diff, which is what "outdated" means
    -- and can be shown as such.
    file_path        TEXT,
    line             INT,
    side             TEXT        CHECK (side IN ('old', 'new')),
    blob             BYTEA       CHECK (blob IS NULL OR octet_length(blob) = 32),
    -- What the author was looking at. Display only; the anchor is the blob.
    commit_hash      BYTEA       CHECK (commit_hash IS NULL OR octet_length(commit_hash) = 32),

    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Set on the first edit, so "edited" can be shown without comparing
    -- timestamps that also move for unrelated reasons.
    edited_at        TIMESTAMPTZ,

    CONSTRAINT one_subject CHECK (
        (issue_id IS NOT NULL)::int + (merge_request_id IS NOT NULL)::int = 1
    ),
    -- A line anchor is all of its parts or none of them; half of one cannot be
    -- rendered and cannot be repaired.
    CONSTRAINT anchor_whole CHECK (
        (file_path IS NULL AND line IS NULL AND side IS NULL AND blob IS NULL)
        OR (file_path IS NOT NULL AND line IS NOT NULL AND side IS NOT NULL AND blob IS NOT NULL)
    ),
    -- Only a merge request has code under it to point at.
    CONSTRAINT lines_belong_to_merges CHECK (blob IS NULL OR merge_request_id IS NOT NULL)
);

CREATE INDEX comment_issue_idx ON comments (issue_id, created_at);
CREATE INDEX comment_merge_idx ON comments (merge_request_id, created_at);
-- Finding every comment on a file as it was: the lookup the diff view makes
-- once per file it renders.
CREATE INDEX comment_blob_idx ON comments (merge_request_id, blob) WHERE blob IS NOT NULL;
