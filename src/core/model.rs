//! Domain types.
//!
//! These are plain data: no database, no serialisation format and no agent
//! knows about the others. Repositories map rows to these types; adapters and
//! the MCP layer consume them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use crate::error::Error;

/// A tracked codebase (or the implicit global scope, which has no project).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    /// Display name, e.g. `FerroGrid`.
    pub name: String,
    /// Lowercase unique key used on the command line.
    pub slug: String,
    pub root_path: Option<PathBuf>,
    pub description: Option<String>,
    pub git_remote: Option<String>,
    pub default_branch: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// `false` after `contextd detach --archive`.
    pub active: bool,
}

/// What kind of knowledge a memory holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    /// Facts about the developer that apply everywhere.
    User,
    /// Facts about one project.
    Project,
    /// Structural facts: components, data flow, deployment.
    Architecture,
    /// A decision that was taken, with its rationale.
    Decision,
    /// Work in progress.
    Task,
    /// Feedback/corrections the developer gave an agent.
    Feedback,
    /// Coding style and conventions.
    Convention,
    /// Reusable technical knowledge.
    Knowledge,
    /// Pointer to an external resource.
    Reference,
}

impl Category {
    pub const ALL: [Category; 9] = [
        Category::User,
        Category::Project,
        Category::Architecture,
        Category::Decision,
        Category::Task,
        Category::Feedback,
        Category::Convention,
        Category::Knowledge,
        Category::Reference,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Category::User => "user",
            Category::Project => "project",
            Category::Architecture => "architecture",
            Category::Decision => "decision",
            Category::Task => "task",
            Category::Feedback => "feedback",
            Category::Convention => "convention",
            Category::Knowledge => "knowledge",
            Category::Reference => "reference",
        }
    }

    /// Categories that describe the *current* system, ranked first when a
    /// coding agent asks "how does this work today?".
    pub fn is_structural(&self) -> bool {
        matches!(self, Category::Architecture | Category::Decision | Category::Convention)
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Category {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Category::ALL.into_iter().find(|c| c.as_str().eq_ignore_ascii_case(s.trim())).ok_or_else(
            || {
                Error::invalid(
                    "category",
                    format!(
                        "`{s}` is not one of: {}",
                        Category::ALL.map(|c| c.as_str()).join(", ")
                    ),
                )
            },
        )
    }
}

/// Lifecycle of a memory. This is what lets ContextD distinguish *current
/// truth* from *historical truth*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Currently true.
    Active,
    /// Replaced by a newer memory (see [`Memory::superseded_by`]).
    Superseded,
    /// No longer the way things are done, with nothing replacing it.
    Deprecated,
    /// Kept for the record, excluded from normal retrieval.
    Archived,
}

impl Status {
    pub const ALL: [Status; 4] =
        [Status::Active, Status::Superseded, Status::Deprecated, Status::Archived];

    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Superseded => "superseded",
            Status::Deprecated => "deprecated",
            Status::Archived => "archived",
        }
    }

    /// Whether this memory still describes how things are.
    pub fn is_current(&self) -> bool {
        matches!(self, Status::Active)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Status {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Status::ALL.into_iter().find(|c| c.as_str().eq_ignore_ascii_case(s.trim())).ok_or_else(
            || {
                Error::invalid(
                    "status",
                    format!("`{s}` is not one of: {}", Status::ALL.map(|c| c.as_str()).join(", ")),
                )
            },
        )
    }
}

/// Where a memory came from. Provenance matters when refresh has to decide
/// which of two conflicting memories to trust.
///
/// In JSON this is the same compact string used in the database
/// (`manual`, `import:claude:CLAUDE.md`, `agent:mcp`), rather than a nested
/// object: it is a single value everywhere it appears, so API consumers and
/// the storage layer see the same thing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
pub enum Source {
    /// Typed by the developer.
    Manual,
    /// Imported from an agent configuration file.
    Import { agent: String, path: Option<String> },
    /// Written by an agent through MCP.
    Agent { agent: String },
    /// Produced by `contextd refresh`.
    Refresh,
    /// Derived from a checkpoint.
    Checkpoint,
}

