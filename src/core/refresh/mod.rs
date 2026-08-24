//! `contextd refresh` — tidying engineering memory.
//!
//! Refresh is not "rebuild the index". Memory accumulates the way notes do:
//! the same fact written twice, an old approach never marked as abandoned, a
//! vector left stale after an edit. Refresh:
//!
//! 1. finds exact and near-duplicate memories,
//! 2. marks the older of a duplicate pair as superseded by the newer,
//! 3. reports merely-similar pairs for a human to judge,
//! 4. rebuilds the full-text index,
//! 5. re-embeds whatever changed,
//! 6. optionally asks a [`Summarizer`] to consolidate a cluster.
//!
//! Everything destructive is reversible: superseding keeps both memories and
//! records the link, and `--dry-run` shows the plan without touching anything.

pub mod summarizer;

use serde::Serialize;

use crate::app::App;
use crate::core::model::{Memory, Project, Status};
use crate::error::Result;
use crate::search::IndexService;
use crate::storage::repository::{MemoryFilter, MemoryOrder, ProjectScope};
use crate::util::text::jaccard;

pub use summarizer::{Cluster, Summarizer};

/// Options for a refresh pass.
#[derive(Debug, Clone)]
pub struct RefreshOptions {
    /// Report what would change without changing anything.
    pub dry_run: bool,
    /// Re-embed every record, not just changed ones.
    pub force_embeddings: bool,
    /// Skip the embedding pass (useful offline with a remote provider).
    pub skip_embeddings: bool,
    /// At or above this similarity, memories are duplicates.
    pub duplicate_threshold: f64,
    /// At or above this, they are related and worth reporting.
    pub similar_threshold: f64,
}

impl RefreshOptions {
    pub fn from_config(app: &App) -> Self {
        let config = &app.config().refresh;
        Self {
            dry_run: false,
            force_embeddings: false,
            skip_embeddings: false,
            duplicate_threshold: config.duplicate_threshold,
            similar_threshold: config.similar_threshold,
        }
    }
}

/// A pair of memories refresh has an opinion about.
#[derive(Debug, Clone, Serialize)]
pub struct Pair {
    pub kept_id: String,
    pub kept_title: String,
    pub other_id: String,
    pub other_title: String,
    pub similarity: f64,
}

/// What a refresh pass did (or would do).
#[derive(Debug, Clone, Default, Serialize)]
pub struct RefreshReport {
    pub scanned: usize,
    /// Older memories marked superseded by a newer duplicate.
    pub merged: Vec<Pair>,
    /// Related but distinct memories, reported for a human to judge.
    pub similar: Vec<Pair>,
    pub fts_records: usize,
    pub embedded: usize,
    pub summaries: Vec<String>,
    pub notes: Vec<String>,
    pub dry_run: bool,
}

/// Memory housekeeping.
pub struct RefreshService<'a> {
    app: &'a App,
    summarizer: Box<dyn Summarizer>,
}

impl<'a> RefreshService<'a> {
    pub fn new(app: &'a App) -> Result<Self> {
        let summarizer = summarizer::build(&app.config().refresh, &app.config().embeddings)?;
        Ok(Self { app, summarizer })
    }

    /// Use a specific summariser (tests, or a CLI override).
    pub fn with_summarizer(app: &'a App, summarizer: Box<dyn Summarizer>) -> Self {
        Self { app, summarizer }
    }

    /// Run a refresh pass over one project (or globally when `project` is None).
    pub async fn run(
        &self,
        project: Option<&Project>,
        options: &RefreshOptions,
    ) -> Result<RefreshReport> {
        let scope = match project {
            Some(p) => ProjectScope::Project(p.id.clone()),
            None => ProjectScope::GlobalOnly,
        };
        let store = self.app.store();

        let memories = store.list_memories(&MemoryFilter {
            order: MemoryOrder::RecentFirst,
            ..MemoryFilter::for_scope(scope.clone())
        })?;

        let mut report = RefreshReport {
            scanned: memories.len(),
            dry_run: options.dry_run,
            ..Default::default()
        };

        let (duplicates, similar) = classify(&memories, options);
        report.similar = similar;

        for pair in &duplicates {
            if !options.dry_run {
                store.supersede_memory(&pair.other_id, &pair.kept_id)?;
            }
            report.merged.push(pair.clone());
        }

        // Consolidation is opt-in and never rewrites existing memories; it
        // adds a note the developer can turn into a memory.
        if self.summarizer.id() != "none" {
            for cluster in clusters(&memories, options) {
                if let Some(summary) = self.summarizer.summarize(&cluster).await? {
                    report.summaries.push(summary);
                }
            }
        }

        if options.dry_run {
            report.notes.push("dry run: no changes were written".into());
            return Ok(report);
        }

        let indexer = IndexService::new(self.app);
        report.fts_records = indexer.rebuild_fulltext()?;

        if options.skip_embeddings {
            report.notes.push("embeddings skipped (--skip-embeddings)".into());
        } else {
            match indexer.embed_pending(&ProjectScope::Any, options.force_embeddings).await {
                Ok(index_report) => {
                    report.embedded = index_report.embedded;
                    report.notes.extend(index_report.note);
                }
                // A refresh that tidied memories should not be reported as a
                // failure because an embedding endpoint was unreachable.
                Err(err) => report.notes.push(format!("embedding pass failed: {err}")),
            }
        }

        Ok(report)
    }
}

