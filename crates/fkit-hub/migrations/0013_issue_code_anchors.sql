-- An issue can point at the exact lines it is about.
--
-- Select some lines while reading a file, open an issue from them, and the
-- issue carries where it came from. GitHub can only give you a permalink to
-- paste into the body.
--
-- Anchored to the blob, for the same reason review comments are: content
-- addressing means the hash names one byte sequence forever, so "lines 40-52
-- of this blob" is a fact that cannot rot. A commit-and-line-number anchor is
-- a guess that quietly becomes wrong the moment anyone edits above line 40 —
-- which is exactly when an old issue is most likely to be reread.
--
-- The file *path* is stored too, but only for display: a file that moves keeps
-- its content, so the anchor survives a rename while the path shown beside it
-- is merely where it was at the time. Same for `ref_name`.
ALTER TABLE issues
    ADD COLUMN file_path  TEXT,
    ADD COLUMN line_start INT,
    ADD COLUMN line_end   INT,
    ADD COLUMN blob       BYTEA,
    -- The branch or tag the author was reading. Display only; the blob is the
    -- anchor, and a branch moves.
    ADD COLUMN ref_name   TEXT;

ALTER TABLE issues
    ADD CONSTRAINT issue_blob_is_a_hash
        CHECK (blob IS NULL OR octet_length(blob) = 32),
    -- Half an anchor cannot be rendered and cannot be repaired, so it is all
    -- of it or none of it.
    ADD CONSTRAINT issue_anchor_whole CHECK (
        (file_path IS NULL AND line_start IS NULL AND line_end IS NULL AND blob IS NULL)
        OR (file_path IS NOT NULL AND line_start IS NOT NULL
            AND line_end IS NOT NULL AND blob IS NOT NULL)
    ),
    -- A range runs forwards, and lines are numbered from one.
    ADD CONSTRAINT issue_anchor_range CHECK (
        line_start IS NULL OR (line_start >= 1 AND line_end >= line_start)
    );

-- "Which issues point at this file?" is the question a reader asks, and it is
-- worth answering without a sequential scan once a repository has a few
-- thousand issues.
CREATE INDEX issue_anchor_idx ON issues (repo_id, file_path) WHERE file_path IS NOT NULL;
