//! Brute-force vector search over the SQLite `embeddings` table.
//!
//! There is no separate index to keep in step: the authoritative vectors *are*
//! the index, so [`upsert`](VectorIndex::upsert) is a no-op here. At the scale
//! ContextD is built for — thousands of memories, not millions — a full scan
//! costs well under a millisecond, and it cannot go stale.

use std::sync::Arc;

use crate::core::model::RecordRef;
use crate::embeddings::{cosine_similarity, provider::BoxFuture};
use crate::error::Result;
use crate::search::vector::{VectorHealth, VectorIndex, VectorMatch, VectorPoint, VectorQuery};
use crate::storage::repository::Storage;

/// The default backend.
pub struct SqliteVectorIndex {
    store: Arc<dyn Storage>,
}

impl SqliteVectorIndex {
    pub fn new(store: Arc<dyn Storage>) -> Self {
        Self { store }
    }
}

impl VectorIndex for SqliteVectorIndex {
    fn backend(&self) -> &str {
        "sqlite"
    }

    /// Vectors are written to the `embeddings` table by the indexer, which is
    /// the same storage this backend searches; there is nothing else to do.
    fn upsert<'a>(&'a self, _points: &'a [VectorPoint]) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Likewise, removing the record removes its vector.
    fn delete<'a>(&'a self, _records: &'a [RecordRef]) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn search<'a>(&'a self, query: &'a VectorQuery) -> BoxFuture<'a, Result<Vec<VectorMatch>>> {
        Box::pin(async move {
            if query.vector.is_empty() {
                return Ok(Vec::new());
            }
            // Archived records are already excluded by the storage query.
            let candidates = self.store.embedded_records(&query.scope, &query.kinds)?;

            let mut scored: Vec<VectorMatch> = candidates
                .into_iter()
                .map(|candidate| VectorMatch {
                    score: cosine_similarity(&query.vector, &candidate.vector).max(0.0),
                    record: candidate.record,
                    project_id: candidate.project_id,
                })
                .filter(|hit| hit.score > 0.0)
                .collect();

            scored.sort_by(|a, b| b.score.total_cmp(&a.score));
            scored.truncate(query.limit.max(1));
            Ok(scored)
        })
    }

    fn clear(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.store.clear_embeddings()?;
            Ok(())
        })
    }

    fn health(&self) -> BoxFuture<'_, Result<VectorHealth>> {
        Box::pin(async move {
            Ok(VectorHealth {
                backend: "sqlite".into(),
                reachable: true,
                points: Some(self.store.count_embeddings()?),
                detail: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{Category, EmbeddingRecord, Memory, Project, Status};
    use crate::storage::repository::ProjectScope;
    use crate::storage::SqliteStore;
    use crate::util::time;

    fn setup() -> (Arc<dyn Storage>, Project) {
        let store: Arc<dyn Storage> = Arc::new(SqliteStore::open_in_memory().unwrap());
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

    fn add(
        store: &Arc<dyn Storage>,
        project: &Project,
        vector: Vec<f32>,
        status: Status,
    ) -> String {
        let mut memory = Memory::new(Category::Architecture, "t", "c");
        memory.project_id = Some(project.id.clone());
        memory.status = status;
        store.create_memory(&memory).unwrap();
        store
            .upsert_embedding(&EmbeddingRecord {
                owner: RecordRef::memory(&memory.id),
                provider: "local".into(),
                model: "test".into(),
                dimensions: vector.len(),
                vector,
                content_hash: "h".into(),
                created_at: time::now(),
            })
            .unwrap();
        memory.id
    }

    #[tokio::test]
    async fn ranks_by_similarity_and_skips_archived() {
        let (store, project) = setup();
        let near = add(&store, &project, vec![1.0, 0.0], Status::Active);
        let far = add(&store, &project, vec![0.2, 0.98], Status::Active);
        let archived = add(&store, &project, vec![1.0, 0.0], Status::Archived);

        let index = SqliteVectorIndex::new(Arc::clone(&store));
        let hits = index
            .search(&VectorQuery {
                vector: vec![1.0, 0.0],
                scope: ProjectScope::Any,
                kinds: vec![],
                limit: 10,
            })
            .await
            .unwrap();

        let ids: Vec<String> = hits.iter().map(|h| h.record.id.clone()).collect();
        assert_eq!(ids.first().map(String::as_str), Some(near.as_str()));
        assert!(ids.contains(&far));
        assert!(!ids.contains(&archived), "archived records must not be retrievable");
    }

    #[tokio::test]
    async fn respects_scope_and_limit() {
        let (store, project) = setup();
        for _ in 0..5 {
            add(&store, &project, vec![1.0, 0.0], Status::Active);
        }
        let index = SqliteVectorIndex::new(Arc::clone(&store));

        let limited = index
            .search(&VectorQuery {
                vector: vec![1.0, 0.0],
                scope: ProjectScope::Project(project.id.clone()),
                kinds: vec![],
                limit: 2,
            })
            .await
            .unwrap();
        assert_eq!(limited.len(), 2);

        let global_only = index
            .search(&VectorQuery {
                vector: vec![1.0, 0.0],
                scope: ProjectScope::GlobalOnly,
                kinds: vec![],
                limit: 10,
            })
            .await
            .unwrap();
        assert!(global_only.is_empty());
    }

    #[tokio::test]
    async fn health_counts_stored_vectors_and_clear_empties_them() {
        let (store, project) = setup();
        add(&store, &project, vec![1.0, 0.0], Status::Active);
        let index = SqliteVectorIndex::new(Arc::clone(&store));

        let health = index.health().await.unwrap();
        assert!(health.reachable);
        assert_eq!(health.points, Some(1));

        index.clear().await.unwrap();
        assert_eq!(index.health().await.unwrap().points, Some(0));
    }

    #[tokio::test]
    async fn an_empty_query_vector_matches_nothing() {
        let (store, project) = setup();
        add(&store, &project, vec![1.0, 0.0], Status::Active);
        let index = SqliteVectorIndex::new(store);
        let hits = index
            .search(&VectorQuery {
                vector: vec![],
                scope: ProjectScope::Any,
                kinds: vec![],
                limit: 5,
            })
            .await
            .unwrap();
        assert!(hits.is_empty());
    }
}
