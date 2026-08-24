//! Agent bindings: the files ContextD reads context from and writes it back to.

use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension, Row};

use crate::core::model::AgentBinding;
use crate::error::Result;
use crate::storage::repository::AgentBindingRepository;
use crate::util::time;

use super::SqliteStore;

const COLUMNS: &str = "id, project_id, agent, path, last_hash, last_exported_at, last_imported_at";

fn map_binding(row: &Row<'_>) -> rusqlite::Result<AgentBinding> {
    Ok(AgentBinding {
        id: row.get(0)?,
        project_id: row.get(1)?,
        agent: row.get(2)?,
        path: PathBuf::from(row.get::<_, String>(3)?),
        last_hash: row.get(4)?,
        last_exported_at: row.get::<_, Option<String>>(5)?.map(|s| time::from_storage(&s)),
        last_imported_at: row.get::<_, Option<String>>(6)?.map(|s| time::from_storage(&s)),
    })
}

impl AgentBindingRepository for SqliteStore {
    /// Insert or update by `(project, agent, path)`.
    ///
    /// The timestamps are merged rather than replaced: an export must not
    /// erase the record of the last import, since conflict detection uses both.
    fn upsert_binding(&self, binding: &AgentBinding) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO agent_bindings
                (id, project_id, agent, path, last_hash, last_exported_at, last_imported_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (project_id, agent, path) DO UPDATE SET
                last_hash        = coalesce(excluded.last_hash, last_hash),
                last_exported_at = coalesce(excluded.last_exported_at, last_exported_at),
                last_imported_at = coalesce(excluded.last_imported_at, last_imported_at)",
            params![
                binding.id,
                binding.project_id,
                binding.agent,
                binding.path.to_string_lossy(),
                binding.last_hash,
                binding.last_exported_at.as_ref().map(time::to_storage),
                binding.last_imported_at.as_ref().map(time::to_storage),
            ],
        )?;
        Ok(())
    }

    fn list_bindings(&self, project_id: &str) -> Result<Vec<AgentBinding>> {
        let conn = self.conn();
        let sql = format!(
            "SELECT {COLUMNS} FROM agent_bindings WHERE project_id = ?1 ORDER BY agent, path"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![project_id], map_binding)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn find_binding(
        &self,
        project_id: &str,
        agent: &str,
        path: &Path,
    ) -> Result<Option<AgentBinding>> {
        let conn = self.conn();
        let sql = format!(
            "SELECT {COLUMNS} FROM agent_bindings
              WHERE project_id = ?1 AND agent = ?2 AND path = ?3"
        );
        Ok(conn
            .query_row(&sql, params![project_id, agent, path.to_string_lossy()], map_binding)
            .optional()?)
    }

    fn delete_binding(&self, id: &str) -> Result<bool> {
        let conn = self.conn();
        let removed = conn.execute("DELETE FROM agent_bindings WHERE id = ?1", params![id])?;
        Ok(removed > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::Project;
    use crate::storage::repository::ProjectRepository;

    fn setup() -> (SqliteStore, Project) {
        let store = SqliteStore::open_in_memory().unwrap();
        let project = Project {
            id: crate::util::ids::new_id(),
            name: "P".into(),
            slug: "p".into(),
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

    fn binding(project: &Project) -> AgentBinding {
        AgentBinding {
            id: crate::util::ids::new_id(),
            project_id: project.id.clone(),
            agent: "claude".into(),
            path: PathBuf::from("/repo/CLAUDE.md"),
            last_hash: None,
            last_exported_at: None,
            last_imported_at: None,
        }
    }

    #[test]
    fn upsert_is_idempotent_per_path() {
        let (store, project) = setup();
        let mut b = binding(&project);
        b.last_imported_at = Some(time::now());
        store.upsert_binding(&b).unwrap();

        let mut export = binding(&project);
        export.last_hash = Some("abc".into());
        export.last_exported_at = Some(time::now());
        store.upsert_binding(&export).unwrap();

        let all = store.list_bindings(&project.id).unwrap();
        assert_eq!(all.len(), 1, "same path must not duplicate");
        let stored = &all[0];
        assert_eq!(stored.last_hash.as_deref(), Some("abc"));
        assert!(stored.last_imported_at.is_some(), "import time must survive an export");
        assert!(stored.last_exported_at.is_some());
    }

    #[test]
    fn find_and_delete() {
        let (store, project) = setup();
        let b = binding(&project);
        store.upsert_binding(&b).unwrap();
        assert!(store
            .find_binding(&project.id, "claude", Path::new("/repo/CLAUDE.md"))
            .unwrap()
            .is_some());
        assert!(store.delete_binding(&b.id).unwrap());
        assert!(store.list_bindings(&project.id).unwrap().is_empty());
    }
}
