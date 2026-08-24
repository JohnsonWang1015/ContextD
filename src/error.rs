//! Crate-wide error type.
//!
//! Library code returns [`Error`]; the CLI binary converts it into a friendly
//! message. `anyhow` is used only at the very edges (binary, adapters that
//! aggregate many unrelated failures).

use std::path::PathBuf;

/// Result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every failure ContextD can produce.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ContextD is not initialised yet — run `contextd init`")]
    NotInitialised,

    #[error("no project is attached to {0} — run `contextd attach` inside a project directory")]
    NoProjectHere(PathBuf),

    #[error("project `{0}` not found")]
    ProjectNotFound(String),

    #[error("memory `{0}` not found")]
    MemoryNotFound(String),

    #[error("checkpoint not found for project `{0}`")]
    CheckpointNotFound(String),

    #[error("ambiguous identifier `{ident}` matches {count} records")]
    Ambiguous { ident: String, count: usize },

    #[error("invalid {field}: {reason}")]
    Invalid { field: &'static str, reason: String },

    #[error("unknown agent adapter `{0}` (known: claude, codex, cursor, generic)")]
    UnknownAgent(String),

    #[error("embedding provider `{0}` is not configured correctly: {1}")]
    EmbeddingProvider(String, String),

    #[error("vector store `{0}`: {1}")]
    VectorStore(String, String),

    #[error(
        "refusing to overwrite {path}: it was modified outside ContextD (use --force to override)"
    )]
    SyncConflict { path: PathBuf },

    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("migration `{name}` failed: {source}")]
    Migration {
        name: String,
        #[source]
        source: rusqlite::Error,
    },

    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("i/o error: {0}")]
    PlainIo(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("serialisation error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    /// Attach a path to an [`std::io::Error`].
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io { path: path.into(), source }
    }

    /// Convenience constructor for validation failures.
    pub fn invalid(field: &'static str, reason: impl Into<String>) -> Self {
        Error::Invalid { field, reason: reason.into() }
    }
}
