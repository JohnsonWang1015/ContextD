//! Persistence.
//!
//! [`repository`] defines the traits; [`sqlite`] is the shipped
//! implementation. Nothing above this module imports `rusqlite`.

pub mod repository;
pub mod sqlite;

pub use repository::{
    AgentBindingRepository, CheckpointRepository, DecisionRepository, EmbeddedRecord,
    EmbeddingRepository, FtsHit, FtsQuery, FullTextIndex, IndexableRecord, MemoryFilter,
    MemoryOrder, MemoryRepository, ProjectRepository, ProjectScope, ProjectStats,
    SessionRepository, Storage,
};
pub use sqlite::SqliteStore;
