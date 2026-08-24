//! Vector indexing.
//!
//! Similarity search sits behind [`VectorIndex`] so the store can change
//! without the retrieval code noticing. Two backends ship:
//!
//! * [`sqlite`] — a brute-force cosine scan over the vectors already in the
//!   database. Nothing to install, sub-millisecond at personal scale, and the
//!   default.
//! * [`qdrant`] — an external Qdrant collection, for stores large enough that
//!   a scan stops being free, or for people already running one.
//!
//! SQLite always holds the authoritative copy of every vector, whichever
//! backend is selected: an external index can then be rebuilt at any time
//! (`contextd refresh --reindex-vectors`), and `contextd bundle` keeps working
//! without the other machine needing the same infrastructure.

pub mod qdrant;
pub mod sqlite;

use std::sync::Arc;

use crate::config::VectorConfig;
use crate::core::model::{RecordKind, RecordRef, Status};
use crate::embeddings::provider::BoxFuture;
use crate::error::{Error, Result};
use crate::storage::repository::{ProjectScope, Storage};

/// One indexed vector plus the metadata the index filters on.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorPoint {
    pub record: RecordRef,
    pub project_id: Option<String>,
    pub status: Status,
    pub vector: Vec<f32>,
}

/// What a similarity search is restricted to.
#[derive(Debug, Clone)]
pub struct VectorQuery {
    pub vector: Vec<f32>,
    pub scope: ProjectScope,
    /// Empty means every kind.
    pub kinds: Vec<RecordKind>,
    pub limit: usize,
}

/// A hit from the index.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorMatch {
    pub record: RecordRef,
    pub project_id: Option<String>,
    /// Cosine similarity, 0.0..=1.0 after clamping.
    pub score: f64,
}

/// Health of the configured backend, for `contextd status`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct VectorHealth {
    pub backend: String,
    pub reachable: bool,
    pub points: Option<usize>,
    pub detail: Option<String>,
}

impl VectorHealth {
    /// Whether this health report describes a separate service.
    pub fn is_external_backend(&self) -> bool {
        self.backend != "sqlite"
    }
}

/// A place vectors can be stored and searched.
pub trait VectorIndex: Send + Sync {
    /// Backend name, as it appears in configuration and status output.
    fn backend(&self) -> &str;

    /// Whether the backend is a separate service.
    fn is_external(&self) -> bool {
        false
    }

    /// Add or replace points.
    fn upsert<'a>(&'a self, points: &'a [VectorPoint]) -> BoxFuture<'a, Result<()>>;

    /// Remove points by record reference.
    fn delete<'a>(&'a self, records: &'a [RecordRef]) -> BoxFuture<'a, Result<()>>;

    /// Nearest neighbours, most similar first.
    fn search<'a>(&'a self, query: &'a VectorQuery) -> BoxFuture<'a, Result<Vec<VectorMatch>>>;

    /// Drop everything in the index.
    fn clear(&self) -> BoxFuture<'_, Result<()>>;

    /// Whether the backend answers, and how much it holds.
    fn health(&self) -> BoxFuture<'_, Result<VectorHealth>>;
}

/// Build the configured index.
pub fn build(config: &VectorConfig, store: Arc<dyn Storage>) -> Result<Arc<dyn VectorIndex>> {
    match config.backend.trim().to_lowercase().as_str() {
        "sqlite" | "" | "none" | "builtin" => Ok(Arc::new(sqlite::SqliteVectorIndex::new(store))),
        "qdrant" => Ok(Arc::new(qdrant::QdrantIndex::new(config)?)),
        other => Err(Error::VectorStore(
            other.to_string(),
            "unknown backend (expected: sqlite, qdrant)".into(),
        )),
    }
}

/// Archived records are excluded from retrieval everywhere; keeping the rule
/// in one place stops the two backends from disagreeing about it.
pub fn is_retrievable(status: Status) -> bool {
    status != Status::Archived
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::SqliteStore;

    fn store() -> Arc<dyn Storage> {
        Arc::new(SqliteStore::open_in_memory().unwrap())
    }

    #[test]
    fn build_selects_backends() {
        let mut config = VectorConfig::default();
        let index = build(&config, store()).unwrap();
        assert_eq!(index.backend(), "sqlite");
        assert!(!index.is_external());

        config.backend = "qdrant".into();
        let index = build(&config, store()).unwrap();
        assert_eq!(index.backend(), "qdrant");
        assert!(index.is_external());

        config.backend = "pinecone".into();
        assert!(build(&config, store()).is_err());
    }

    #[test]
    fn archived_records_are_never_retrievable() {
        assert!(is_retrievable(Status::Active));
        assert!(is_retrievable(Status::Superseded));
        assert!(!is_retrievable(Status::Archived));
    }
}
