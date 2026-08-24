//! Checkpoint rows.

use rusqlite::{params, OptionalExtension, Row};

use crate::core::model::{Checkpoint, RecordKind, RecordRef};
use crate::error::Result;
use crate::storage::repository::{CheckpointRepository, IndexableRecord};
use crate::util::time;

use super::{from_json_list, to_json_list, SqliteStore};

const COLUMNS: &str = "id, project_id, summary, current_goal, completed, current_state, \
                       next_steps, open_problems, related_files, git_branch, git_commit, \
                       dirty_files, created_at";

fn map_checkpoint(row: &Row<'_>) -> rusqlite::Result<Checkpoint> {
    Ok(Checkpoint {
        id: row.get(0)?,
        project_id: row.get(1)?,
        summary: row.get(2)?,
        current_goal: row.get(3)?,
        completed: from_json_list(&row.get::<_, String>(4)?),
        current_state: row.get(5)?,
        next_steps: from_json_list(&row.get::<_, String>(6)?),
        open_problems: from_json_list(&row.get::<_, String>(7)?),
        related_files: from_json_list(&row.get::<_, String>(8)?),
        git_branch: row.get(9)?,
        git_commit: row.get(10)?,
        dirty_files: from_json_list(&row.get::<_, String>(11)?),
        created_at: time::from_storage(&row.get::<_, String>(12)?),
    })
}

impl CheckpointRepository for SqliteStore {
    fn create_checkpoint(&self, checkpoint: &Checkpoint) -> Result<()> {
        if checkpoint.summary.trim().is_empty() {
            return Err(crate::error::Error::invalid("summary", "must not be empty"));
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO checkpoints (id, project_id, summary, current_goal, completed,
                                      current_state, next_steps, open_problems, related_files,
                                      git_branch, git_commit, dirty_files, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                checkpoint.id,
                checkpoint.project_id,
                checkpoint.summary,
                checkpoint.current_goal,
                to_json_list(&checkpoint.completed),
                checkpoint.current_state,
                to_json_list(&checkpoint.next_steps),
                to_json_list(&checkpoint.open_problems),
                to_json_list(&checkpoint.related_files),
                checkpoint.git_branch,
                checkpoint.git_commit,
                to_json_list(&checkpoint.dirty_files),
                time::to_storage(&checkpoint.created_at),
            ],
        )?;
        super::fts::index_record(
            &tx,
            &IndexableRecord {
                record: RecordRef::checkpoint(&checkpoint.id),
                project_id: Some(checkpoint.project_id.clone()),
                title: checkpoint.summary.clone(),
                body: checkpoint.indexable_text(),
                tags: Vec::new(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    fn latest_checkpoint(&self, project_id: &str) -> Result<Option<Checkpoint>> {
        let conn = self.conn();
        let sql = format!(
            "SELECT {COLUMNS} FROM checkpoints WHERE project_id = ?1
              ORDER BY created_at DESC, rowid DESC LIMIT 1"
        );
        Ok(conn.query_row(&sql, params![project_id], map_checkpoint).optional()?)
    }

    fn list_checkpoints(&self, project_id: &str, limit: usize) -> Result<Vec<Checkpoint>> {
        let conn = self.conn();
        let sql = format!(
            "SELECT {COLUMNS} FROM checkpoints WHERE project_id = ?1
              ORDER BY created_at DESC, rowid DESC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![project_id, limit as i64], map_checkpoint)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn get_checkpoint(&self, id: &str) -> Result<Option<Checkpoint>> {
        let conn = self.conn();
        let sql = format!("SELECT {COLUMNS} FROM checkpoints WHERE id = ?1");
        Ok(conn.query_row(&sql, params![id], map_checkpoint).optional()?)
    }

    fn delete_checkpoint(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        super::fts::delete_record(&tx, &RecordRef::checkpoint(id))?;
        tx.execute(
            "DELETE FROM embeddings WHERE record_kind = ?1 AND record_id = ?2",
            params![RecordKind::Checkpoint.as_str(), id],
        )?;
        let removed = tx.execute("DELETE FROM checkpoints WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(removed > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::Project;
    use crate::storage::repository::ProjectRepository;
    use chrono::Duration;

    fn setup() -> (SqliteStore, Project) {
        let store = SqliteStore::open_in_memory().unwrap();
        let project = Project {
            id: crate::util::ids::new_id(),
            name: "FerroGrid".into(),
            slug: "ferrogrid".into(),
            root_path: None,
            description: None,
            git_remote: None,
            default_branch: None,
            created_at: time::now(),
            updated_at: time::now(),
            active: true,
        };
        store.create_project(&project).unwrap();
        (store, project)
    }

    #[test]
    fn roundtrip_preserves_lists() {
        let (store, project) = setup();
        let mut cp = Checkpoint::new(&project.id, "worker heartbeat completed");
        cp.current_goal = Some("Implement distributed GPU scheduling".into());
        cp.completed = vec!["Coordinator".into(), "Worker registration".into()];
        cp.next_steps = vec!["Lease-based GPU allocation".into()];
        cp.open_problems = vec!["Worker reconnect".into()];
        cp.git_branch = Some("main".into());
        store.create_checkpoint(&cp).unwrap();

        let loaded = store.latest_checkpoint(&project.id).unwrap().unwrap();
        assert_eq!(loaded.completed.len(), 2);
        assert_eq!(loaded.next_steps, vec!["Lease-based GPU allocation".to_string()]);
        assert_eq!(loaded.git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn latest_is_the_newest() {
        let (store, project) = setup();
        let mut old = Checkpoint::new(&project.id, "old");
        old.created_at = time::now() - Duration::hours(3);
        let newer = Checkpoint::new(&project.id, "newer");
        store.create_checkpoint(&old).unwrap();
        store.create_checkpoint(&newer).unwrap();
        assert_eq!(store.latest_checkpoint(&project.id).unwrap().unwrap().summary, "newer");
        assert_eq!(store.list_checkpoints(&project.id, 10).unwrap().len(), 2);
    }

    #[test]
    fn empty_summary_is_rejected() {
        let (store, project) = setup();
        assert!(store.create_checkpoint(&Checkpoint::new(&project.id, "   ")).is_err());
    }

    #[test]
    fn no_checkpoint_yet() {
        let (store, project) = setup();
        assert!(store.latest_checkpoint(&project.id).unwrap().is_none());
    }
}
