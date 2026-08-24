//! Hybrid retrieval: fuse the lexical and semantic arms, then rank.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::app::App;
use crate::core::model::{Category, Project, RecordKind, RecordRef, Status};
use crate::error::Result;
use crate::search::scoring::{Breakdown, Features, Scorer, WeightedScorer};
use crate::storage::repository::ProjectScope;
use crate::util::{text, time};

/// Which arms to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchMode {
    /// Lexical only — exact, fast, no provider needed.
    Fulltext,
    /// Vectors only.
    Semantic,
    /// Both, fused. Falls back to lexical when no provider is configured.
    #[default]
    Hybrid,
}

/// A retrieval request.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub scope: ProjectScope,
    /// Empty means every kind.
    pub kinds: Vec<RecordKind>,
    /// Empty means every category (memories only).
    pub categories: Vec<Category>,
    pub limit: usize,
    /// Include superseded, deprecated and archived records.
    pub include_history: bool,
    pub mode: SearchMode,
}

impl SearchRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            scope: ProjectScope::Any,
            kinds: Vec::new(),
            categories: Vec::new(),
            limit: 20,
            include_history: false,
            mode: SearchMode::Hybrid,
        }
    }

    pub fn in_scope(mut self, scope: ProjectScope) -> Self {
        self.scope = scope;
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

/// One ranked result, carrying enough context to be rendered or injected
/// without a second lookup.
#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub kind: RecordKind,
    pub id: String,
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub title: String,
    /// Full body — the CLI truncates for display, the context builder budgets
    /// against it, and MCP clients get the whole thing.
    pub content: String,
    pub category: Option<Category>,
    pub status: Status,
    pub priority: i64,
    pub updated_at: DateTime<Utc>,
    pub superseded_by: Option<String>,
    pub score: f64,
    pub breakdown: Breakdown,
}

impl SearchHit {
    /// Short one-line form for tables.
    pub fn excerpt(&self, max_chars: usize) -> String {
        text::one_line(&self.content, max_chars)
    }

    /// True when this record describes how things are *now*.
    pub fn is_current(&self) -> bool {
        self.status.is_current()
    }

    pub fn record(&self) -> RecordRef {
        RecordRef::new(self.kind, &self.id)
    }
}

/// Runs searches for the CLI, the MCP server and the context builder.
pub struct SearchService<'a> {
    app: &'a App,
    scorer: Box<dyn Scorer>,
}

impl<'a> SearchService<'a> {
    pub fn new(app: &'a App) -> Self {
        let scorer = Box::new(WeightedScorer::new(app.config().search.clone()));
        Self { app, scorer }
    }

    /// Use a custom scorer (tests, experiments).
    pub fn with_scorer(app: &'a App, scorer: Box<dyn Scorer>) -> Self {
        Self { app, scorer }
    }

