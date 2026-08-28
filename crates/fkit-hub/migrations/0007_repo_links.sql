-- A repository's homepage and topics, for the About panel.

ALTER TABLE repos
    -- Where the thing this repository builds actually lives. Rendered as a
    -- link, so the scheme is constrained to http/https in the API: a
    -- javascript: URL stored here would execute in a visitor's session.
    ADD COLUMN homepage TEXT   NOT NULL DEFAULT '',
    -- Free-form labels. An array rather than a join table because they are
    -- only ever read as a whole and written as a whole.
    ADD COLUMN topics   TEXT[] NOT NULL DEFAULT '{}';

-- Finding every repository on a topic is the only query this needs to serve.
CREATE INDEX repos_topics_idx ON repos USING GIN (topics);