impl Source {
    /// Compact storage form: `manual`, `import:claude`, `agent:codex`, …
    pub fn to_storage(&self) -> String {
        match self {
            Source::Manual => "manual".into(),
            Source::Import { agent, path } => match path {
                Some(p) => format!("import:{agent}:{p}"),
                None => format!("import:{agent}"),
            },
            Source::Agent { agent } => format!("agent:{agent}"),
            Source::Refresh => "refresh".into(),
            Source::Checkpoint => "checkpoint".into(),
        }
    }

    /// Inverse of [`Source::to_storage`]; unknown values degrade to `Manual`
    /// rather than failing a whole query.
    pub fn from_storage(raw: &str) -> Self {
        let mut parts = raw.splitn(3, ':');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("import"), Some(agent), path) => {
                Source::Import { agent: agent.to_string(), path: path.map(str::to_string) }
            }
            (Some("agent"), Some(agent), _) => Source::Agent { agent: agent.to_string() },
            (Some("refresh"), _, _) => Source::Refresh,
            (Some("checkpoint"), _, _) => Source::Checkpoint,
            _ => Source::Manual,
        }
    }

    /// Human-facing label, e.g. `import/claude`.
    pub fn label(&self) -> String {
        match self {
            Source::Manual => "manual".into(),
            Source::Import { agent, .. } => format!("import/{agent}"),
            Source::Agent { agent } => format!("agent/{agent}"),
            Source::Refresh => "refresh".into(),
            Source::Checkpoint => "checkpoint".into(),
        }
    }
}

impl From<Source> for String {
    fn from(source: Source) -> Self {
        source.to_storage()
    }
}

impl From<String> for Source {
    fn from(raw: String) -> Self {
        Source::from_storage(&raw)
    }
}

