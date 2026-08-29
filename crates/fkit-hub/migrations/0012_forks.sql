-- Forks.
--
-- A fork shares its parent's object store rather than copying it.
--
-- This is what content addressing buys. An object's name *is* a digest of its
-- bytes, so two repositories cannot disagree about what a hash means: sharing
-- a store between them is safe by construction, not by convention. Forking is
-- then O(1) on disk however large the repository, and a merge request across
-- two forks needs no transfer at all, because both sides' commits already
-- resolve in the same store.
--
-- `network_id` is the root of the fork tree and denormalised on purpose: both
-- "which directory holds these objects" and "whose refs must garbage
-- collection treat as roots" are answered by one indexed equality rather than
-- by walking a parent chain.
ALTER TABLE repos
    ADD COLUMN forked_from UUID REFERENCES repos(id) ON DELETE SET NULL,
    ADD COLUMN network_id  UUID;

-- Every repository that exists today is its own network.
UPDATE repos SET network_id = id WHERE network_id IS NULL;

ALTER TABLE repos
    ALTER COLUMN network_id SET NOT NULL,
    ADD CONSTRAINT network_is_a_repo FOREIGN KEY (network_id) REFERENCES repos(id);

-- The query garbage collection makes: every repository sharing this store.
CREATE INDEX repos_network_idx ON repos (network_id);
CREATE INDEX repos_forked_from_idx ON repos (forked_from) WHERE forked_from IS NOT NULL;

-- A merge request may propose a branch that lives in another repository of the
-- same network. Null means the same repository, which is what every existing
-- row is.
ALTER TABLE merge_requests
    ADD COLUMN source_repo_id UUID REFERENCES repos(id) ON DELETE CASCADE;

CREATE INDEX mr_source_repo_idx ON merge_requests (source_repo_id)
    WHERE source_repo_id IS NOT NULL;

-- The "one open request per branch pair" rule has to account for which
-- repository the source branch is in, or two forks proposing their own `main`
-- into the same target would collide.
DROP INDEX IF EXISTS mr_one_open_per_pair;
CREATE UNIQUE INDEX mr_one_open_per_pair
    ON merge_requests (repo_id, COALESCE(source_repo_id, repo_id), source_branch, target_branch)
    WHERE state = 'open';