/// Split memory pairs into duplicates (act) and merely similar (report).
fn classify(memories: &[Memory], options: &RefreshOptions) -> (Vec<Pair>, Vec<Pair>) {
    let mut duplicates = Vec::new();
    let mut similar = Vec::new();
    // Memories already scheduled for superseding must not also be treated as
    // survivors, or a three-way duplicate would form a cycle.
    let mut consumed: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for (i, a) in memories.iter().enumerate() {
        if !a.status.is_current() {
            continue;
        }
        for b in memories.iter().skip(i + 1) {
            if !b.status.is_current() || consumed.contains(b.id.as_str()) {
                continue;
            }
            let similarity = similarity(a, b);
            if similarity >= options.duplicate_threshold {
                // Newer content wins; on a tie, higher priority wins. The
                // list arrives newest-first, so `a` is the newer of the pair
                // whenever the timestamps are equal.
                let (kept, other) = if (a.updated_at, a.priority) >= (b.updated_at, b.priority) {
                    (a, b)
                } else {
                    (b, a)
                };
                if consumed.contains(kept.id.as_str()) {
                    continue;
                }
                consumed.insert(other.id.as_str());
                duplicates.push(Pair {
                    kept_id: kept.id.clone(),
                    kept_title: kept.title.clone(),
                    other_id: other.id.clone(),
                    other_title: other.title.clone(),
                    similarity,
                });
            } else if similarity >= options.similar_threshold {
                similar.push(Pair {
                    kept_id: a.id.clone(),
                    kept_title: a.title.clone(),
                    other_id: b.id.clone(),
                    other_title: b.title.clone(),
                    similarity,
                });
            }
        }
    }
    (duplicates, similar)
}

/// Similarity over title and body, with the title weighted higher because two
/// memories with the same title are usually about the same thing.
fn similarity(a: &Memory, b: &Memory) -> f64 {
    let title = jaccard(&a.title, &b.title);
    let body = jaccard(&a.content, &b.content);
    (title * 0.4) + (body * 0.6)
}

/// Group memories into topical clusters for the summariser.
fn clusters(memories: &[Memory], options: &RefreshOptions) -> Vec<Cluster> {
    let mut clusters: Vec<(String, Vec<&Memory>)> = Vec::new();
    for memory in memories.iter().filter(|m| m.status.is_current()) {
        let slot = clusters.iter_mut().find(|(_, members)| {
            members.iter().any(|other| similarity(memory, other) >= options.similar_threshold)
        });
        match slot {
            Some((_, members)) => members.push(memory),
            None => clusters.push((memory.title.clone(), vec![memory])),
        }
    }
    clusters
        .into_iter()
        .filter(|(_, members)| members.len() > 1)
        .map(|(topic, members)| Cluster {
            topic,
            statements: members.iter().map(|m| format!("{}: {}", m.title, m.content)).collect(),
        })
        .collect()
}