/// A single unit of engineering memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    /// `None` means global (applies to every project).
    pub project_id: Option<String>,
    pub category: Category,
    pub title: String,
    pub content: String,
    pub source: Source,
    /// 1 (background) … 5 (must always be injected). Default 3.
    pub priority: i64,
    pub status: Status,
    /// Id of the memory that replaced this one.
    pub superseded_by: Option<String>,
    pub tags: Vec<String>,
    /// Files this memory is about.
    pub files: Vec<String>,
    /// Commit this memory was learned at.
    pub commit: Option<String>,
    /// Code symbol (function, type) this memory is about.
    pub symbol: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Memory {
    /// Minimum viable memory; callers override fields as needed.
    pub fn new(category: Category, title: impl Into<String>, content: impl Into<String>) -> Self {
        let now = Utc::now();
        Memory {
            id: crate::util::ids::new_id(),
            project_id: None,
            category,
            title: title.into(),
            content: content.into(),
            source: Source::Manual,
            priority: DEFAULT_PRIORITY,
            status: Status::Active,
            superseded_by: None,
            tags: Vec::new(),
            files: Vec::new(),
            commit: None,
            symbol: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Text used for indexing and embedding: title carries most of the signal,
    /// so it is included with the body.
    pub fn indexable_text(&self) -> String {
        let mut text = String::with_capacity(self.title.len() + self.content.len() + 16);
        text.push_str(&self.title);
        text.push('\n');
        text.push_str(&self.content);
        if !self.tags.is_empty() {
            text.push('\n');
            text.push_str(&self.tags.join(" "));
        }
        text
    }

    /// Validate invariants that the database cannot express.
    pub fn validate(&self) -> crate::error::Result<()> {
        if self.title.trim().is_empty() {
            return Err(Error::invalid("title", "must not be empty"));
        }
        if self.content.trim().is_empty() {
            return Err(Error::invalid("content", "must not be empty"));
        }
        if !(MIN_PRIORITY..=MAX_PRIORITY).contains(&self.priority) {
            return Err(Error::invalid(
                "priority",
                format!("must be within {MIN_PRIORITY}..={MAX_PRIORITY}"),
            ));
        }
        if self.status == Status::Superseded && self.superseded_by.is_none() {
            // Allowed, but worth being explicit: a superseded memory with no
            // successor is really "deprecated".
            tracing::debug!(memory = %self.id, "superseded memory has no successor");
        }
        Ok(())
    }
}

pub const MIN_PRIORITY: i64 = 1;
pub const MAX_PRIORITY: i64 = 5;
pub const DEFAULT_PRIORITY: i64 = 3;

/// A saved point in a work session — the answer to "where was I?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub project_id: String,
    /// One-line summary, e.g. "worker heartbeat completed".
    pub summary: String,
    pub current_goal: Option<String>,
    pub completed: Vec<String>,
    pub current_state: Option<String>,
    pub next_steps: Vec<String>,
    pub open_problems: Vec<String>,
    pub related_files: Vec<String>,
    pub git_branch: Option<String>,
    pub git_commit: Option<String>,
    pub dirty_files: Vec<String>,
    pub created_at: DateTime<Utc>,
}

impl Checkpoint {
    pub fn new(project_id: impl Into<String>, summary: impl Into<String>) -> Self {
        Checkpoint {
            id: crate::util::ids::new_id(),
            project_id: project_id.into(),
            summary: summary.into(),
            current_goal: None,
            completed: Vec::new(),
            current_state: None,
            next_steps: Vec::new(),
            open_problems: Vec::new(),
            related_files: Vec::new(),
            git_branch: None,
            git_commit: None,
            dirty_files: Vec::new(),
            created_at: Utc::now(),
        }
    }

    pub fn indexable_text(&self) -> String {
        let mut parts = vec![self.summary.clone()];
        parts.extend(self.current_goal.clone());
        parts.extend(self.current_state.clone());
        parts.extend(self.completed.iter().cloned());
        parts.extend(self.next_steps.iter().cloned());
        parts.extend(self.open_problems.iter().cloned());
        parts.join("\n")
    }
}

/// Status of an architecture decision record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionStatus {
    Proposed,
    Accepted,
    Superseded,
    Rejected,
}

impl DecisionStatus {
    pub const ALL: [DecisionStatus; 4] = [
        DecisionStatus::Proposed,
        DecisionStatus::Accepted,
        DecisionStatus::Superseded,
        DecisionStatus::Rejected,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            DecisionStatus::Proposed => "proposed",
            DecisionStatus::Accepted => "accepted",
            DecisionStatus::Superseded => "superseded",
            DecisionStatus::Rejected => "rejected",
        }
    }

    pub fn is_current(&self) -> bool {
        matches!(self, DecisionStatus::Accepted | DecisionStatus::Proposed)
    }
}

impl fmt::Display for DecisionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DecisionStatus {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        DecisionStatus::ALL
            .into_iter()
            .find(|c| c.as_str().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| {
                Error::invalid(
                    "decision status",
                    format!(
                        "`{s}` is not one of: {}",
                        DecisionStatus::ALL.map(|c| c.as_str()).join(", ")
                    ),
                )
            })
    }
}

/// An architecture decision record (ADR).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub project_id: String,
    pub title: String,
    /// Why the decision was needed.
    pub context: Option<String>,
    /// What was decided.
    pub decision: String,
    /// What follows from it.
    pub consequences: Option<String>,
    /// Options that were considered and not taken.
    pub alternatives: Vec<String>,
    pub status: DecisionStatus,
    /// Id of the decision this one replaces.
    pub supersedes: Option<String>,
    /// Id of the decision that replaced this one.
    pub superseded_by: Option<String>,
    pub decided_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Decision {
    pub fn indexable_text(&self) -> String {
        let mut parts = vec![self.title.clone(), self.decision.clone()];
        parts.extend(self.context.clone());
        parts.extend(self.consequences.clone());
        parts.extend(self.alternatives.iter().cloned());
        parts.join("\n")
    }
}