    /// Retrieve and rank.
    pub async fn search(&self, request: &SearchRequest) -> Result<Vec<SearchHit>> {
        let store = self.app.store();
        let candidate_limit = self.app.config().search.candidate_limit.max(request.limit);

        let use_fts = request.mode != SearchMode::Semantic;
        let use_semantic = request.mode != SearchMode::Fulltext;

        let fts_scores = if use_fts {
            crate::search::fulltext::candidates(
                store,
                &request.query,
                &request.scope,
                &request.kinds,
                candidate_limit,
            )?
        } else {
            HashMap::new()
        };

        // The semantic arm is best effort. With no provider configured it
        // contributes nothing; if the provider or the vector store is
        // unreachable it also contributes nothing, and the search still
        // answers from the local full-text index. Losing recall quality while
        // a service is down is far better than losing search altogether — the
        // failure is logged so it does not pass unnoticed.
        let semantic_scores = match (use_semantic, self.app.embedder()) {
            (true, Some(provider)) => {
                match crate::search::semantic::candidates(
                    self.app.vector(),
                    provider,
                    &request.query,
                    &request.scope,
                    &request.kinds,
                    candidate_limit,
                )
                .await
                {
                    Ok(scores) => scores,
                    Err(err) if request.mode == SearchMode::Semantic => return Err(err),
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "semantic search unavailable; answering from the full-text index only"
                        );
                        HashMap::new()
                    }
                }
            }
            _ => HashMap::new(),
        };

        let mut refs: HashSet<RecordRef> = HashSet::new();
        refs.extend(fts_scores.keys().cloned());
        refs.extend(semantic_scores.keys().cloned());
        if refs.is_empty() {
            return Ok(Vec::new());
        }

        let project_names = self.project_names()?;
        let focus_project = request.scope.project_id().map(str::to_string);

        let mut hits = Vec::with_capacity(refs.len());
        for record in refs {
            let Some(mut hit) = self.hydrate(&record, &project_names)? else {
                continue; // record vanished between retrieval and hydration
            };
            if !request.include_history && !hit.status.is_current() {
                continue;
            }
            if !request.categories.is_empty()
                && !hit.category.is_some_and(|c| request.categories.contains(&c))
            {
                continue;
            }

            let features = Features {
                fts: fts_scores.get(&record).copied().unwrap_or(0.0),
                semantic: semantic_scores.get(&record).copied().unwrap_or(0.0),
                priority: hit.priority,
                age_days: time::age_days(&hit.updated_at),
                same_project: match (&focus_project, &hit.project_id) {
                    (Some(focus), Some(project)) => focus == project,
                    _ => false,
                },
                status: hit.status,
                category: hit.category,
            };
            hit.breakdown = self.scorer.breakdown(&features);
            hit.score = hit.breakdown.total;
            hits.push(hit);
        }

        // Ties are broken by recency so repeated runs are stable and the newer
        // of two equally relevant records wins.
        hits.sort_by(|a, b| {
            b.score.total_cmp(&a.score).then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        hits.truncate(request.limit);
        Ok(hits)
    }

    /// Load the record behind a reference.
    fn hydrate(
        &self,
        record: &RecordRef,
        project_names: &HashMap<String, String>,
    ) -> Result<Option<SearchHit>> {
        let store = self.app.store();
        let hit = match record.kind {
            RecordKind::Memory => store.get_memory(&record.id)?.map(|memory| SearchHit {
                kind: RecordKind::Memory,
                id: memory.id,
                project_name: memory
                    .project_id
                    .as_ref()
                    .and_then(|id| project_names.get(id).cloned()),
                project_id: memory.project_id,
                title: memory.title,
                content: memory.content,
                category: Some(memory.category),
                status: memory.status,
                priority: memory.priority,
                updated_at: memory.updated_at,
                superseded_by: memory.superseded_by,
                score: 0.0,
                breakdown: Breakdown::default(),
            }),
            RecordKind::Decision => store.get_decision(&record.id)?.map(|decision| SearchHit {
                kind: RecordKind::Decision,
                id: decision.id,
                project_name: project_names.get(&decision.project_id).cloned(),
                project_id: Some(decision.project_id),
                title: decision.title,
                content: render_decision(
                    &decision.decision,
                    decision.context.as_deref(),
                    decision.consequences.as_deref(),
                ),
                category: Some(Category::Decision),
                // Decision status maps onto memory status so one scorer ranks
                // both: a replaced ADR is history exactly like a superseded
                // memory is.
                status: if decision.status.is_current() {
                    Status::Active
                } else {
                    Status::Superseded
                },
                // ADRs are the canonical statement of current architecture.
                priority: 4,
                updated_at: decision.updated_at,
                superseded_by: decision.superseded_by,
                score: 0.0,
                breakdown: Breakdown::default(),
            }),
            RecordKind::Checkpoint => {
                store.get_checkpoint(&record.id)?.map(|checkpoint| SearchHit {
                    kind: RecordKind::Checkpoint,
                    id: checkpoint.id.clone(),
                    project_name: project_names.get(&checkpoint.project_id).cloned(),
                    project_id: Some(checkpoint.project_id.clone()),
                    title: checkpoint.summary.clone(),
                    content: checkpoint.indexable_text(),
                    category: Some(Category::Task),
                    status: Status::Active,
                    priority: 3,
                    updated_at: checkpoint.created_at,
                    superseded_by: None,
                    score: 0.0,
                    breakdown: Breakdown::default(),
                })
            }
        };
        Ok(hit)
    }

    fn project_names(&self) -> Result<HashMap<String, String>> {
        Ok(self
            .app
            .store()
            .list_projects(true)?
            .into_iter()
            .map(|p: Project| (p.id, p.name))
            .collect())
    }
}

