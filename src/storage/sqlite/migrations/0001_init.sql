-- Core schema: projects, memories and everything hanging off them.
--
-- Conventions:
--   * ids are UUIDv4 text
--   * timestamps are UTC RFC3339 text (sorts lexicographically = chronologically)
--   * list-valued fields are JSON arrays in TEXT columns, except tags which
--     get their own table because they are queried and aggregated

CREATE TABLE projects (
    id             TEXT PRIMARY KEY,
    name           TEXT NOT NULL,
    slug           TEXT NOT NULL UNIQUE,
    root_path      TEXT UNIQUE,
    description    TEXT,
    git_remote     TEXT,
    default_branch TEXT,
    active         INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);

CREATE INDEX idx_projects_active ON projects(active);

-- project_id NULL means the memory is global (applies to every project).
CREATE TABLE memories (
    id            TEXT PRIMARY KEY,
    project_id    TEXT REFERENCES projects(id) ON DELETE CASCADE,
    category      TEXT NOT NULL,
    title         TEXT NOT NULL,
    content       TEXT NOT NULL,
    source        TEXT NOT NULL DEFAULT 'manual',
    priority      INTEGER NOT NULL DEFAULT 3 CHECK (priority BETWEEN 1 AND 5),
    status        TEXT NOT NULL DEFAULT 'active'
                  CHECK (status IN ('active', 'superseded', 'deprecated', 'archived')),
    superseded_by TEXT REFERENCES memories(id) ON DELETE SET NULL,
    related_files TEXT NOT NULL DEFAULT '[]',
    commit_hash   TEXT,
    symbol        TEXT,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);

CREATE INDEX idx_memories_project  ON memories(project_id);
CREATE INDEX idx_memories_category ON memories(category);
CREATE INDEX idx_memories_status   ON memories(status);
CREATE INDEX idx_memories_updated  ON memories(updated_at DESC);

CREATE TABLE memory_tags (
    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    tag       TEXT NOT NULL,
    PRIMARY KEY (memory_id, tag)
);

CREATE INDEX idx_memory_tags_tag ON memory_tags(tag);

CREATE TABLE checkpoints (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    summary       TEXT NOT NULL,
    current_goal  TEXT,
    completed     TEXT NOT NULL DEFAULT '[]',
    current_state TEXT,
    next_steps    TEXT NOT NULL DEFAULT '[]',
    open_problems TEXT NOT NULL DEFAULT '[]',
    related_files TEXT NOT NULL DEFAULT '[]',
    git_branch    TEXT,
    git_commit    TEXT,
    dirty_files   TEXT NOT NULL DEFAULT '[]',
    created_at    TEXT NOT NULL
);

CREATE INDEX idx_checkpoints_project ON checkpoints(project_id, created_at DESC);

CREATE TABLE architecture_decisions (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    title         TEXT NOT NULL,
    context       TEXT,
    decision      TEXT NOT NULL,
    consequences  TEXT,
    alternatives  TEXT NOT NULL DEFAULT '[]',
    status        TEXT NOT NULL DEFAULT 'accepted'
                  CHECK (status IN ('proposed', 'accepted', 'superseded', 'rejected')),
    supersedes    TEXT REFERENCES architecture_decisions(id) ON DELETE SET NULL,
    superseded_by TEXT REFERENCES architecture_decisions(id) ON DELETE SET NULL,
    decided_at    TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);

CREATE INDEX idx_decisions_project ON architecture_decisions(project_id, decided_at DESC);
CREATE INDEX idx_decisions_status  ON architecture_decisions(status);

CREATE TABLE sessions (
    id         TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    agent      TEXT,
    started_at TEXT NOT NULL,
    ended_at   TEXT,
    summary    TEXT
);

CREATE INDEX idx_sessions_project ON sessions(project_id, started_at DESC);

-- One row per (project, agent, file): where ContextD reads context from and
-- writes it back to. last_hash is what ContextD last wrote, which is how a
-- hand-edited CLAUDE.md is detected before it would be overwritten.
CREATE TABLE agent_bindings (
    id               TEXT PRIMARY KEY,
    project_id       TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    agent            TEXT NOT NULL,
    path             TEXT NOT NULL,
    last_hash        TEXT,
    last_exported_at TEXT,
    last_imported_at TEXT,
    UNIQUE (project_id, agent, path)
);

CREATE INDEX idx_bindings_project ON agent_bindings(project_id);

-- Vectors are stored as little-endian f32 blobs. Provider/model/dimensions are
-- recorded per row so switching provider is detectable and re-embedding can be
-- incremental rather than wholesale.
CREATE TABLE embeddings (
    record_kind  TEXT NOT NULL,
    record_id    TEXT NOT NULL,
    provider     TEXT NOT NULL,
    model        TEXT NOT NULL,
    dimensions   INTEGER NOT NULL,
    vector       BLOB NOT NULL,
    content_hash TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    PRIMARY KEY (record_kind, record_id)
);

CREATE INDEX idx_embeddings_model ON embeddings(provider, model);
