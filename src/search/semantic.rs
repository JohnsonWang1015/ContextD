//! Semantic retrieval arm.
//!
//! The arm itself is backend-agnostic: it embeds the query and hands the
//! vector to whichever [`VectorIndex`] is configured — the built-in SQLite
//! scan, or Qdrant for a store that has outgrown one.

use std::collections::HashMap;

use crate::core::model::{RecordKind, RecordRef};
use crate::embeddings::{provider::embed_one, EmbeddingProvider};
use crate::error::Result;
use crate::search::vector::{VectorIndex, VectorQuery};
use crate::storage::repository::ProjectScope;

/// Run the semantic arm, returning `record -> similarity` in 0.0..=1.0.
///
/// Cosine ranges over -1..=1; negatives (actively dissimilar) collapse to 0 so
/// the score can be summed with the lexical one.
pub async fn candidates(
    index: &dyn VectorIndex,
    provider: &dyn EmbeddingProvider,
    query: &str,
    scope: &ProjectScope,
    kinds: &[RecordKind],
    limit: usize,
) -> Result<HashMap<RecordRef, f64>> {
    if query.trim().is_empty() {
        return Ok(HashMap::new());
    }

    let query_vector = embed_one(provider, query).await?;
    if query_vector.is_empty() {
        return Ok(HashMap::new());
    }

    let matches = index
        .search(&VectorQuery {
            vector: query_vector,
            scope: scope.clone(),
            kinds: kinds.to_vec(),
            limit,
        })
        .await?;

    Ok(matches.into_iter().map(|hit| (hit.record, hit.score)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{Category, EmbeddingRecord, Memory, Project};
    use crate::embeddings::local::LocalEmbedder;
    use crate::storage::SqliteStore;
    use crate::util::hash::content_hash;
    use crate::util::time;

    fn seed() -> (std::sync::Arc<dyn crate::storage::repository::Storage>, Project, LocalEmbedder) {
        let store: std::sync::Arc<dyn crate::storage::repository::Storage> =
            std::sync::Arc::new(SqliteStore::open_in_memory().unwrap());
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
        (store, project, LocalEmbedder::new(256))
    }

    fn index(
        store: &std::sync::Arc<dyn crate::storage::repository::Storage>,
    ) -> crate::search::vector::sqlite::SqliteVectorIndex {
        crate::search::vector::sqlite::SqliteVectorIndex::new(std::sync::Arc::clone(store))
    }

    fn add(
        store: &std::sync::Arc<dyn crate::storage::repository::Storage>,
        project: &Project,
        embedder: &LocalEmbedder,
        text: &str,
    ) -> String {
        let mut memory = Memory::new(Category::Architecture, text, text);
        memory.project_id = Some(project.id.clone());
        store.create_memory(&memory).unwrap();
        let vector = embedder.embed_text(text);
        store
            .upsert_embedding(&EmbeddingRecord {
                owner: RecordRef::memory(&memory.id),
                provider: "local".into(),
                model: "hashing-v1".into(),
                dimensions: vector.len(),
                vector,
                content_hash: content_hash(text),
                created_at: time::now(),
            })
            .unwrap();
        memory.id
    }

    #[tokio::test]
    async fn ranks_related_memories_first() {
        let (store, project, embedder) = seed();
        let transport = add(
            &store,
            &project,
            &embedder,
            "After evaluating Redis and PostgreSQL LISTEN/NOTIFY, the scheduler transport \
             was migrated to NATS",
        );
        let unrelated = add(&store, &project, &embedder, "The CLI prints a colourful table");

        let hits = candidates(
            &index(&store),
            &embedder,
            "which message transport does the scheduler use",
            &ProjectScope::Any,
            &[],
            10,
        )
        .await
        .unwrap();

        let transport_score = hits[&RecordRef::memory(&transport)];
        let unrelated_score = hits.get(&RecordRef::memory(&unrelated)).copied().unwrap_or(0.0);
        assert!(transport_score > unrelated_score);
    }

    #[tokio::test]
    async fn empty_inputs_are_handled() {
        let (store, _project, embedder) = seed();
        assert!(candidates(&index(&store), &embedder, "  ", &ProjectScope::Any, &[], 10)
            .await
            .unwrap()
            .is_empty());
        assert!(candidates(&index(&store), &embedder, "anything", &ProjectScope::Any, &[], 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn respects_the_candidate_limit() {
        let (store, project, embedder) = seed();
        for i in 0..5 {
            add(&store, &project, &embedder, &format!("scheduler note number {i}"));
        }
        let hits = candidates(&index(&store), &embedder, "scheduler", &ProjectScope::Any, &[], 2)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
    }
}
