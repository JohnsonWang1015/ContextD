//! Vector storage.
//!
//! Vectors are little-endian `f32` blobs. SQLite has no vector type and
//! ContextD deliberately avoids an extension dependency: for a personal memory
//! store (thousands, not millions, of records) a brute-force cosine scan over
//! blobs is well under a millisecond, and the [`EmbeddingRepository`] trait
//! leaves room for an ANN backend later without touching callers.

use rusqlite::{params, OptionalExtension, Row};

use crate::core::model::{EmbeddingRecord, RecordKind, RecordRef};
use crate::error::{Error, Result};
use crate::storage::repository::{
    EmbeddedRecord, EmbeddingRepository, FullTextIndex, IndexableRecord, ProjectScope,
};
use crate::util::hash::content_hash;
use crate::util::time;

use super::SqliteStore;

/// Encode a vector for storage.
pub(crate) fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Decode a stored vector. A blob whose length is not a multiple of four is
/// corrupt; the trailing bytes are ignored rather than panicking.
pub(crate) fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

fn map_embedding(row: &Row<'_>) -> rusqlite::Result<EmbeddingRecord> {
    let kind: String = row.get(0)?;
    let blob: Vec<u8> = row.get(5)?;
    Ok(EmbeddingRecord {
        owner: RecordRef::new(kind.parse().unwrap_or(RecordKind::Memory), row.get::<_, String>(1)?),
        provider: row.get(2)?,
        model: row.get(3)?,
        dimensions: row.get::<_, i64>(4)? as usize,
        vector: decode_vector(&blob),
        content_hash: row.get(6)?,
        created_at: time::from_storage(&row.get::<_, String>(7)?),
    })
}

