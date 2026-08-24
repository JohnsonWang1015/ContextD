//! Project rows.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::core::model::Project;
use crate::error::Result;
use crate::storage::repository::{ProjectRepository, ProjectStats};
use crate::util::time;

use super::SqliteStore;

const COLUMNS: &str = "id, name, slug, root_path, description, git_remote, default_branch, \
                       active, created_at, updated_at";

fn map_project(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        slug: row.get(2)?,
        root_path: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
        description: row.get(4)?,
        git_remote: row.get(5)?,
        default_branch: row.get(6)?,
        active: row.get::<_, i64>(7)? != 0,
        created_at: time::from_storage(&row.get::<_, String>(8)?),
        updated_at: time::from_storage(&row.get::<_, String>(9)?),
    })
}

impl ProjectRepository for SqliteStore {
    fn create_project(&self, project: &Project) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO projects (id, name, slug, root_path, description, git_remote,
                                   default_branch, active, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                project.id,
                project.name,
                project.slug,
                project.root_path.as_deref().map(path_key),
                project.description,
                project.git_remote,
                project.default_branch,
                project.active as i64,
                time::to_storage(&project.created_at),
                time::to_storage(&project.updated_at),
            ],
        )?;
        Ok(())
    }

    fn update_project(&self, project: &Project) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE projects
                SET name = ?2, slug = ?3, root_path = ?4, description = ?5, git_remote = ?6,
                    default_branch = ?7, active = ?8, updated_at = ?9
              WHERE id = ?1",
            params![
                project.id,
                project.name,
                project.slug,
                project.root_path.as_deref().map(path_key),
                project.description,
                project.git_remote,
                project.default_branch,
                project.active as i64,
                time::to_storage(&time::now()),
            ],
        )?;
        Ok(())
    }

    fn get_project(&self, id: &str) -> Result<Option<Project>> {
        let conn = self.conn();
        let sql = format!("SELECT {COLUMNS} FROM projects WHERE id = ?1");
        Ok(conn.query_row(&sql, params![id], map_project).optional()?)
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<Option<Project>> {
        let conn = self.conn();
        let sql = format!("SELECT {COLUMNS} FROM projects WHERE slug = ?1");
        Ok(conn.query_row(&sql, params![slug], map_project).optional()?)
    }

    /// Innermost project containing `path`.
    ///
    /// Nested repositories are legitimate (a workspace with its own ContextD
    /// project inside a monorepo), so the *longest* matching root wins rather
    /// than the first one found.
    fn find_project_by_path(&self, path: &Path) -> Result<Option<Project>> {
        let target = normalise(path);
        let candidates = self.list_projects(true)?;
        Ok(candidates
            .into_iter()
            .filter_map(|p| {
                let root = normalise(p.root_path.as_ref()?);
                target.starts_with(&root).then(|| (root.components().count(), p))
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, p)| p))
    }

    fn list_projects(&self, include_inactive: bool) -> Result<Vec<Project>> {
        let conn = self.conn();
        let sql = format!(
            "SELECT {COLUMNS} FROM projects {} ORDER BY active DESC, name COLLATE NOCASE",
            if include_inactive { "" } else { "WHERE active = 1" }
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], map_project)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn delete_project(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        // Search rows are not foreign-keyed to the base tables (FTS5 virtual
        // tables cannot be), so they are cleaned up explicitly.
        tx.execute(
            "DELETE FROM search_index WHERE project_id = ?1
                OR record_id IN (SELECT id FROM checkpoints WHERE project_id = ?1)
                OR record_id IN (SELECT id FROM architecture_decisions WHERE project_id = ?1)
                OR record_id IN (SELECT id FROM memories WHERE project_id = ?1)",
            params![id],
        )?;
        tx.execute(
            "DELETE FROM embeddings WHERE record_id IN (
                 SELECT id FROM memories WHERE project_id = ?1
                 UNION ALL SELECT id FROM architecture_decisions WHERE project_id = ?1
                 UNION ALL SELECT id FROM checkpoints WHERE project_id = ?1)",
            params![id],
        )?;
        let removed = tx.execute("DELETE FROM projects WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(removed > 0)
    }

    fn project_stats(&self, id: &str) -> Result<ProjectStats> {
        let conn = self.conn();
        Ok(ProjectStats {
            memories: scalar(&conn, "SELECT count(*) FROM memories WHERE project_id = ?1", id)?,
            active_memories: scalar(
                &conn,
                "SELECT count(*) FROM memories WHERE project_id = ?1 AND status = 'active'",
                id,
            )?,
            superseded_memories: scalar(
                &conn,
                "SELECT count(*) FROM memories WHERE project_id = ?1
                   AND status IN ('superseded', 'deprecated')",
                id,
            )?,
            decisions: scalar(
                &conn,
                "SELECT count(*) FROM architecture_decisions WHERE project_id = ?1",
                id,
            )?,
            checkpoints: scalar(
                &conn,
                "SELECT count(*) FROM checkpoints WHERE project_id = ?1",
                id,
            )?,
            sessions: scalar(&conn, "SELECT count(*) FROM sessions WHERE project_id = ?1", id)?,
            embedded_records: scalar(
                &conn,
                "SELECT count(*) FROM embeddings WHERE record_id IN (
                     SELECT id FROM memories WHERE project_id = ?1
                     UNION ALL SELECT id FROM architecture_decisions WHERE project_id = ?1
                     UNION ALL SELECT id FROM checkpoints WHERE project_id = ?1)",
                id,
            )?,
        })
    }
}

