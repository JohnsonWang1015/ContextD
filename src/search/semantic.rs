//! Semantic retrieval arm.
//!
//! Brute-force cosine over the vectors in scope. At ContextD's scale (a
//! personal memory store) this is a sub-millisecond scan and needs no index to
//! keep in sync; the [`crate::storage::repository::EmbeddingRepository`] trait
//! is where an ANN backend would slot in if a store ever grew large enough to
//! need one.

use std::collections::HashMap;

use crate::core::model::{RecordKind, RecordRef};
use crate::embeddings::{cosine_similarity, provider::embed_one, EmbeddingProvider};
use crate::error::Result;
use crate::storage::repository::{EmbeddingRepository, ProjectScope};

/// Run the semantic arm, returning `record -> similarity` in 0.0..=1.0.
///
/// Cosine ranges over -1..=1; it is rescaled so it can be summed with the
/// lexical score, and negatives (actively dissimilar) collapse to 0.
pub async fn candidates(
    store: &dyn EmbeddingRepository,
    provider: &dyn EmbeddingProvider,
    query: &str,
    scope: &ProjectScope,
    kinds: &[RecordKind],
    limit: usize,
) -> Result<HashMap<RecordRef, f64>> {
    if query.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let records = store.embedded_records(scope, kinds)?;
    if records.is_empty() {
        return Ok(HashMap::new());
    }

    let query_vector = embed_one(provider, query).await?;
    if query_vector.is_empty() {
        return Ok(HashMap::new());
    }

    let mut scored: Vec<(RecordRef, f64)> = records
        .into_iter()
        .map(|record| {
            let similarity = cosine_similarity(&query_vector, &record.vector);
            (record.record, similarity.max(0.0))
        })
        .filter(|(_, similarity)| *similarity > 0.0)
        .collect();

    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(limit);
    Ok(scored.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{Category, EmbeddingRecord, Memory, Project};
    use crate::embeddings::local::LocalEmbedder;
    use crate::storage::repository::{MemoryRepository, ProjectRepository};
    use crate::storage::SqliteStore;
    use crate::util::hash::content_hash;
    use crate::util::time;

    fn seed() -> (SqliteStore, Project, LocalEmbedder) {
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
        (store, project, LocalEmbedder::new(256))
    }

    fn add(store: &SqliteStore, project: &Project, embedder: &LocalEmbedder, text: &str) -> String {
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
            &store,
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
        assert!(candidates(&store, &embedder, "  ", &ProjectScope::Any, &[], 10)
            .await
            .unwrap()
            .is_empty());
        assert!(candidates(&store, &embedder, "anything", &ProjectScope::Any, &[], 10)
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
        let hits =
            candidates(&store, &embedder, "scheduler", &ProjectScope::Any, &[], 2).await.unwrap();
        assert_eq!(hits.len(), 2);
    }
}