impl EmbeddingRepository for SqliteStore {
    fn upsert_embedding(&self, record: &EmbeddingRecord) -> Result<()> {
        if record.vector.is_empty() {
            return Err(Error::invalid("vector", "must not be empty"));
        }
        if record.vector.len() != record.dimensions {
            return Err(Error::invalid(
                "dimensions",
                format!("declared {} but vector has {}", record.dimensions, record.vector.len()),
            ));
        }
        let conn = self.conn();
        conn.execute(
            "INSERT INTO embeddings
                (record_kind, record_id, provider, model, dimensions, vector, content_hash,
                 created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (record_kind, record_id) DO UPDATE SET
                provider = excluded.provider, model = excluded.model,
                dimensions = excluded.dimensions, vector = excluded.vector,
                content_hash = excluded.content_hash, created_at = excluded.created_at",
            params![
                record.owner.kind.as_str(),
                record.owner.id,
                record.provider,
                record.model,
                record.dimensions as i64,
                encode_vector(&record.vector),
                record.content_hash,
                time::to_storage(&record.created_at),
            ],
        )?;
        Ok(())
    }

    fn get_embedding(&self, record: &RecordRef) -> Result<Option<EmbeddingRecord>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT record_kind, record_id, provider, model, dimensions, vector,
                        content_hash, created_at
                   FROM embeddings WHERE record_kind = ?1 AND record_id = ?2",
                params![record.kind.as_str(), record.id],
                map_embedding,
            )
            .optional()?)
    }

    fn embedded_records(
        &self,
        scope: &ProjectScope,
        kinds: &[RecordKind],
    ) -> Result<Vec<EmbeddedRecord>> {
        let wanted: Vec<RecordKind> = if kinds.is_empty() {
            vec![RecordKind::Memory, RecordKind::Decision, RecordKind::Checkpoint]
        } else {
            kinds.to_vec()
        };

        let conn = self.conn();
        let mut out = Vec::new();
        for kind in wanted {
            let table = match kind {
                RecordKind::Memory => "memories",
                RecordKind::Decision => "architecture_decisions",
                RecordKind::Checkpoint => "checkpoints",
            };
            // Archived memories are excluded here rather than at ranking time:
            // they should not consume candidate slots at all.
            let extra = if kind == RecordKind::Memory { "AND t.status != 'archived'" } else { "" };
            let (scope_sql, param) = scope.sql("t.project_id");
            let sql = format!(
                "SELECT e.record_id, t.project_id, e.vector
                   FROM embeddings e JOIN {table} t ON t.id = e.record_id
                  WHERE e.record_kind = ?1 AND {scope_sql} {extra}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mapper = |row: &Row<'_>| {
                Ok(EmbeddedRecord {
                    record: RecordRef::new(kind, row.get::<_, String>(0)?),
                    project_id: row.get(1)?,
                    vector: decode_vector(&row.get::<_, Vec<u8>>(2)?),
                })
            };
            let rows = match param {
                Some(p) => stmt
                    .query_map(params![kind.as_str(), p], mapper)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                None => stmt
                    .query_map(params![kind.as_str()], mapper)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            };
            out.extend(rows);
        }
        Ok(out)
    }

    /// Records with no vector, a vector from a different provider/model, or a
    /// vector whose source text has since changed.
    fn records_needing_embedding(
        &self,
        provider: &str,
        model: &str,
        scope: &ProjectScope,
    ) -> Result<Vec<IndexableRecord>> {
        let candidates = self.indexable_records(scope, &[])?;
        let existing = {
            let conn = self.conn();
            let mut stmt = conn.prepare(
                "SELECT record_kind, record_id, provider, model, content_hash FROM embeddings",
            )?;
            let map = stmt
                .query_map([], |row| {
                    Ok((
                        format!("{}:{}", row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                        (
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                        ),
                    ))
                })?
                .collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()?;
            map
        };

        Ok(candidates
            .into_iter()
            .filter(|record| {
                let key = format!("{}:{}", record.record.kind.as_str(), record.record.id);
                match existing.get(&key) {
                    None => true,
                    Some((p, m, hash)) => {
                        p != provider || m != model || *hash != content_hash(&record.embed_text())
                    }
                }
            })
            .collect())
    }

    fn delete_embedding(&self, record: &RecordRef) -> Result<bool> {
        let conn = self.conn();
        let removed = conn.execute(
            "DELETE FROM embeddings WHERE record_kind = ?1 AND record_id = ?2",
            params![record.kind.as_str(), record.id],
        )?;
        Ok(removed > 0)
    }

    fn clear_embeddings(&self) -> Result<usize> {
        let conn = self.conn();
        Ok(conn.execute("DELETE FROM embeddings", [])?)
    }

    fn count_embeddings(&self) -> Result<usize> {
        let conn = self.conn();
        let n: i64 = conn.query_row("SELECT count(*) FROM embeddings", [], |row| row.get(0))?;
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{Category, Memory, Project};
    use crate::storage::repository::{MemoryRepository, ProjectRepository};

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

    fn embedding(owner: RecordRef, vector: Vec<f32>, hash: &str) -> EmbeddingRecord {
        EmbeddingRecord {
            owner,
            provider: "local".into(),
            model: "hashing-v1".into(),
            dimensions: vector.len(),
            vector,
            content_hash: hash.into(),
            created_at: time::now(),
        }
    }

    #[test]
    fn vector_blob_roundtrip() {
        let v = vec![0.5_f32, -1.25, 3.0];
        assert_eq!(decode_vector(&encode_vector(&v)), v);
        assert_eq!(decode_vector(&[1, 2, 3]), Vec::<f32>::new());
    }

    #[test]
    fn upsert_replaces_and_validates() {
        let (store, project) = setup();
        let mut m = Memory::new(Category::Architecture, "T", "NATS");
        m.project_id = Some(project.id.clone());
        store.create_memory(&m).unwrap();

        let owner = RecordRef::memory(&m.id);
        store.upsert_embedding(&embedding(owner.clone(), vec![1.0, 0.0], "h1")).unwrap();
        store.upsert_embedding(&embedding(owner.clone(), vec![0.0, 1.0], "h2")).unwrap();
        assert_eq!(store.count_embeddings().unwrap(), 1);
        assert_eq!(store.get_embedding(&owner).unwrap().unwrap().content_hash, "h2");

        let mut bad = embedding(owner.clone(), vec![1.0], "h");
        bad.dimensions = 7;
        assert!(store.upsert_embedding(&bad).is_err());
    }

    #[test]
    fn needing_embedding_tracks_content_changes() {
        let (store, project) = setup();
        let mut m = Memory::new(Category::Architecture, "T", "NATS");
        m.project_id = Some(project.id.clone());
        store.create_memory(&m).unwrap();

        let pending = store.records_needing_embedding("local", "v1", &ProjectScope::Any).unwrap();
        assert_eq!(pending.len(), 1);

        let hash = content_hash(&pending[0].embed_text());
        store
            .upsert_embedding(&EmbeddingRecord {
                owner: pending[0].record.clone(),
                provider: "local".into(),
                model: "v1".into(),
                dimensions: 2,
                vector: vec![1.0, 0.0],
                content_hash: hash,
                created_at: time::now(),
            })
            .unwrap();
        assert!(store
            .records_needing_embedding("local", "v1", &ProjectScope::Any)
            .unwrap()
            .is_empty());

        // Switching model invalidates the vector.
        assert_eq!(
            store.records_needing_embedding("local", "v2", &ProjectScope::Any).unwrap().len(),
            1
        );

        // Editing the memory invalidates it too.
        let mut edited = store.get_memory(&m.id).unwrap().unwrap();
        edited.content = "NATS JetStream".into();
        store.update_memory(&edited).unwrap();
        assert_eq!(
            store.records_needing_embedding("local", "v1", &ProjectScope::Any).unwrap().len(),
            1
        );
    }

    #[test]
    fn embedded_records_are_scoped_and_skip_archived() {
        let (store, project) = setup();
        let mut active = Memory::new(Category::Architecture, "A", "NATS");
        active.project_id = Some(project.id.clone());
        let mut archived = Memory::new(Category::Architecture, "B", "Redis");
        archived.project_id = Some(project.id.clone());
        archived.status = crate::core::model::Status::Archived;
        store.create_memory(&active).unwrap();
        store.create_memory(&archived).unwrap();
        for m in [&active, &archived] {
            store
                .upsert_embedding(&embedding(RecordRef::memory(&m.id), vec![1.0, 0.0], "h"))
                .unwrap();
        }

        let found = store
            .embedded_records(&ProjectScope::Project(project.id.clone()), &[RecordKind::Memory])
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].record.id, active.id);

        assert!(store
            .embedded_records(&ProjectScope::GlobalOnly, &[RecordKind::Memory])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn deleting_memory_removes_its_vector() {
        let (store, project) = setup();
        let mut m = Memory::new(Category::Architecture, "T", "NATS");
        m.project_id = Some(project.id.clone());
        store.create_memory(&m).unwrap();
        store.upsert_embedding(&embedding(RecordRef::memory(&m.id), vec![1.0], "h")).unwrap();
        store.delete_memory(&m.id).unwrap();
        assert_eq!(store.count_embeddings().unwrap(), 0);
    }
}
