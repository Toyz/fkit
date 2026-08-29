-- What may be done to a branch, and by whom.
--
-- Write access is a single bit today: you can push, or you cannot. That is the
-- right shape for a scratch branch and the wrong one for the branch everything
-- is cut from, where the dangerous operations are not "push" but "rewrite" and
-- "remove" — the two that destroy work already pushed rather than adding to it.
CREATE TABLE branch_rules (
    id      UUID PRIMARY KEY,
    repo_id UUID NOT NULL REFERENCES repos(id) ON DELETE CASCADE,

    -- A branch name, or a prefix ending in `*` — `main`, `release/*`, `*`.
    -- Stored bare, without the tags/ prefix: these govern branches.
    pattern TEXT NOT NULL CHECK (pattern <> ''),

    -- Refuse a push that is not a fast-forward. This is the one that matters:
    -- a fast-forward only ever adds, so anything already pushed survives it.
    no_force  BOOLEAN NOT NULL DEFAULT TRUE,
    -- Refuse deleting the branch outright.
    no_delete BOOLEAN NOT NULL DEFAULT TRUE,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- One rule per pattern. Two rules for the same name would raise the
    -- question of which wins, and every answer to that is a surprise.
    UNIQUE (repo_id, pattern)
);

CREATE INDEX branch_rules_repo_idx ON branch_rules (repo_id);

COMMENT ON TABLE branch_rules IS
    'Per-branch limits on force-pushing and deletion. The repository owner is '
    'never bound by them: a mirror pushes with the owner''s token and must be '
    'able to rewrite whatever it is mirroring.';
