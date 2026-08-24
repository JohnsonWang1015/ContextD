//! Work sessions: which agent worked on what, and when.

use rusqlite::{params, OptionalExtension, Row};

use crate::core::model::Session;
use crate::error::Result;
use crate::storage::repository::SessionRepository;
use crate::util::time;

use super::SqliteStore;

const COLUMNS: &str = "id, project_id, agent, started_at, ended_at, summary";

fn map_session(row: &Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        project_id: row.get(1)?,
        agent: row.get(2)?,
        started_at: time::from_storage(&row.get::<_, String>(3)?),
        ended_at: row.get::<_, Option<String>>(4)?.map(|s| time::from_storage(&s)),
        summary: row.get(5)?,
    })
}

impl SessionRepository for SqliteStore {
    fn start_session(&self, session: &Session) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO sessions (id, project_id, agent, started_at, ended_at, summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session.id,
                session.project_id,
                session.agent,
                time::to_storage(&session.started_at),
                session.ended_at.as_ref().map(time::to_storage),
                session.summary,
            ],
        )?;
        Ok(())
    }

    fn end_session(&self, id: &str, summary: Option<&str>) -> Result<bool> {
        let conn = self.conn();
        let changed = conn.execute(
            "UPDATE sessions SET ended_at = ?2, summary = coalesce(?3, summary) WHERE id = ?1",
            params![id, time::to_storage(&time::now()), summary],
        )?;
        Ok(changed > 0)
    }

    fn latest_session(&self, project_id: &str) -> Result<Option<Session>> {
        let conn = self.conn();
        let sql = format!(
            "SELECT {COLUMNS} FROM sessions WHERE project_id = ?1
              ORDER BY started_at DESC, rowid DESC LIMIT 1"
        );
        Ok(conn.query_row(&sql, params![project_id], map_session).optional()?)
    }

    fn list_sessions(&self, project_id: &str, limit: usize) -> Result<Vec<Session>> {
        let conn = self.conn();
        let sql = format!(
            "SELECT {COLUMNS} FROM sessions WHERE project_id = ?1
              ORDER BY started_at DESC, rowid DESC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![project_id, limit as i64], map_session)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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

    #[test]
    fn start_and_end() {
        let (store, project) = setup();
        let session = Session {
            id: crate::util::ids::new_id(),
            project_id: project.id.clone(),
            agent: Some("claude".into()),
            started_at: time::now(),
            ended_at: None,
            summary: None,
        };
        store.start_session(&session).unwrap();
        assert!(store.latest_session(&project.id).unwrap().unwrap().ended_at.is_none());

        assert!(store.end_session(&session.id, Some("done")).unwrap());
        let loaded = store.latest_session(&project.id).unwrap().unwrap();
        assert!(loaded.ended_at.is_some());
        assert_eq!(loaded.summary.as_deref(), Some("done"));
        assert_eq!(store.list_sessions(&project.id, 5).unwrap().len(), 1);
    }

    #[test]
    fn ending_unknown_session_reports_false() {
        let (store, _) = setup();
        assert!(!store.end_session("nope", None).unwrap());
    }
}
