-- When the work was done, as distinct from when it arrived.
--
-- `pushed_at` is the only timestamp this table had, and it is the wrong one to
-- draw a year of activity from. It records delivery. Push a project with five
-- years of history and every one of those commits lands on today; work on a
-- plane for a fortnight and the fortnight is empty while the day you landed is
-- a wall. A graph built on it is not a graph of when somebody worked, and
-- reading it as one gives an answer that is false in both directions.
--
-- So this stores what the commit itself says, taken from the object at push
-- time -- the server has already parsed it to walk the parents, so it is not
-- an extra read.
--
-- # What is and is not known
--
-- These two columns are trustworthy in different ways, and the split is the
-- point rather than an inconsistency.
--
--   user_id      is authenticated. The push carried a session or a token, so
--                the server knows who delivered this, and no author string can
--                argue with it.
--   committed_at is claimed. It is a field inside the commit, which means it
--                is content, which means it is whatever the person who made it
--                decided. Backdating is possible here exactly as it is in git.
--
-- Claimed is still the right answer for this question: it is the only record
-- of when the work happened, an imported history genuinely did happen on the
-- days it says, and the alternative is not a more honest graph but a wrong
-- one. What matters is not pretending it is the other kind of fact.
ALTER TABLE commit_authors
    ADD COLUMN committed_at TIMESTAMPTZ;

COMMENT ON COLUMN commit_authors.committed_at IS
    'The commit''s own timestamp: when the work was done, as claimed by whoever did it. NULL for rows written before this column existed. Distinct from pushed_at, which is when it arrived and is the server''s own observation.';

-- The query the profile graph makes: one person, one year, by when the work
-- was done rather than when it turned up.
CREATE INDEX commit_authors_when_idx
    ON commit_authors (user_id, committed_at DESC);
