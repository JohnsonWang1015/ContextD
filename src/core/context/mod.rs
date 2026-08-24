//! Context assembly.
//!
//! ```text
//! query → project detection → FTS → semantic → ranking → budget → context
//! ```
//!
//! The budget is the point. A memory store that grows for a year cannot be
//! pasted into an agent's context window, so ContextD selects: pinned memories
//! first, then the current architecture, then whatever the query pulled up, in
//! rank order, until the token budget is spent. What did not fit is counted,
//! never silently dropped.

pub mod render;

use serde::Serialize;

use crate::app::App;
use crate::core::model::{Category, Checkpoint, Decision, Project, RecordKind, Status};
use crate::error::Result;
use crate::search::{SearchHit, SearchRequest, SearchService};
use crate::storage::repository::{MemoryFilter, MemoryOrder, ProjectScope};
use crate::util::text::estimate_tokens;

/// What to build context for.
#[derive(Debug, Clone)]
pub struct ContextRequest {
    pub project: Option<Project>,
    /// The agent's question. With no query, ContextD returns the project's
    /// standing context: pinned memories, current architecture, latest state.
    pub query: Option<String>,
    pub max_tokens: usize,
    pub max_memories: usize,
    /// Append a short "superseded" section so an agent can see what *used* to
    /// be true and not re-propose it.
    pub include_superseded: bool,
    pub include_checkpoint: bool,
    /// Include global (cross-project) memories such as coding conventions.
    pub include_global: bool,
}

impl ContextRequest {
    /// Defaults taken from configuration.
    pub fn from_config(app: &App, project: Option<Project>) -> Self {
        let config = &app.config().context;
        Self {
            project,
            query: None,
            max_tokens: config.max_context_tokens,
            max_memories: config.max_memories,
            include_superseded: config.include_superseded,
            include_checkpoint: true,
            include_global: true,
        }
    }

    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        let query = query.into();
        self.query = (!query.trim().is_empty()).then_some(query);
        self
    }
}

/// A memory chosen for injection, with the reason it was chosen.
#[derive(Debug, Clone, Serialize)]
pub struct SelectedMemory {
    pub hit: SearchHit,
    /// Why it is here: `pinned`, `architecture`, `relevant`, `recent`.
    pub reason: &'static str,
    pub tokens: usize,
}

/// The assembled context.
#[derive(Debug, Clone, Serialize)]
pub struct ContextBundle {
    pub project: Option<Project>,
    pub query: Option<String>,
    pub checkpoint: Option<Checkpoint>,
    /// Decisions that describe the architecture as it currently stands.
    pub decisions: Vec<Decision>,
    /// Decisions and memories that were replaced, for "do not go back there".
    pub superseded: Vec<SupersededNote>,
    pub memories: Vec<SelectedMemory>,
    pub budget: BudgetReport,
}

/// A one-line note about something that is no longer true.
#[derive(Debug, Clone, Serialize)]
pub struct SupersededNote {
    pub title: String,
    pub replaced_by: Option<String>,
    pub kind: RecordKind,
}

/// Token accounting, reported so a caller can see how full the context is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BudgetReport {
    pub max_tokens: usize,
    pub used_tokens: usize,
    /// Records that ranked high enough to be considered but did not fit.
    pub dropped: usize,
}

impl BudgetReport {
    pub fn remaining(&self) -> usize {
        self.max_tokens.saturating_sub(self.used_tokens)
    }
}

/// Builds context bundles for `resume`, agent export and MCP.
pub struct ContextBuilder<'a> {
    app: &'a App,
}

