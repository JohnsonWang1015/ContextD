//! Storage traits.
//!
//! The rest of ContextD talks to these traits, never to SQLite. That is what
//! keeps the "SQLite → FTS → embeddings → semantic memory → MCP" evolution
//! from turning into one tangled module: a different backend only has to
//! implement [`Storage`].

use std::path::Path;

use crate::core::model::*;
use crate::error::Result;

/// Which project's records a query is about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ProjectScope {
    /// Every project plus global memories.
    #[default]
    Any,
    /// Only memories with no project.
    GlobalOnly,
    /// Only this project.
    Project(String),
    /// This project *and* global memories — the usual scope when building
    /// context for an agent working in one repository.
    ProjectWithGlobal(String),
}

impl ProjectScope {
    /// The project id in play, if any.
    pub fn project_id(&self) -> Option<&str> {
        match self {
            ProjectScope::Project(id) | ProjectScope::ProjectWithGlobal(id) => Some(id),
            _ => None,
        }
    }

    /// SQL fragment plus bound parameter, applied to a column named `col`.
    pub(crate) fn sql(&self, col: &str) -> (String, Option<String>) {
        match self {
            ProjectScope::Any => ("1 = 1".into(), None),
            ProjectScope::GlobalOnly => (format!("{col} IS NULL"), None),
            ProjectScope::Project(id) => (format!("{col} = ?"), Some(id.clone())),
            ProjectScope::ProjectWithGlobal(id) => {
                (format!("({col} = ? OR {col} IS NULL)"), Some(id.clone()))
            }
        }
    }
}

/// How a memory listing is ordered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MemoryOrder {
    /// Newest first.
    #[default]
    RecentFirst,
    /// Highest priority first, then newest.
    PriorityFirst,
    /// Oldest first.
    OldestFirst,
}

/// Filter for listing memories.
#[derive(Debug, Clone, Default)]
pub struct MemoryFilter {
    pub scope: ProjectScope,
    pub categories: Vec<Category>,
    /// Empty means "active only"; pass explicitly to include history.
    pub statuses: Vec<Status>,
    pub tags: Vec<String>,
    /// Substring match on title/content, used by `list --grep`.
    pub contains: Option<String>,
    pub order: MemoryOrder,
    pub limit: Option<usize>,
    pub offset: usize,
}

impl MemoryFilter {
    pub fn for_scope(scope: ProjectScope) -> Self {
        Self { scope, ..Default::default() }
    }

    pub fn with_statuses(mut self, statuses: Vec<Status>) -> Self {
        self.statuses = statuses;
        self
    }

    pub fn with_categories(mut self, categories: Vec<Category>) -> Self {
        self.categories = categories;
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Statuses actually applied (defaulting to active-only).
    pub fn effective_statuses(&self) -> Vec<Status> {
        if self.statuses.is_empty() {
            vec![Status::Active]
        } else {
            self.statuses.clone()
        }
    }
}

/// Counts shown by `contextd status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ProjectStats {
    pub memories: usize,
    pub active_memories: usize,
    pub superseded_memories: usize,
    pub decisions: usize,
    pub checkpoints: usize,
    pub sessions: usize,
    pub embedded_records: usize,
}

/// A record in a form the search and embedding layers can consume without
/// knowing which table it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexableRecord {
    pub record: RecordRef,
    pub project_id: Option<String>,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
}

impl IndexableRecord {
    /// Text handed to an embedding provider.
    pub fn embed_text(&self) -> String {
        if self.tags.is_empty() {
            format!("{}\n{}", self.title, self.body)
        } else {
            format!("{}\n{}\n{}", self.title, self.body, self.tags.join(" "))
        }
    }
}

/// A stored vector along with just enough metadata to rank and filter it.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddedRecord {
    pub record: RecordRef,
    pub project_id: Option<String>,
    /// Lifecycle of the underlying record. Decisions map onto the same scale
    /// (a replaced ADR is `Superseded`) so one filter covers every kind.
    pub status: Status,
    pub vector: Vec<f32>,
}

/// One full-text hit, before hybrid fusion.
#[derive(Debug, Clone, PartialEq)]
pub struct FtsHit {
    pub record: RecordRef,
    pub project_id: Option<String>,
    /// Normalised to 0.0..=1.0, higher is better.
    pub score: f64,
}

/// Query passed to the full-text index.
#[derive(Debug, Clone)]
pub struct FtsQuery {
    pub text: String,
    pub scope: ProjectScope,
    pub kinds: Vec<RecordKind>,
    pub limit: usize,
}

/// Projects.
pub trait ProjectRepository {
    fn create_project(&self, project: &Project) -> Result<()>;
    fn update_project(&self, project: &Project) -> Result<()>;
    fn get_project(&self, id: &str) -> Result<Option<Project>>;
    fn find_project_by_slug(&self, slug: &str) -> Result<Option<Project>>;
    /// Innermost project whose root is `path` or an ancestor of it.
    fn find_project_by_path(&self, path: &Path) -> Result<Option<Project>>;
    fn list_projects(&self, include_inactive: bool) -> Result<Vec<Project>>;
    /// Removes the project and, by cascade, everything belonging to it.
    fn delete_project(&self, id: &str) -> Result<bool>;
    fn project_stats(&self, id: &str) -> Result<ProjectStats>;
}

