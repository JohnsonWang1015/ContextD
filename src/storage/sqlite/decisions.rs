//! Architecture decision records.

use rusqlite::{params, OptionalExtension, Row};

use crate::core::model::{Decision, DecisionStatus, RecordKind, RecordRef};
use crate::error::{Error, Result};
use crate::storage::repository::{DecisionRepository, IndexableRecord};
use crate::util::time;

use super::{from_json_list, placeholders, to_json_list, SqliteStore};

const COLUMNS: &str = "id, project_id, title, context, decision, consequences, alternatives, \
                       status, supersedes, superseded_by, decided_at, created_at, updated_at";

fn map_decision(row: &Row<'_>) -> rusqlite::Result<Decision> {
    Ok(Decision {
        id: row.get(0)?,
        project_id: row.get(1)?,
        title: row.get(2)?,
        context: row.get(3)?,
        decision: row.get(4)?,
        consequences: row.get(5)?,
        alternatives: from_json_list(&row.get::<_, String>(6)?),
        status: row.get::<_, String>(7)?.parse().unwrap_or(DecisionStatus::Accepted),
        supersedes: row.get(8)?,
        superseded_by: row.get(9)?,
        decided_at: time::from_storage(&row.get::<_, String>(10)?),
        created_at: time::from_storage(&row.get::<_, String>(11)?),
        updated_at: time::from_storage(&row.get::<_, String>(12)?),
    })
}

fn indexable(decision: &Decision) -> IndexableRecord {
    IndexableRecord {
        record: RecordRef::decision(&decision.id),
        project_id: Some(decision.project_id.clone()),
        title: decision.title.clone(),
        body: decision.indexable_text(),
        tags: Vec::new(),
    }
}

