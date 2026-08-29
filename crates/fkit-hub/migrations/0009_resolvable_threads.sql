-- Resolving a line comment.
--
-- A review comment is a question, and a merge request should not land with
-- questions outstanding. Marking one resolved is how the person who asked, or
-- the person who answered, says it has been dealt with.
--
-- The state lives on the comment rather than in a threads table because a
-- thread here is not an object: it is every comment sharing one anchor. Adding
-- a table to hold what the anchor already identifies would mean keeping the
-- two in step forever.
ALTER TABLE comments
    ADD COLUMN resolved_at TIMESTAMPTZ,
    ADD COLUMN resolved_by UUID REFERENCES users(id) ON DELETE SET NULL;

-- Only a line comment can be resolved: a remark about the change as a whole
-- has nothing to be resolved against.
ALTER TABLE comments
    ADD CONSTRAINT only_lines_resolve
    CHECK (resolved_at IS NULL OR blob IS NOT NULL);

-- The question the merge path asks: does this request still have any?
CREATE INDEX comment_unresolved_idx
    ON comments (merge_request_id)
    WHERE blob IS NOT NULL AND resolved_at IS NULL;
