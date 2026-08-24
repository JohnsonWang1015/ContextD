-- Deletions have to travel between machines, or a memory deleted on the laptop
-- comes back on the next sync from the desktop, which still has it.
--
-- A tombstone is the record that a record was deleted, and when. It carries no
-- foreign key: the row it refers to is gone by definition, and the project it
-- belonged to may be gone too.
CREATE TABLE tombstones (
    record_kind TEXT NOT NULL,
    record_id   TEXT NOT NULL,
    project_id  TEXT,
    deleted_at  TEXT NOT NULL,
    PRIMARY KEY (record_kind, record_id)
);

-- Bundles carry tombstones since a given instant, exactly like records.
CREATE INDEX idx_tombstones_deleted_at ON tombstones(deleted_at);
CREATE INDEX idx_tombstones_project ON tombstones(project_id);
