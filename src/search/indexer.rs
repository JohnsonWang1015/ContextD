//! Keeping the semantic index up to date.
//!
//! Embedding is incremental: a record is re-embedded only when its text, the
//! provider or the model changed (see
//! [`records_needing_embedding`](crate::storage::repository::EmbeddingRepository::records_needing_embedding)).
//! That keeps `contextd refresh` cheap and, with a paid provider, inexpensive.

use crate::app::App;
use crate::core::model::EmbeddingRecord;
use crate::error::Result;
use crate::storage::repository::ProjectScope;
use crate::util::hash::content_hash;
use crate::util::time;

/// Outcome of an indexing pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct IndexReport {
    pub embedded: usize,
    pub skipped: usize,
    pub fts_records: usize,
    /// Set when embeddings are configured off, or the provider failed.
    pub note: Option<String>,
}

/// Index maintenance.
pub struct IndexService<'a> {
    app: &'a App,
}

impl<'a> IndexService<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    /// Rebuild the full-text index from the base tables.
    pub fn rebuild_fulltext(&self) -> Result<usize> {
        self.app.store().rebuild_fts()
    }

    /// Embed everything in `scope` that needs it.
    ///
    /// `force` re-embeds records that already have a current vector, which is
    /// what a user wants after switching provider parameters that ContextD
    /// cannot see (a changed endpoint behind the same model name, say).
    pub async fn embed_pending(&self, scope: &ProjectScope, force: bool) -> Result<IndexReport> {
        let store = self.app.store();
        let Some(provider) = self.app.embedder() else {
            return Ok(IndexReport {
                note: Some("embeddings are disabled (embeddings.provider = \"none\")".into()),
                ..Default::default()
            });
        };

        let pending = if force {
            store.indexable_records(scope, &[])?
        } else {
            store.records_needing_embedding(provider.id(), provider.model(), scope)?
        };
        if pending.is_empty() {
            return Ok(IndexReport { skipped: 0, ..Default::default() });
        }

        let texts: Vec<String> = pending.iter().map(|r| r.embed_text()).collect();
        let vectors = provider.embed(&texts).await?;
        if vectors.len() != pending.len() {
            return Ok(IndexReport {
                note: Some(format!(
                    "provider returned {} vectors for {} records; index not updated",
                    vectors.len(),
                    pending.len()
                )),
                ..Default::default()
            });
        }

        let mut embedded = 0;
        for (record, vector) in pending.iter().zip(vectors) {
            if vector.is_empty() {
                continue; // nothing to index for an empty record
            }
            store.upsert_embedding(&EmbeddingRecord {
                owner: record.record.clone(),
                provider: provider.id().to_string(),
                model: provider.model().to_string(),
                dimensions: vector.len(),
                vector,
                content_hash: content_hash(&record.embed_text()),
                created_at: time::now(),
            })?;
            embedded += 1;
        }

        Ok(IndexReport { embedded, skipped: pending.len() - embedded, ..Default::default() })
    }

    /// Embed one record, best effort.
    ///
    /// Used right after a write so a new memory is immediately recallable.
    /// A provider failure is reported but never fails the write that preceded
    /// it — losing a vector is recoverable with `contextd refresh`, losing the
    /// memory is not.
    pub async fn embed_record(&self, record: &crate::core::model::RecordRef) -> Result<bool> {
        let store = self.app.store();
        let Some(provider) = self.app.embedder() else {
            return Ok(false);
        };
        let Some(indexable) = store.get_indexable(record)? else {
            return Ok(false);
        };
        let text = indexable.embed_text();
        let vector = crate::embeddings::provider::embed_one(provider, &text).await?;
        if vector.is_empty() {
            return Ok(false);
        }
        store.upsert_embedding(&EmbeddingRecord {
            owner: record.clone(),
            provider: provider.id().to_string(),
            model: provider.model().to_string(),
            dimensions: vector.len(),
            vector,
            content_hash: content_hash(&text),
            created_at: time::now(),
        })?;
        Ok(true)
    }

    /// How many records in scope have a current vector, out of how many exist.
    pub fn coverage(&self, scope: &ProjectScope) -> Result<(usize, usize)> {
        let store = self.app.store();
        let total = store.indexable_records(scope, &[])?.len();
        let Some(provider) = self.app.embedder() else {
            return Ok((0, total));
        };
        let pending =
            store.records_needing_embedding(provider.id(), provider.model(), scope)?.len();
        Ok((total.saturating_sub(pending), total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::core::memory::{MemoryService, NewMemory};
    use crate::core::model::Category;
    use crate::core::model::Project;
    use crate::core::project::{AttachRequest, ProjectService};

    async fn fixture() -> (tempfile::TempDir, App, Project) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let app =
            App::open_or_create(Paths::with_root(dir.path().join("home"))).unwrap().with_cwd(&repo);
        let (project, _) = ProjectService::new(&app)
            .attach(AttachRequest {
                dir: repo,
                name: Some("P".into()),
                description: None,
                bindings: vec![],
            })
            .unwrap();
        (dir, app, project)
    }

    #[tokio::test]
    async fn embeds_only_what_changed() {
        let (_dir, app, project) = fixture().await;
        let memories = MemoryService::new(&app);
        memories
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Architecture, "scheduler uses NATS")
            })
            .unwrap();

        let indexer = IndexService::new(&app);
        let first = indexer.embed_pending(&ProjectScope::Any, false).await.unwrap();
        assert_eq!(first.embedded, 1);

        let second = indexer.embed_pending(&ProjectScope::Any, false).await.unwrap();
        assert_eq!(second.embedded, 0, "unchanged records must not be re-embedded");

        let forced = indexer.embed_pending(&ProjectScope::Any, true).await.unwrap();
        assert_eq!(forced.embedded, 1);
        assert_eq!(indexer.coverage(&ProjectScope::Any).unwrap(), (1, 1));
    }

    #[tokio::test]
    async fn disabled_provider_reports_a_note() {
        let (_dir, mut app, project) = fixture().await;
        app.set_embedder(None);
        MemoryService::new(&app)
            .add(NewMemory {
                project: Some(project),
                ..NewMemory::new(Category::Architecture, "x")
            })
            .unwrap();

        let report =
            IndexService::new(&app).embed_pending(&ProjectScope::Any, false).await.unwrap();
        assert_eq!(report.embedded, 0);
        assert!(report.note.is_some());
        assert_eq!(IndexService::new(&app).coverage(&ProjectScope::Any).unwrap(), (0, 1));
    }

    #[tokio::test]
    async fn rebuild_fulltext_reindexes_everything() {
        let (_dir, app, project) = fixture().await;
        MemoryService::new(&app)
            .add(NewMemory {
                project: Some(project),
                ..NewMemory::new(Category::Architecture, "scheduler uses NATS")
            })
            .unwrap();
        assert_eq!(IndexService::new(&app).rebuild_fulltext().unwrap(), 1);
    }
}
