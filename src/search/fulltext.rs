//! Full-text retrieval arm.
//!
//! A thin adapter over the storage index: it exists so [`crate::search::hybrid`]
//! can treat "lexical" and "semantic" as two interchangeable candidate
//! sources, and so a different lexical backend could be substituted.

use std::collections::HashMap;

use crate::core::model::{RecordKind, RecordRef};
use crate::error::Result;
use crate::storage::repository::{FtsQuery, FullTextIndex, ProjectScope};

/// Run the lexical arm, returning `record -> normalised score`.
pub fn candidates(
    index: &dyn FullTextIndex,
    query: &str,
    scope: &ProjectScope,
    kinds: &[RecordKind],
    limit: usize,
) -> Result<HashMap<RecordRef, f64>> {
    let hits = index.fts_search(&FtsQuery {
        text: query.to_string(),
        scope: scope.clone(),
        kinds: kinds.to_vec(),
        limit,
    })?;
    Ok(hits.into_iter().map(|hit| (hit.record, hit.score)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{Category, Memory, Project};
    use crate::storage::repository::{MemoryRepository, ProjectRepository};
    use crate::storage::SqliteStore;
    use crate::util::time;

    #[test]
    fn returns_scored_candidates() {
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
        let mut memory = Memory::new(Category::Architecture, "Transport", "scheduler uses NATS");
        memory.project_id = Some(project.id.clone());
        store.create_memory(&memory).unwrap();

        let found = candidates(&store, "NATS", &ProjectScope::Any, &[], 10).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[&RecordRef::memory(&memory.id)] > 0.0);

        assert!(candidates(&store, "kubernetes", &ProjectScope::Any, &[], 10).unwrap().is_empty());
        assert!(candidates(&store, "   ", &ProjectScope::Any, &[], 10).unwrap().is_empty());
    }
}
