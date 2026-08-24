-- Sessions become first-class: a checkpoint records which working session it
-- was made in, so "what happened while Claude Code was connected on Tuesday"
-- is answerable.
--
-- The column is nullable and set to NULL when its session is deleted: a
-- checkpoint is meaningful on its own, and losing the session it belonged to
-- must never lose the checkpoint.
ALTER TABLE checkpoints ADD COLUMN session_id TEXT REFERENCES sessions(id) ON DELETE SET NULL;

CREATE INDEX idx_checkpoints_session ON checkpoints(session_id);

-- Finding the open session for a project is the hottest session query there
-- is: every checkpoint and every status line asks for it.
CREATE INDEX idx_sessions_open ON sessions(project_id, ended_at);