fn scalar(conn: &Connection, sql: &str, id: &str) -> Result<usize> {
    let n: i64 = conn.query_row(sql, params![id], |row| row.get(0))?;
    Ok(n as usize)
}

/// Canonical string form used in the `root_path` column.
fn path_key(path: &Path) -> String {
    normalise(path).to_string_lossy().into_owned()
}

/// Normalise a path for comparison: resolve symlinks when possible, and strip
/// Windows verbatim prefixes so `\\?\C:\x` and `C:\x` compare equal.
fn normalise(path: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => resolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::Project;
    use chrono::Utc;

    fn project(name: &str, root: Option<&Path>) -> Project {
        Project {
            id: crate::util::ids::new_id(),
            name: name.to_string(),
            slug: crate::util::ids::slugify(name),
            root_path: root.map(Path::to_path_buf),
            description: None,
            git_remote: None,
            default_branch: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            active: true,
        }
    }

    #[test]
    fn crud_roundtrip() {
        let store = SqliteStore::open_in_memory().unwrap();
        let mut p = project("FerroGrid", None);
        store.create_project(&p).unwrap();

        let loaded = store.get_project(&p.id).unwrap().unwrap();
        assert_eq!(loaded.name, "FerroGrid");
        assert_eq!(loaded.slug, "ferrogrid");

        p.description = Some("GPU scheduler".into());
        store.update_project(&p).unwrap();
        assert_eq!(
            store.find_project_by_slug("ferrogrid").unwrap().unwrap().description.as_deref(),
            Some("GPU scheduler")
        );

        assert!(store.delete_project(&p.id).unwrap());
        assert!(store.get_project(&p.id).unwrap().is_none());
    }

    #[test]
    fn slug_is_unique() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_project(&project("Dup", None)).unwrap();
        assert!(store.create_project(&project("Dup", None)).is_err());
    }

    #[test]
    fn innermost_project_wins_for_nested_roots() {
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().to_path_buf();
        let inner = outer.join("crates/inner");
        std::fs::create_dir_all(&inner).unwrap();

        let store = SqliteStore::open_in_memory().unwrap();
        store.create_project(&project("Outer", Some(&outer))).unwrap();
        store.create_project(&project("Inner", Some(&inner))).unwrap();

        let found = store.find_project_by_path(&inner.join("src")).unwrap().unwrap();
        assert_eq!(found.name, "Inner");
        let found = store.find_project_by_path(&outer).unwrap().unwrap();
        assert_eq!(found.name, "Outer");
    }

    #[test]
    fn unrelated_path_finds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_project(&project("P", Some(dir.path()))).unwrap();
        assert!(store.find_project_by_path(Path::new("/definitely/not/here")).unwrap().is_none());
    }

    #[test]
    fn inactive_projects_are_hidden_by_default() {
        let store = SqliteStore::open_in_memory().unwrap();
        let mut p = project("Old", None);
        p.active = false;
        store.create_project(&p).unwrap();
        assert!(store.list_projects(false).unwrap().is_empty());
        assert_eq!(store.list_projects(true).unwrap().len(), 1);
    }
}