impl<'a> ContextBuilder<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    /// Assemble context under the request's budget.
    pub async fn build(&self, request: &ContextRequest) -> Result<ContextBundle> {
        let store = self.app.store();
        let scope = match (&request.project, request.include_global) {
            (Some(p), true) => ProjectScope::ProjectWithGlobal(p.id.clone()),
            (Some(p), false) => ProjectScope::Project(p.id.clone()),
            (None, _) => ProjectScope::GlobalOnly,
        };

        let checkpoint = match (&request.project, request.include_checkpoint) {
            (Some(project), true) => store.latest_checkpoint(&project.id)?,
            _ => None,
        };
        let decisions = match &request.project {
            Some(project) => store.list_decisions(&project.id, false)?,
            None => Vec::new(),
        };

        let mut budget = BudgetReport { max_tokens: request.max_tokens, ..Default::default() };
        budget.used_tokens += fixed_cost(&checkpoint, &decisions);

        let candidates = self.candidates(request, &scope).await?;
        let (memories, dropped) = self.pack(candidates, request, &mut budget);
        budget.dropped = dropped;

        let superseded = if request.include_superseded {
            self.superseded_notes(request, &scope)?
        } else {
            Vec::new()
        };

        Ok(ContextBundle {
            project: request.project.clone(),
            query: request.query.clone(),
            checkpoint,
            decisions,
            superseded,
            memories,
            budget,
        })
    }

    /// Gather candidates: always-inject memories first, then query results (or
    /// the project's standing knowledge when there is no query).
    async fn candidates(
        &self,
        request: &ContextRequest,
        scope: &ProjectScope,
    ) -> Result<Vec<SelectedMemory>> {
        let store = self.app.store();
        let mut out: Vec<SelectedMemory> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Priority 5 means "the agent must always know this".
        let pinned = store.list_memories(&MemoryFilter {
            order: MemoryOrder::PriorityFirst,
            limit: Some(request.max_memories),
            ..MemoryFilter::for_scope(scope.clone())
        })?;
        for memory in pinned.into_iter().filter(|m| m.priority >= 5) {
            if seen.insert(memory.id.clone()) {
                out.push(to_selected(memory_hit(&memory), "pinned"));
            }
        }

        match &request.query {
            Some(query) => {
                let hits = SearchService::new(self.app)
                    .search(&SearchRequest {
                        limit: request.max_memories * 2,
                        ..SearchRequest::new(query.clone()).in_scope(scope.clone())
                    })
                    .await?;
                for hit in hits {
                    if seen.insert(hit.id.clone()) {
                        out.push(to_selected(hit, "relevant"));
                    }
                }
            }
            None => {
                // No question asked: hand over the standing context — what
                // kind of code this is, how it is built, what is in flight.
                let structural = store.list_memories(&MemoryFilter {
                    order: MemoryOrder::PriorityFirst,
                    categories: vec![
                        Category::Architecture,
                        Category::Convention,
                        Category::Decision,
                        Category::Project,
                        Category::User,
                    ],
                    limit: Some(request.max_memories),
                    ..MemoryFilter::for_scope(scope.clone())
                })?;
                for memory in structural {
                    if seen.insert(memory.id.clone()) {
                        out.push(to_selected(memory_hit(&memory), "architecture"));
                    }
                }

                let recent = store.list_memories(&MemoryFilter {
                    order: MemoryOrder::RecentFirst,
                    limit: Some(request.max_memories),
                    ..MemoryFilter::for_scope(scope.clone())
                })?;
                for memory in recent {
                    if seen.insert(memory.id.clone()) {
                        out.push(to_selected(memory_hit(&memory), "recent"));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Greedy pack in candidate order, which is already rank order.
    fn pack(
        &self,
        candidates: Vec<SelectedMemory>,
        request: &ContextRequest,
        budget: &mut BudgetReport,
    ) -> (Vec<SelectedMemory>, usize) {
        let mut chosen = Vec::new();
        let mut dropped = 0;
        for candidate in candidates {
            if chosen.len() >= request.max_memories {
                dropped += 1;
                continue;
            }
            // Pinned memories are the one exception to the budget: if they do
            // not fit, the budget is too small, and silently dropping them
            // would defeat the point of pinning.
            let fits = budget.used_tokens + candidate.tokens <= budget.max_tokens;
            if fits || candidate.reason == "pinned" {
                budget.used_tokens += candidate.tokens;
                chosen.push(candidate);
            } else {
                dropped += 1;
            }
        }
        (chosen, dropped)
    }

    /// A compact record of what is no longer true.
    fn superseded_notes(
        &self,
        request: &ContextRequest,
        scope: &ProjectScope,
    ) -> Result<Vec<SupersededNote>> {
        let store = self.app.store();
        let mut notes = Vec::new();

        let replaced = store.list_memories(&MemoryFilter {
            statuses: vec![Status::Superseded, Status::Deprecated],
            order: MemoryOrder::RecentFirst,
            limit: Some(10),
            ..MemoryFilter::for_scope(scope.clone())
        })?;
        let successor_ids: Vec<String> =
            replaced.iter().filter_map(|m| m.superseded_by.clone()).collect();
        let successors = store.get_memories(&successor_ids)?;
        for memory in replaced {
            let replaced_by = memory
                .superseded_by
                .as_ref()
                .and_then(|id| successors.iter().find(|s| &s.id == id))
                .map(|s| s.title.clone());
            notes.push(SupersededNote {
                title: memory.title,
                replaced_by,
                kind: RecordKind::Memory,
            });
        }

        if let Some(project) = &request.project {
            let all = store.list_decisions(&project.id, true)?;
            for decision in all.iter().filter(|d| !d.status.is_current()) {
                let replaced_by = decision
                    .superseded_by
                    .as_ref()
                    .and_then(|id| all.iter().find(|d| &d.id == id))
                    .map(|d| d.decision.clone());
                notes.push(SupersededNote {
                    title: format!("{}: {}", decision.title, decision.decision),
                    replaced_by,
                    kind: RecordKind::Decision,
                });
            }
        }
        Ok(notes)
    }
}

fn to_selected(hit: SearchHit, reason: &'static str) -> SelectedMemory {
    let tokens = estimate_tokens(&hit.title) + estimate_tokens(&hit.content) + 8;
    SelectedMemory { hit, reason, tokens }
}

/// Wrap a memory as a search hit so packing has one input type.
fn memory_hit(memory: &crate::core::model::Memory) -> SearchHit {
    SearchHit {
        kind: RecordKind::Memory,
        id: memory.id.clone(),
        project_id: memory.project_id.clone(),
        project_name: None,
        title: memory.title.clone(),
        content: memory.content.clone(),
        category: Some(memory.category),
        status: memory.status,
        priority: memory.priority,
        updated_at: memory.updated_at,
        superseded_by: memory.superseded_by.clone(),
        score: 0.0,
        breakdown: Default::default(),
    }
}

/// Tokens the header sections will cost once rendered.
fn fixed_cost(checkpoint: &Option<Checkpoint>, decisions: &[Decision]) -> usize {
    let mut total = 0;
    if let Some(checkpoint) = checkpoint {
        total += estimate_tokens(&checkpoint.indexable_text()) + 16;
    }
    for decision in decisions {
        total += estimate_tokens(&decision.title) + estimate_tokens(&decision.decision) + 8;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::core::checkpoint::{CheckpointService, NewCheckpoint};
    use crate::core::decision::{DecisionService, NewDecision};
    use crate::core::memory::{MemoryService, NewMemory};
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
                name: Some("FerroGrid".into()),
                description: None,
                bindings: vec![],
            })
            .unwrap();
        (dir, app, project)
    }

    #[tokio::test]
    async fn standing_context_includes_checkpoint_and_architecture() {
        let (_dir, app, project) = fixture().await;
        MemoryService::new(&app)
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Architecture, "Coordinator owns GPU leases")
            })
            .unwrap();
        DecisionService::new(&app).record(&project, NewDecision::new("Transport", "NATS")).unwrap();
        CheckpointService::new(&app)
            .create(
                &project,
                NewCheckpoint {
                    summary: "heartbeat done".into(),
                    current_goal: Some("Distributed GPU scheduler".into()),
                    skip_git: true,
                    ..Default::default()
                },
            )
            .unwrap();

        let bundle = ContextBuilder::new(&app)
            .build(&ContextRequest::from_config(&app, Some(project)))
            .await
            .unwrap();

        assert_eq!(
            bundle.checkpoint.unwrap().current_goal.as_deref(),
            Some("Distributed GPU scheduler")
        );
        assert_eq!(bundle.decisions.len(), 1);
        assert_eq!(bundle.memories.len(), 1);
        assert_eq!(bundle.memories[0].reason, "architecture");
        assert!(bundle.budget.used_tokens > 0);
    }

    #[tokio::test]
    async fn a_query_selects_relevant_memories() {
        let (_dir, app, project) = fixture().await;
        let memories = MemoryService::new(&app);
        memories
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Architecture, "Scheduler transport is NATS")
            })
            .unwrap();
        memories
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Convention, "Format with rustfmt before committing")
            })
            .unwrap();

        let bundle = ContextBuilder::new(&app)
            .build(&ContextRequest::from_config(&app, Some(project)).with_query("transport"))
            .await
            .unwrap();
        assert_eq!(bundle.memories.len(), 1);
        assert_eq!(bundle.memories[0].reason, "relevant");
        assert!(bundle.memories[0].hit.content.contains("NATS"));
    }

    #[tokio::test]
    async fn the_budget_is_enforced_and_pinned_memories_always_survive() {
        let (_dir, app, project) = fixture().await;
        let memories = MemoryService::new(&app);
        memories
            .add(NewMemory {
                project: Some(project.clone()),
                priority: Some(5),
                ..NewMemory::new(Category::Convention, "Always run cargo clippy before pushing")
            })
            .unwrap();
        for i in 0..20 {
            memories
                .add(NewMemory {
                    project: Some(project.clone()),
                    ..NewMemory::new(
                        Category::Knowledge,
                        format!("Filler memory {i}: {}", "lorem ipsum ".repeat(40)),
                    )
                })
                .unwrap();
        }

        let request =
            ContextRequest { max_tokens: 200, ..ContextRequest::from_config(&app, Some(project)) };
        let bundle = ContextBuilder::new(&app).build(&request).await.unwrap();

        assert!(bundle.memories.iter().any(|m| m.reason == "pinned"));
        assert!(bundle.budget.dropped > 0, "over-budget memories must be counted");
        let non_pinned: usize =
            bundle.memories.iter().filter(|m| m.reason != "pinned").map(|m| m.tokens).sum();
        assert!(non_pinned <= request.max_tokens);
    }

    #[tokio::test]
    async fn superseded_history_is_listed_separately() {
        let (_dir, app, project) = fixture().await;
        let memories = MemoryService::new(&app);
        let redis = memories
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Architecture, "Task queue is Redis")
            })
            .unwrap();
        memories
            .add(NewMemory {
                project: Some(project.clone()),
                supersedes: Some(redis.id.clone()),
                ..NewMemory::new(Category::Architecture, "Task queue is NATS")
            })
            .unwrap();

        let bundle = ContextBuilder::new(&app)
            .build(&ContextRequest::from_config(&app, Some(project)))
            .await
            .unwrap();

        assert!(bundle.memories.iter().all(|m| m.hit.status == Status::Active));
        assert_eq!(bundle.superseded.len(), 1);
        assert!(bundle.superseded[0].title.contains("Redis"));
        assert!(bundle.superseded[0].replaced_by.as_deref().unwrap().contains("NATS"));
    }

    #[tokio::test]
    async fn global_memories_can_be_excluded() {
        let (_dir, app, project) = fixture().await;
        MemoryService::new(&app)
            .add(NewMemory::new(Category::User, "Prefers concise commit messages"))
            .unwrap();

        let with_global = ContextBuilder::new(&app)
            .build(&ContextRequest::from_config(&app, Some(project.clone())))
            .await
            .unwrap();
        assert_eq!(with_global.memories.len(), 1);

        let without = ContextBuilder::new(&app)
            .build(&ContextRequest {
                include_global: false,
                ..ContextRequest::from_config(&app, Some(project))
            })
            .await
            .unwrap();
        assert!(without.memories.is_empty());
    }
}
