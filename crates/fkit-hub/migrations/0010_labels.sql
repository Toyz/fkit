-- Labels on issues.
--
-- A label is a repository's own vocabulary — "bug", "storage", "wontfix" —
-- so it is defined per repository rather than globally. Two repositories
-- calling different things "urgent" is normal and not a conflict to resolve.

CREATE TABLE labels (
    id          UUID PRIMARY KEY,
    repo_id     UUID        NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    name        TEXT        NOT NULL,
    -- The hue, 0-359, not a hex colour.
    --
    -- The palette is derived from it at render time against the theme in use,
    -- which is what stops a label picked in the dark theme from being
    -- unreadable in the light one. Storing #ff0000 would store a decision that
    -- was only correct on one background.
    hue         INT         NOT NULL DEFAULT 0 CHECK (hue >= 0 AND hue < 360),
    description TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Case-insensitively unique: "Bug" and "bug" being separate labels is
    -- never what anyone meant.
    CONSTRAINT label_name_per_repo UNIQUE (repo_id, name)
);

CREATE UNIQUE INDEX label_name_ci_idx ON labels (repo_id, lower(name));

CREATE TABLE issue_labels (
    issue_id UUID NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    label_id UUID NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (issue_id, label_id)
);

CREATE INDEX issue_labels_by_label ON issue_labels (label_id);
