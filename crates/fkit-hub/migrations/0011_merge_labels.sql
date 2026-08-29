-- Labels on merge requests.
--
-- The same labels an issue carries: a repository's vocabulary should not
-- fork depending on which kind of thing is being sorted. A second join table
-- rather than a polymorphic column, because a foreign key that only sometimes
-- points at a real row is not a foreign key.
CREATE TABLE merge_labels (
    merge_request_id UUID NOT NULL REFERENCES merge_requests(id) ON DELETE CASCADE,
    label_id         UUID NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (merge_request_id, label_id)
);

CREATE INDEX merge_labels_by_label ON merge_labels (label_id);