impl DecisionRepository for SqliteStore {
    fn create_decision(&self, decision: &Decision) -> Result<()> {
        validate(decision)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO architecture_decisions
                (id, project_id, title, context, decision, consequences, alternatives, status,
                 supersedes, superseded_by, decided_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                decision.id,
                decision.project_id,
                decision.title,
                decision.context,
                decision.decision,
                decision.consequences,
                to_json_list(&decision.alternatives),
                decision.status.as_str(),
                decision.supersedes,
                decision.superseded_by,
                time::to_storage(&decision.decided_at),
                time::to_storage(&decision.created_at),
                time::to_storage(&decision.updated_at),
            ],
        )?;
        // A decision that replaces another closes it out in the same
        // transaction: history must never show two current answers.
        if let Some(previous) = &decision.supersedes {
            mark_superseded(&tx, previous, &decision.id)?;
        }
        super::fts::index_record(&tx, &indexable(decision))?;
        tx.commit()?;
        Ok(())
    }

    fn update_decision(&self, decision: &Decision) -> Result<()> {
        validate(decision)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE architecture_decisions
                SET title = ?2, context = ?3, decision = ?4, consequences = ?5,
                    alternatives = ?6, status = ?7, supersedes = ?8, superseded_by = ?9,
                    decided_at = ?10, updated_at = ?11
              WHERE id = ?1",
            params![
                decision.id,
                decision.title,
                decision.context,
                decision.decision,
                decision.consequences,
                to_json_list(&decision.alternatives),
                decision.status.as_str(),
                decision.supersedes,
                decision.superseded_by,
                time::to_storage(&decision.decided_at),
                time::to_storage(&time::now()),
            ],
        )?;
        super::fts::index_record(&tx, &indexable(decision))?;
        tx.commit()?;
        Ok(())
    }

    fn get_decision(&self, id: &str) -> Result<Option<Decision>> {
        let conn = self.conn();
        let sql = format!("SELECT {COLUMNS} FROM architecture_decisions WHERE id = ?1");
        Ok(conn.query_row(&sql, params![id], map_decision).optional()?)
    }

    fn resolve_decision(&self, ident: &str) -> Result<Option<Decision>> {
        let ident = ident.trim();
        if ident.is_empty() {
            return Ok(None);
        }
        if let Some(found) = self.get_decision(ident)? {
            return Ok(Some(found));
        }
        let matches: Vec<String> = {
            let conn = self.conn();
            let mut stmt = conn
                .prepare("SELECT id FROM architecture_decisions WHERE id LIKE ?1 || '%' LIMIT 5")?;
            let ids = stmt
                .query_map(params![ident], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ids
        };
        match matches.len() {
            0 => Ok(None),
            1 => self.get_decision(&matches[0]),
            n => Err(Error::Ambiguous { ident: ident.to_string(), count: n }),
        }
    }

    fn list_decisions(&self, project_id: &str, include_superseded: bool) -> Result<Vec<Decision>> {
        let conn = self.conn();
        let filter = if include_superseded { "" } else { "AND status IN ('accepted', 'proposed')" };
        let sql = format!(
            "SELECT {COLUMNS} FROM architecture_decisions
              WHERE project_id = ?1 {filter}
              ORDER BY decided_at DESC, rowid DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![project_id], map_decision)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn get_decisions(&self, ids: &[String]) -> Result<Vec<Decision>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn();
        let sql = format!(
            "SELECT {COLUMNS} FROM architecture_decisions WHERE id IN ({})",
            placeholders(ids.len())
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt
            .query_map(rusqlite::params_from_iter(ids), map_decision)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.sort_by_key(|d| ids.iter().position(|id| *id == d.id).unwrap_or(usize::MAX));
        Ok(rows)
    }

    fn delete_decision(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        super::fts::delete_record(&tx, &RecordRef::decision(id))?;
        tx.execute(
            "DELETE FROM embeddings WHERE record_kind = ?1 AND record_id = ?2",
            params![RecordKind::Decision.as_str(), id],
        )?;
        let removed =
            tx.execute("DELETE FROM architecture_decisions WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(removed > 0)
    }

    fn supersede_decision(&self, old_id: &str, new_id: &str) -> Result<()> {
        if old_id == new_id {
            return Err(Error::invalid("supersedes", "a decision cannot supersede itself"));
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        mark_superseded(&tx, old_id, new_id)?;
        tx.execute(
            "UPDATE architecture_decisions SET supersedes = ?2, updated_at = ?3 WHERE id = ?1",
            params![new_id, old_id, time::to_storage(&time::now())],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn mark_superseded(conn: &rusqlite::Connection, old_id: &str, new_id: &str) -> Result<()> {
    let changed = conn.execute(
        "UPDATE architecture_decisions
            SET status = 'superseded', superseded_by = ?2, updated_at = ?3
          WHERE id = ?1",
        params![old_id, new_id, time::to_storage(&time::now())],
    )?;
    if changed == 0 {
        return Err(Error::invalid("supersedes", format!("no decision with id `{old_id}`")));
    }
    // Keep the index in step with the status change.
    let mut stmt =
        conn.prepare(&format!("SELECT {COLUMNS} FROM architecture_decisions WHERE id = ?1"))?;
    if let Some(updated) = stmt.query_row(params![old_id], map_decision).optional()? {
        super::fts::index_record(conn, &indexable(&updated))?;
    }
    Ok(())
}

fn validate(decision: &Decision) -> Result<()> {
    if decision.title.trim().is_empty() {
        return Err(Error::invalid("title", "must not be empty"));
    }
    if decision.decision.trim().is_empty() {
        return Err(Error::invalid("decision", "must not be empty"));
    }
    Ok(())
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

    fn decision(project: &Project, title: &str, body: &str) -> Decision {
        let now = time::now();
        Decision {
            id: crate::util::ids::new_id(),
            project_id: project.id.clone(),
            title: title.into(),
            context: None,
            decision: body.into(),
            consequences: None,
            alternatives: vec![],
            status: DecisionStatus::Accepted,
            supersedes: None,
            superseded_by: None,
            decided_at: now,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn superseding_closes_the_previous_decision() {
        let (store, project) = setup();
        let redis = decision(&project, "Task queue", "Use Redis");
        store.create_decision(&redis).unwrap();

        let mut nats = decision(&project, "Task queue", "Use NATS");
        nats.supersedes = Some(redis.id.clone());
        store.create_decision(&nats).unwrap();

        let old = store.get_decision(&redis.id).unwrap().unwrap();
        assert_eq!(old.status, DecisionStatus::Superseded);
        assert_eq!(old.superseded_by.as_deref(), Some(nats.id.as_str()));

        let current = store.list_decisions(&project.id, false).unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].decision, "Use NATS");
        assert_eq!(store.list_decisions(&project.id, true).unwrap().len(), 2);
    }

    #[test]
    fn superseding_a_missing_decision_fails() {
        let (store, project) = setup();
        let mut d = decision(&project, "T", "b");
        d.supersedes = Some("nope".into());
        assert!(store.create_decision(&d).is_err());
        // The failed transaction must not have left the row behind.
        assert!(store.get_decision(&d.id).unwrap().is_none());
    }

    #[test]
    fn empty_fields_are_rejected() {
        let (store, project) = setup();
        assert!(store.create_decision(&decision(&project, "", "b")).is_err());
        assert!(store.create_decision(&decision(&project, "t", "  ")).is_err());
    }

    #[test]
    fn resolve_by_prefix_and_delete() {
        let (store, project) = setup();
        let d = decision(&project, "Transport", "NATS");
        store.create_decision(&d).unwrap();
        assert_eq!(store.resolve_decision(&d.id[..8]).unwrap().unwrap().id, d.id);
        assert!(store.delete_decision(&d.id).unwrap());
        assert!(store.get_decision(&d.id).unwrap().is_none());
    }
}