fn render_decision(decision: &str, context: Option<&str>, consequences: Option<&str>) -> String {
    let mut out = decision.to_string();
    if let Some(context) = context.filter(|c| !c.trim().is_empty()) {
        out.push_str("\n\nContext: ");
        out.push_str(context.trim());
    }
    if let Some(consequences) = consequences.filter(|c| !c.trim().is_empty()) {
        out.push_str("\n\nConsequences: ");
        out.push_str(consequences.trim());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::core::decision::{DecisionService, NewDecision};
    use crate::core::memory::{MemoryService, NewMemory};
    use crate::core::model::EmbeddingRecord;
    use crate::core::project::{AttachRequest, ProjectService};
    use crate::embeddings::provider::embed_one;
    use crate::util::hash::content_hash;

    struct Fixture {
        _dir: tempfile::TempDir,
        app: App,
        project: Project,
    }

    async fn fixture() -> Fixture {
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
        Fixture { _dir: dir, app, project }
    }

    /// Embed everything that has no vector yet, the way `refresh` does.
    async fn index(app: &App) {
        let provider = app.embedder().expect("local provider");
        let pending = app
            .store()
            .records_needing_embedding(provider.id(), provider.model(), &ProjectScope::Any)
            .unwrap();
        for record in pending {
            let text = record.embed_text();
            let vector = embed_one(provider, &text).await.unwrap();
            app.store()
                .upsert_embedding(&EmbeddingRecord {
                    owner: record.record.clone(),
                    provider: provider.id().to_string(),
                    model: provider.model().to_string(),
                    dimensions: vector.len(),
                    vector,
                    content_hash: content_hash(&text),
                    created_at: time::now(),
                })
                .unwrap();
        }
    }

    #[tokio::test]
    async fn finds_memories_by_keyword() {
        let f = fixture().await;
        MemoryService::new(&f.app)
            .add(NewMemory {
                project: Some(f.project.clone()),
                ..NewMemory::new(Category::Architecture, "GPU scheduler uses NATS for transport")
            })
            .unwrap();

        let hits = SearchService::new(&f.app)
            .search(
                &SearchRequest::new("scheduler")
                    .in_scope(ProjectScope::ProjectWithGlobal(f.project.id.clone())),
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("NATS"));
        assert_eq!(hits[0].project_name.as_deref(), Some("FerroGrid"));
    }

    #[tokio::test]
    async fn semantic_recall_finds_a_paraphrase() {
        let f = fixture().await;
        MemoryService::new(&f.app)
            .add(NewMemory {
                project: Some(f.project.clone()),
                ..NewMemory::new(
                    Category::Architecture,
                    "After evaluating Redis and PostgreSQL LISTEN/NOTIFY, the scheduler \
                     transport was migrated to NATS",
                )
            })
            .unwrap();
        index(&f.app).await;

        // No shared rare keyword with the memory except "transport".
        let hits = SearchService::new(&f.app)
            .search(&SearchRequest::new("which message transport does the scheduler use?"))
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].content.contains("NATS"));
        assert!(hits[0].breakdown.semantic > 0.0, "semantic arm should contribute");
    }

    #[tokio::test]
    async fn current_truth_ranks_above_superseded_history() {
        let f = fixture().await;
        let service = MemoryService::new(&f.app);
        // Redis appears three times, NATS once: frequency must not win.
        let redis = service
            .add(NewMemory {
                project: Some(f.project.clone()),
                ..NewMemory::new(
                    Category::Architecture,
                    "Task queue uses Redis. Redis streams carry scheduler tasks. Redis is \
                     the transport.",
                )
            })
            .unwrap();
        let nats = service
            .add(NewMemory {
                project: Some(f.project.clone()),
                supersedes: Some(redis.id.clone()),
                ..NewMemory::new(Category::Architecture, "Task queue transport is NATS")
            })
            .unwrap();
        index(&f.app).await;

        let search = SearchService::new(&f.app);
        let current = search
            .search(
                &SearchRequest::new("task queue transport")
                    .in_scope(ProjectScope::ProjectWithGlobal(f.project.id.clone())),
            )
            .await
            .unwrap();
        assert_eq!(current.len(), 1, "history is excluded by default");
        assert_eq!(current[0].id, nats.id);

        let with_history = search
            .search(&SearchRequest {
                include_history: true,
                ..SearchRequest::new("task queue transport")
                    .in_scope(ProjectScope::ProjectWithGlobal(f.project.id.clone()))
            })
            .await
            .unwrap();
        assert_eq!(with_history.len(), 2);
        assert_eq!(with_history[0].id, nats.id, "current truth must rank first");
    }

    #[tokio::test]
    async fn decisions_and_checkpoints_are_searchable() {
        let f = fixture().await;
        DecisionService::new(&f.app)
            .record(
                &f.project,
                NewDecision::new("Task transport", "Use NATS JetStream for task delivery"),
            )
            .unwrap();
        crate::core::checkpoint::CheckpointService::new(&f.app)
            .create(
                &f.project,
                crate::core::checkpoint::NewCheckpoint {
                    summary: "worker heartbeat completed".into(),
                    skip_git: true,
                    ..Default::default()
                },
            )
            .unwrap();

        let hits = SearchService::new(&f.app)
            .search(&SearchRequest::new("heartbeat jetstream").with_limit(10))
            .await
            .unwrap();
        let kinds: HashSet<RecordKind> = hits.iter().map(|h| h.kind).collect();
        assert!(kinds.contains(&RecordKind::Decision));
        assert!(kinds.contains(&RecordKind::Checkpoint));
    }

    #[tokio::test]
    async fn category_and_kind_filters_apply() {
        let f = fixture().await;
        let service = MemoryService::new(&f.app);
        service
            .add(NewMemory {
                project: Some(f.project.clone()),
                ..NewMemory::new(Category::Architecture, "scheduler transport is NATS")
            })
            .unwrap();
        service
            .add(NewMemory {
                project: Some(f.project.clone()),
                ..NewMemory::new(Category::Task, "scheduler needs a retry test")
            })
            .unwrap();

        let hits = SearchService::new(&f.app)
            .search(&SearchRequest {
                categories: vec![Category::Architecture],
                ..SearchRequest::new("scheduler")
            })
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].category, Some(Category::Architecture));

        let none = SearchService::new(&f.app)
            .search(&SearchRequest {
                kinds: vec![RecordKind::Decision],
                ..SearchRequest::new("scheduler")
            })
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn search_without_embeddings_still_works() {
        let mut f = fixture().await;
        f.app.set_embedder(None);
        MemoryService::new(&f.app)
            .add(NewMemory {
                project: Some(f.project.clone()),
                ..NewMemory::new(Category::Architecture, "scheduler transport is NATS")
            })
            .unwrap();

        let hits =
            SearchService::new(&f.app).search(&SearchRequest::new("scheduler")).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].breakdown.semantic, 0.0);
    }

    #[tokio::test]
    async fn a_failing_semantic_arm_falls_back_to_keywords() {
        use crate::search::vector::{VectorIndex, VectorMatch, VectorQuery};

        /// A vector store that is down.
        struct Broken;

        impl VectorIndex for Broken {
            fn backend(&self) -> &str {
                "broken"
            }
            fn is_external(&self) -> bool {
                true
            }
            fn upsert<'a>(
                &'a self,
                _points: &'a [crate::search::vector::VectorPoint],
            ) -> crate::embeddings::provider::BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }
            fn delete<'a>(
                &'a self,
                _records: &'a [RecordRef],
            ) -> crate::embeddings::provider::BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }
            fn search<'a>(
                &'a self,
                _query: &'a VectorQuery,
            ) -> crate::embeddings::provider::BoxFuture<'a, Result<Vec<VectorMatch>>> {
                Box::pin(async {
                    Err(crate::error::Error::VectorStore(
                        "broken".into(),
                        "connection refused".into(),
                    ))
                })
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
                        backend: "broken".into(),
                        reachable: false,
                        points: None,
                        detail: None,
                    })
                })
            }
        }

        let mut f = fixture().await;
        f.app.set_vector(std::sync::Arc::new(Broken));
        MemoryService::new(&f.app)
            .add(NewMemory {
                project: Some(f.project.clone()),
                ..NewMemory::new(Category::Architecture, "Scheduler transport is NATS")
            })
            .unwrap();

        let hits =
            SearchService::new(&f.app).search(&SearchRequest::new("transport")).await.unwrap();
        assert_eq!(hits.len(), 1, "keyword results must survive a broken vector store");
        assert_eq!(hits[0].breakdown.semantic, 0.0);

        // An explicitly semantic-only search has nothing to fall back to and
        // reports the failure instead of pretending there are no matches.
        let semantic_only = SearchService::new(&f.app)
            .search(&SearchRequest {
                mode: SearchMode::Semantic,
                ..SearchRequest::new("transport")
            })
            .await;
        assert!(semantic_only.is_err());
    }

    #[tokio::test]
    async fn empty_query_returns_nothing() {
        let f = fixture().await;
        let hits = SearchService::new(&f.app).search(&SearchRequest::new("   ")).await.unwrap();
        assert!(hits.is_empty());
    }
}
