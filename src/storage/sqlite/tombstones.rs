//! Tombstone rows.

use rusqlite::{params, OptionalExtension, Row};

use crate::core::model::{RecordKind, RecordRef, Tombstone};
use crate::error::Result;
use crate::storage::repository::{ProjectScope, TombstoneRepository};
use crate::util::time;

use super::SqliteStore;

const COLUMNS: &str = "record_kind, record_id, project_id, deleted_at";

fn map_tombstone(row: &Row<'_>) -> rusqlite::Result<Tombstone> {
    let kind: String = row.get(0)?;
    Ok(Tombstone {
        record: RecordRef::new(
            kind.parse().unwrap_or(RecordKind::Memory),
            row.get::<_, String>(1)?,
        ),
        project_id: row.get(2)?,
        deleted_at: time::from_storage(&row.get::<_, String>(3)?),
    })
}

/// Record a deletion. Shared with the delete paths, which call it inside their
/// own transaction so a record can never vanish without its tombstone.
pub(crate) fn insert(
    conn: &rusqlite::Connection,
    record: &RecordRef,
    project_id: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO tombstones (record_kind, record_id, project_id, deleted_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (record_kind, record_id) DO NOTHING",
        params![record.kind.as_str(), record.id, project_id, time::to_storage(&time::now()),],
    )?;
    Ok(())
}

impl TombstoneRepository for SqliteStore {
    fn record_tombstone(&self, tombstone: &Tombstone) -> Result<()> {
        let conn = self.conn();
        // The earliest deletion wins: once one machine has agreed a record is
        // gone, a later re-import of the same tombstone must not move the
        // timestamp forward and start overriding edits made before it.
        conn.execute(
            "INSERT INTO tombstones (record_kind, record_id, project_id, deleted_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (record_kind, record_id) DO UPDATE SET
                deleted_at = min(deleted_at, excluded.deleted_at),
                project_id = coalesce(project_id, excluded.project_id)",
            params![
                tombstone.record.kind.as_str(),
                tombstone.record.id,
                tombstone.project_id,
                time::to_storage(&tombstone.deleted_at),
            ],
        )?;
        Ok(())
    }

    fn tombstones(
        &self,
        scope: &ProjectScope,
        since: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<Tombstone>> {
        let (scope_sql, scope_param) = scope.sql("project_id");
        let conn = self.conn();
        let sql = format!(
            "SELECT {COLUMNS} FROM tombstones WHERE {scope_sql} AND deleted_at >= ?
              ORDER BY deleted_at"
        );
        let mut stmt = conn.prepare(&sql)?;
        let floor = since.map(|s| time::to_storage(&s)).unwrap_or_default();
        let rows = match scope_param {
            Some(project) => stmt
                .query_map(params![project, floor], map_tombstone)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt
                .query_map(params![floor], map_tombstone)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    fn tombstone_for(&self, record: &RecordRef) -> Result<Option<Tombstone>> {
        let conn = self.conn();
        let sql =
            format!("SELECT {COLUMNS} FROM tombstones WHERE record_kind = ?1 AND record_id = ?2");
        Ok(conn
            .query_row(&sql, params![record.kind.as_str(), record.id], map_tombstone)
            .optional()?)
    }

    fn purge_tombstones(&self, before: chrono::DateTime<chrono::Utc>) -> Result<usize> {
        let conn = self.conn();
        Ok(conn.execute(
            "DELETE FROM tombstones WHERE deleted_at < ?1",
            params![time::to_storage(&before)],
        )?)
    }

    fn clear_tombstone(&self, record: &RecordRef) -> Result<bool> {
        let conn = self.conn();
        let removed = conn.execute(
            "DELETE FROM tombstones WHERE record_kind = ?1 AND record_id = ?2",
            params![record.kind.as_str(), record.id],
        )?;
        Ok(removed > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{Category, Memory, Project};
    use crate::storage::repository::{MemoryRepository, ProjectRepository};
    use chrono::Duration;

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
    fn deleting_a_memory_leaves_a_tombstone() {
        let (store, project) = setup();
        let mut memory = Memory::new(Category::Architecture, "T", "c");
        memory.project_id = Some(project.id.clone());
        store.create_memory(&memory).unwrap();
        store.delete_memory(&memory.id).unwrap();

        let tombstone = store.tombstone_for(&RecordRef::memory(&memory.id)).unwrap().unwrap();
        assert_eq!(tombstone.project_id.as_deref(), Some(project.id.as_str()));
        assert_eq!(store.tombstones(&ProjectScope::Any, None).unwrap().len(), 1);
    }

    #[test]
    fn re_recording_keeps_the_earliest_deletion() {
        let (store, _project) = setup();
        let record = RecordRef::memory("m1");
        let early = time::now() - Duration::hours(2);
        store
            .record_tombstone(&Tombstone {
                record: record.clone(),
                project_id: None,
                deleted_at: early,
            })
            .unwrap();
        store
            .record_tombstone(&Tombstone {
                record: record.clone(),
                project_id: Some("p1".into()),
                deleted_at: time::now(),
            })
            .unwrap();

        let stored = store.tombstone_for(&record).unwrap().unwrap();
        assert_eq!(stored.deleted_at.timestamp(), early.timestamp());
        assert_eq!(stored.project_id.as_deref(), Some("p1"), "missing detail is filled in");
    }

    #[test]
    fn scope_and_since_filters_apply() {
        let (store, project) = setup();
        store
            .record_tombstone(&Tombstone {
                record: RecordRef::memory("old"),
                project_id: Some(project.id.clone()),
                deleted_at: time::now() - Duration::days(2),
            })
            .unwrap();
        store
            .record_tombstone(&Tombstone {
                record: RecordRef::memory("new"),
                project_id: Some(project.id.clone()),
                deleted_at: time::now(),
            })
            .unwrap();
        store
            .record_tombstone(&Tombstone {
                record: RecordRef::memory("global"),
                project_id: None,
                deleted_at: time::now(),
            })
            .unwrap();

        assert_eq!(store.tombstones(&ProjectScope::Any, None).unwrap().len(), 3);
        assert_eq!(
            store.tombstones(&ProjectScope::Project(project.id.clone()), None).unwrap().len(),
            2
        );
        assert_eq!(store.tombstones(&ProjectScope::GlobalOnly, None).unwrap().len(), 1);
        assert_eq!(
            store
                .tombstones(&ProjectScope::Any, Some(time::now() - Duration::hours(1)))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn purging_and_clearing() {
        let (store, _project) = setup();
        store
            .record_tombstone(&Tombstone {
                record: RecordRef::memory("old"),
                project_id: None,
                deleted_at: time::now() - Duration::days(400),
            })
            .unwrap();
        let recent = RecordRef::memory("recent");
        store
            .record_tombstone(&Tombstone {
                record: recent.clone(),
                project_id: None,
                deleted_at: time::now(),
            })
            .unwrap();

        assert_eq!(store.purge_tombstones(time::now() - Duration::days(365)).unwrap(), 1);
        assert_eq!(store.tombstones(&ProjectScope::Any, None).unwrap().len(), 1);

        assert!(store.clear_tombstone(&recent).unwrap());
        assert!(!store.clear_tombstone(&recent).unwrap());
        assert!(store.tombstones(&ProjectScope::Any, None).unwrap().is_empty());
    }
}