/// Memories.
pub trait MemoryRepository {
    fn create_memory(&self, memory: &Memory) -> Result<()>;
    fn update_memory(&self, memory: &Memory) -> Result<()>;
    fn get_memory(&self, id: &str) -> Result<Option<Memory>>;
    /// Resolve a full id or unique id prefix.
    fn resolve_memory(&self, ident: &str) -> Result<Option<Memory>>;
    fn list_memories(&self, filter: &MemoryFilter) -> Result<Vec<Memory>>;
    fn count_memories(&self, filter: &MemoryFilter) -> Result<usize>;
    fn delete_memory(&self, id: &str) -> Result<bool>;
    /// Mark `old` superseded by `new`, atomically.
    fn supersede_memory(&self, old_id: &str, new_id: &str) -> Result<()>;
    fn get_memories(&self, ids: &[String]) -> Result<Vec<Memory>>;
    fn all_tags(&self, scope: &ProjectScope) -> Result<Vec<(String, usize)>>;
}

/// Checkpoints.
pub trait CheckpointRepository {
    fn create_checkpoint(&self, checkpoint: &Checkpoint) -> Result<()>;
    fn latest_checkpoint(&self, project_id: &str) -> Result<Option<Checkpoint>>;
    fn list_checkpoints(&self, project_id: &str, limit: usize) -> Result<Vec<Checkpoint>>;
    fn get_checkpoint(&self, id: &str) -> Result<Option<Checkpoint>>;
    fn delete_checkpoint(&self, id: &str) -> Result<bool>;
}

/// Architecture decision records.
pub trait DecisionRepository {
    fn create_decision(&self, decision: &Decision) -> Result<()>;
    fn update_decision(&self, decision: &Decision) -> Result<()>;
    fn get_decision(&self, id: &str) -> Result<Option<Decision>>;
    fn resolve_decision(&self, ident: &str) -> Result<Option<Decision>>;
    fn list_decisions(&self, project_id: &str, include_superseded: bool) -> Result<Vec<Decision>>;
    fn get_decisions(&self, ids: &[String]) -> Result<Vec<Decision>>;
    fn delete_decision(&self, id: &str) -> Result<bool>;
    fn supersede_decision(&self, old_id: &str, new_id: &str) -> Result<()>;
}

/// Work sessions.
pub trait SessionRepository {
    fn start_session(&self, session: &Session) -> Result<()>;
    fn end_session(&self, id: &str, summary: Option<&str>) -> Result<bool>;
    fn latest_session(&self, project_id: &str) -> Result<Option<Session>>;
    fn list_sessions(&self, project_id: &str, limit: usize) -> Result<Vec<Session>>;
}

/// Links to agent configuration files.
pub trait AgentBindingRepository {
    fn upsert_binding(&self, binding: &AgentBinding) -> Result<()>;
    fn list_bindings(&self, project_id: &str) -> Result<Vec<AgentBinding>>;
    fn find_binding(
        &self,
        project_id: &str,
        agent: &str,
        path: &Path,
    ) -> Result<Option<AgentBinding>>;
    fn delete_binding(&self, id: &str) -> Result<bool>;
}

/// Vector storage.
pub trait EmbeddingRepository {
    fn upsert_embedding(&self, record: &EmbeddingRecord) -> Result<()>;
    fn get_embedding(&self, record: &RecordRef) -> Result<Option<EmbeddingRecord>>;
    /// Vectors in scope, for brute-force similarity search.
    fn embedded_records(
        &self,
        scope: &ProjectScope,
        kinds: &[RecordKind],
    ) -> Result<Vec<EmbeddedRecord>>;
    /// Records whose text has no current vector for this provider/model.
    fn records_needing_embedding(
        &self,
        provider: &str,
        model: &str,
        scope: &ProjectScope,
    ) -> Result<Vec<IndexableRecord>>;
    fn delete_embedding(&self, record: &RecordRef) -> Result<bool>;
    fn clear_embeddings(&self) -> Result<usize>;
    fn count_embeddings(&self) -> Result<usize>;
}

/// Full-text index maintenance and query.
pub trait FullTextIndex {
    fn fts_search(&self, query: &FtsQuery) -> Result<Vec<FtsHit>>;
    /// Drop and rebuild the whole index from the base tables.
    fn rebuild_fts(&self) -> Result<usize>;
    /// Every record that should be indexed/embedded, in scope.
    fn indexable_records(
        &self,
        scope: &ProjectScope,
        kinds: &[RecordKind],
    ) -> Result<Vec<IndexableRecord>>;
    fn get_indexable(&self, record: &RecordRef) -> Result<Option<IndexableRecord>>;
}

/// Everything a ContextD service needs from storage.
///
/// Services take `&dyn Storage`, so swapping the backend (or substituting a
/// fake in tests) is a one-line change.
pub trait Storage:
    ProjectRepository
    + MemoryRepository
    + CheckpointRepository
    + DecisionRepository
    + SessionRepository
    + AgentBindingRepository
    + EmbeddingRepository
    + FullTextIndex
    + Send
    + Sync
{
    /// Reclaim space and refresh query planner statistics.
    fn maintenance(&self) -> Result<()>;

    /// Schema version of the open database.
    fn schema_version(&self) -> Result<i64>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_sql_shapes() {
        assert_eq!(ProjectScope::Any.sql("p").0, "1 = 1");
        assert_eq!(ProjectScope::GlobalOnly.sql("p").0, "p IS NULL");
        let (sql, param) = ProjectScope::ProjectWithGlobal("x".into()).sql("p");
        assert_eq!(sql, "(p = ? OR p IS NULL)");
        assert_eq!(param.as_deref(), Some("x"));
    }

    #[test]
    fn filter_defaults_to_active() {
        let f = MemoryFilter::default();
        assert_eq!(f.effective_statuses(), vec![Status::Active]);
        let f = f.with_statuses(vec![Status::Archived]);
        assert_eq!(f.effective_statuses(), vec![Status::Archived]);
    }
}
