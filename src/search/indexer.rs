//! Keeping the semantic index up to date.
//!
//! Embedding is incremental: a record is re-embedded only when its text, the
//! provider or the model changed (see
//! [`records_needing_embedding`](crate::storage::repository::EmbeddingRepository::records_needing_embedding)).
//! That keeps `contextd refresh` cheap and, with a paid provider, inexpensive.

use crate::app::App;
use crate::core::model::{EmbeddingRecord, RecordRef, Status};
use crate::error::Result;
use crate::search::vector::{VectorHealth, VectorPoint};
use crate::storage::repository::ProjectScope;
use crate::util::hash::content_hash;
use crate::util::time;

/// Outcome of an indexing pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct IndexReport {
    pub embedded: usize,
    pub skipped: usize,
    pub fts_records: usize,
    /// Points written to an external vector index, if one is configured.
    pub indexed: usize,
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
        let mut points = Vec::with_capacity(pending.len());
        for (record, vector) in pending.iter().zip(vectors) {
            if vector.is_empty() {
                continue; // nothing to index for an empty record
            }
            // SQLite keeps the authoritative copy whatever the search backend
            // is, so an external index can always be rebuilt from it.
            store.upsert_embedding(&EmbeddingRecord {
                owner: record.record.clone(),
                provider: provider.id().to_string(),
                model: provider.model().to_string(),
                dimensions: vector.len(),
                vector: vector.clone(),
                content_hash: content_hash(&record.embed_text()),
                created_at: time::now(),
            })?;
            points.push(VectorPoint {
                record: record.record.clone(),
                project_id: record.project_id.clone(),
                status: self.status_of(&record.record),
                vector,
            });
            embedded += 1;
        }

        let indexed = self.publish(&points).await?;
        Ok(IndexReport {
            embedded,
            skipped: pending.len() - embedded,
            indexed,
            ..Default::default()
        })
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
            vector: vector.clone(),
            content_hash: content_hash(&text),
            created_at: time::now(),
        })?;
        self.publish(&[VectorPoint {
            record: record.clone(),
            project_id: indexable.project_id.clone(),
            status: self.status_of(record),
            vector,
        }])
        .await?;
        Ok(true)
    }

    /// Push points to an external index. With the built-in backend this is a
    /// no-op, because the SQLite table it searches has just been written.
    async fn publish(&self, points: &[VectorPoint]) -> Result<usize> {
        if points.is_empty() || !self.app.vector().is_external() {
            return Ok(0);
        }
        self.app.vector().upsert(points).await?;
        Ok(points.len())
    }

    /// Remove a record from the vector index.
    ///
    /// The SQLite row goes with the record itself (foreign keys see to that);
    /// an external index has to be told.
    pub async fn forget_record(&self, record: &RecordRef) -> Result<()> {
        if !self.app.vector().is_external() {
            return Ok(());
        }
        self.app.vector().delete(std::slice::from_ref(record)).await
    }

    /// Re-publish every stored vector to the configured index.
    ///
    /// This is the command to run after switching backend or embedding model:
    /// it moves what is already in SQLite into the new index without paying to
    /// embed anything again.
    pub async fn reindex_vectors(&self) -> Result<usize> {
        if !self.app.vector().is_external() {
            return Ok(0);
        }
        let points: Vec<VectorPoint> = self
            .app
            .store()
            .embedded_records(&ProjectScope::Any, &[])?
            .into_iter()
            .map(|record| VectorPoint {
                record: record.record,
                project_id: record.project_id,
                status: record.status,
                vector: record.vector,
            })
            .filter(|point| !point.vector.is_empty())
            .collect();

        self.app.vector().upsert(&points).await?;
        Ok(points.len())
    }

    /// Lifecycle of a record, for the index payload.
    fn status_of(&self, record: &RecordRef) -> Status {
        match record.kind {
            crate::core::model::RecordKind::Memory => self
                .app
                .store()
                .get_memory(&record.id)
                .ok()
                .flatten()
                .map(|memory| memory.status)
                .unwrap_or(Status::Active),
            crate::core::model::RecordKind::Decision => {
                self.app
                    .store()
                    .get_decision(&record.id)
                    .ok()
                    .flatten()
                    .map(|decision| {
                        if decision.status.is_current() {
                            Status::Active
                        } else {
                            Status::Superseded
                        }
                    })
                    .unwrap_or(Status::Active)
            }
            crate::core::model::RecordKind::Checkpoint => Status::Active,
        }
    }

    /// Health of the configured vector backend.
    pub async fn vector_health(&self) -> Result<VectorHealth> {
        self.app.vector().health().await
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
    async fn points_reach_an_external_index() {
        use crate::search::vector::{VectorIndex, VectorMatch, VectorQuery};
        use std::sync::{Arc, Mutex};

        /// Stands in for Qdrant: records what it was told, nothing more.
        #[derive(Default)]
        struct RecordingIndex {
            points: Mutex<Vec<VectorPoint>>,
            deleted: Mutex<Vec<RecordRef>>,
        }

        impl VectorIndex for RecordingIndex {
            fn backend(&self) -> &str {
                "recording"
            }
            fn is_external(&self) -> bool {
                true
            }
            fn upsert<'a>(
                &'a self,
                points: &'a [VectorPoint],
            ) -> crate::embeddings::provider::BoxFuture<'a, Result<()>> {
                Box::pin(async move {
                    self.points.lock().unwrap().extend_from_slice(points);
                    Ok(())
                })
            }
            fn delete<'a>(
                &'a self,
                records: &'a [RecordRef],
            ) -> crate::embeddings::provider::BoxFuture<'a, Result<()>> {
                Box::pin(async move {
                    self.deleted.lock().unwrap().extend_from_slice(records);
                    Ok(())
                })
            }
            fn search<'a>(
                &'a self,
                _query: &'a VectorQuery,
            ) -> crate::embeddings::provider::BoxFuture<'a, Result<Vec<VectorMatch>>> {
                Box::pin(async { Ok(Vec::new()) })
            }
            fn clear(&self) -> crate::embeddings::provider::BoxFuture<'_, Result<()>> {
                Box::pin(async { Ok(()) })
            }
            fn health(
                &self,
            ) -> crate::embeddings::provider::BoxFuture<
                '_,
                Result<crate::search::vector::VectorHealth>,
            > {
                Box::pin(async {
                    Ok(crate::search::vector::VectorHealth {
                        backend: "recording".into(),
                        reachable: true,
                        points: None,
                        detail: None,
                    })
                })
            }
        }

        let (_dir, mut app, project) = fixture().await;
        let index = Arc::new(RecordingIndex::default());
        app.set_vector(Arc::clone(&index) as Arc<dyn VectorIndex>);

        let memory = MemoryService::new(&app)
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Architecture, "scheduler uses NATS")
            })
            .unwrap();

        let indexer = IndexService::new(&app);
        let report = indexer.embed_pending(&ProjectScope::Any, false).await.unwrap();
        assert_eq!(report.embedded, 1);
        assert_eq!(report.indexed, 1, "the external index must be told about new vectors");
        assert_eq!(index.points.lock().unwrap()[0].status, Status::Active);
        assert!(!index.points.lock().unwrap()[0].vector.is_empty());

        // Rebuilding pushes what SQLite already holds, without re-embedding.
        assert_eq!(indexer.reindex_vectors().await.unwrap(), 1);

        indexer.forget_record(&RecordRef::memory(&memory.id)).await.unwrap();
        assert_eq!(index.deleted.lock().unwrap().len(), 1);
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