/// A working session, used to group checkpoints and agent activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub project_id: String,
    /// Which agent is working: `claude`, `codex`, `cursor`, …
    pub agent: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub summary: Option<String>,
}

/// Link between a project and an agent's configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBinding {
    pub id: String,
    pub project_id: String,
    /// `claude`, `codex`, `cursor`, `generic`.
    pub agent: String,
    /// File ContextD reads from / writes to.
    pub path: PathBuf,
    /// Hash of the content ContextD last wrote, for conflict detection.
    pub last_hash: Option<String>,
    pub last_exported_at: Option<DateTime<Utc>>,
    pub last_imported_at: Option<DateTime<Utc>>,
}

/// A stored vector for one indexed record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingRecord {
    pub owner: RecordRef,
    pub provider: String,
    pub model: String,
    pub dimensions: usize,
    pub vector: Vec<f32>,
    /// Hash of the text that produced this vector, so refresh can skip
    /// unchanged records instead of re-embedding everything.
    pub content_hash: String,
    pub created_at: DateTime<Utc>,
}

/// Which kind of record a search hit or embedding refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    Memory,
    Decision,
    Checkpoint,
}

impl RecordKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RecordKind::Memory => "memory",
            RecordKind::Decision => "decision",
            RecordKind::Checkpoint => "checkpoint",
        }
    }
}

impl fmt::Display for RecordKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RecordKind {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "memory" => Ok(RecordKind::Memory),
            "decision" => Ok(RecordKind::Decision),
            "checkpoint" => Ok(RecordKind::Checkpoint),
            other => Err(Error::invalid(
                "kind",
                format!("`{other}` is not one of: memory, decision, checkpoint"),
            )),
        }
    }
}

/// A typed reference to any indexed record.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecordRef {
    pub kind: RecordKind,
    pub id: String,
}

impl RecordRef {
    pub fn new(kind: RecordKind, id: impl Into<String>) -> Self {
        Self { kind, id: id.into() }
    }

    pub fn memory(id: impl Into<String>) -> Self {
        Self::new(RecordKind::Memory, id)
    }

    pub fn decision(id: impl Into<String>) -> Self {
        Self::new(RecordKind::Decision, id)
    }

    pub fn checkpoint(id: impl Into<String>) -> Self {
        Self::new(RecordKind::Checkpoint, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_parses_case_insensitively() {
        assert_eq!("Architecture".parse::<Category>().unwrap(), Category::Architecture);
        assert!("nonsense".parse::<Category>().is_err());
    }

    #[test]
    fn status_parses_and_flags_current() {
        assert!(Status::Active.is_current());
        assert!(!"superseded".parse::<Status>().unwrap().is_current());
    }

    #[test]
    fn source_roundtrips() {
        let cases = [
            Source::Manual,
            Source::Refresh,
            Source::Checkpoint,
            Source::Agent { agent: "codex".into() },
            Source::Import { agent: "claude".into(), path: None },
            Source::Import { agent: "claude".into(), path: Some("CLAUDE.md".into()) },
        ];
        for case in cases {
            assert_eq!(Source::from_storage(&case.to_storage()), case);
        }
    }

    #[test]
    fn source_serialises_as_a_flat_string() {
        let json = serde_json::to_string(&Source::Agent { agent: "mcp".into() }).unwrap();
        assert_eq!(json, "\"agent:mcp\"");
        let parsed: Source = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Source::Agent { agent: "mcp".into() });
    }

    #[test]
    fn unknown_source_degrades_to_manual() {
        assert_eq!(Source::from_storage("who-knows"), Source::Manual);
    }

    #[test]
    fn memory_validation() {
        let mut m = Memory::new(Category::Project, "t", "c");
        assert!(m.validate().is_ok());
        m.title = "  ".into();
        assert!(m.validate().is_err());
        m.title = "t".into();
        m.priority = 9;
        assert!(m.validate().is_err());
    }
}