/// Statuses refresh treats as history.
pub fn history_statuses() -> Vec<Status> {
    vec![Status::Superseded, Status::Deprecated]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::core::memory::{MemoryService, NewMemory};
    use crate::core::model::Category;
    use crate::core::project::{AttachRequest, ProjectService};
    use crate::embeddings::provider::BoxFuture;

    fn fixture() -> (tempfile::TempDir, App, Project) {
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

    fn add(app: &App, project: &Project, content: &str) -> Memory {
        MemoryService::new(app)
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Architecture, content)
            })
            .unwrap()
    }

    #[tokio::test]
    async fn duplicates_are_merged_newest_wins() {
        let (_dir, app, project) = fixture();
        let old = add(&app, &project, "The scheduler uses NATS for task transport");
        let new = add(&app, &project, "The scheduler uses NATS for task transport");

        let report = RefreshService::new(&app)
            .unwrap()
            .run(Some(&project), &RefreshOptions::from_config(&app))
            .await
            .unwrap();

        assert_eq!(report.merged.len(), 1);
        assert_eq!(report.merged[0].kept_id, new.id);
        let stored = MemoryService::new(&app).get(&old.id).unwrap();
        assert_eq!(stored.status, Status::Superseded);
        assert_eq!(stored.superseded_by.as_deref(), Some(new.id.as_str()));
    }

    #[tokio::test]
    async fn dry_run_changes_nothing() {
        let (_dir, app, project) = fixture();
        let old = add(&app, &project, "Duplicated memory text about the scheduler");
        add(&app, &project, "Duplicated memory text about the scheduler");

        let options = RefreshOptions { dry_run: true, ..RefreshOptions::from_config(&app) };
        let report =
            RefreshService::new(&app).unwrap().run(Some(&project), &options).await.unwrap();
        assert_eq!(report.merged.len(), 1);
        assert_eq!(MemoryService::new(&app).get(&old.id).unwrap().status, Status::Active);
        assert!(report.notes.iter().any(|n| n.contains("dry run")));
    }

    #[tokio::test]
    async fn similar_memories_are_reported_not_merged() {
        let (_dir, app, project) = fixture();
        add(&app, &project, "The scheduler sends tasks over NATS to workers");
        let other = add(&app, &project, "The scheduler sends heartbeats over NATS to workers");

        let report = RefreshService::new(&app)
            .unwrap()
            .run(Some(&project), &RefreshOptions::from_config(&app))
            .await
            .unwrap();
        assert!(report.merged.is_empty(), "distinct facts must not be merged");
        assert_eq!(report.similar.len(), 1);
        assert_eq!(MemoryService::new(&app).get(&other.id).unwrap().status, Status::Active);
    }

    #[tokio::test]
    async fn three_way_duplicates_collapse_to_one_survivor() {
        let (_dir, app, project) = fixture();
        let a = add(&app, &project, "Workers register with the coordinator at startup");
        let b = add(&app, &project, "Workers register with the coordinator at startup");
        let c = add(&app, &project, "Workers register with the coordinator at startup");

        RefreshService::new(&app)
            .unwrap()
            .run(Some(&project), &RefreshOptions::from_config(&app))
            .await
            .unwrap();

        let memories = MemoryService::new(&app);
        let active: Vec<_> = [&a, &b, &c]
            .iter()
            .map(|m| memories.get(&m.id).unwrap())
            .filter(|m| m.status.is_current())
            .collect();
        assert_eq!(active.len(), 1, "exactly one survivor");
        assert_eq!(active[0].id, c.id, "the newest survives");
    }

    #[tokio::test]
    async fn refresh_rebuilds_indexes() {
        let (_dir, app, project) = fixture();
        add(&app, &project, "Scheduler transport is NATS");
        let report = RefreshService::new(&app)
            .unwrap()
            .run(Some(&project), &RefreshOptions::from_config(&app))
            .await
            .unwrap();
        assert!(report.fts_records >= 1);
        assert!(report.embedded >= 1);
    }

    struct FakeSummarizer;

    impl Summarizer for FakeSummarizer {
        fn id(&self) -> &str {
            "fake"
        }

        fn summarize<'a>(&'a self, cluster: &'a Cluster) -> BoxFuture<'a, Result<Option<String>>> {
            let topic = cluster.topic.clone();
            let count = cluster.statements.len();
            Box::pin(async move { Ok(Some(format!("{topic}: consolidated {count} statements"))) })
        }
    }

    #[tokio::test]
    async fn a_configured_summarizer_consolidates_clusters() {
        let (_dir, app, project) = fixture();
        add(&app, &project, "Task queue transport migrated from Redis to NATS");
        add(&app, &project, "Task queue transport is NATS after leaving Redis");

        let service = RefreshService::with_summarizer(&app, Box::new(FakeSummarizer));
        let report = service
            .run(
                Some(&project),
                &RefreshOptions {
                    duplicate_threshold: 0.99,
                    similar_threshold: 0.4,
                    ..RefreshOptions::from_config(&app)
                },
            )
            .await
            .unwrap();
        assert_eq!(report.summaries.len(), 1);
        assert!(report.summaries[0].contains("consolidated 2 statements"));
    }
}
