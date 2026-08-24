//! What a ContextD store holds, without reading the memories themselves.
//!
//! An inventory answers "what is on that machine?" — how many projects, how
//! much memory in each, when it was last touched — in a few kilobytes, so
//! surveying a remote does not mean downloading its entire memory first. It is
//! also what `contextd inventory` prints locally.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::core::model::Category;
use crate::error::Result;
use crate::storage::repository::{MemoryFilter, MemoryOrder, ProjectScope};
use crate::util::time;

/// Format version, so a future field can be added without breaking readers.
pub const INVENTORY_VERSION: u32 = 1;

/// A survey of one ContextD home.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inventory {
    pub contextd_inventory: u32,
    /// Version of the binary that produced it.
    pub version: String,
    pub host: Option<String>,
    /// The home this describes, as resolved on that machine.
    pub home: String,
    pub schema_version: i64,
    pub generated_at: DateTime<Utc>,
    pub totals: Totals,
    pub projects: Vec<ProjectSummary>,
    /// Memories that belong to no project.
    pub global: ScopeSummary,
    /// Embedding provider and vector backend in use over there.
    pub embeddings: String,
    pub vector_backend: String,
}

/// Counts across the whole store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Totals {
    pub projects: usize,
    pub memories: usize,
    pub active_memories: usize,
    pub superseded_memories: usize,
    pub decisions: usize,
    pub checkpoints: usize,
    pub sessions: usize,
    /// Deletions still propagating.
    pub tombstones: usize,
}

/// Per-project summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub name: String,
    pub slug: String,
    pub git_remote: Option<String>,
    pub root_path: Option<String>,
    pub active: bool,
    pub memories: usize,
    pub active_memories: usize,
    pub decisions: usize,
    pub checkpoints: usize,
    /// Newest memory, decision or checkpoint in this project.
    pub last_activity: Option<DateTime<Utc>>,
    pub last_checkpoint: Option<String>,
    pub categories: Vec<CategoryCount>,
}

/// Summary of memories in a scope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScopeSummary {
    pub memories: usize,
    pub categories: Vec<CategoryCount>,
    pub last_activity: Option<DateTime<Utc>>,
}

/// One category and how many memories carry it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryCount {
    pub category: Category,
    pub count: usize,
}

impl Inventory {
    /// True when the store has nothing in it yet.
    pub fn is_empty(&self) -> bool {
        self.totals.memories == 0 && self.totals.decisions == 0 && self.totals.checkpoints == 0
    }

    /// Newest activity anywhere in the store.
    pub fn last_activity(&self) -> Option<DateTime<Utc>> {
        self.projects
            .iter()
            .filter_map(|project| project.last_activity)
            .chain(self.global.last_activity)
            .max()
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// Parse an inventory produced by another machine.
    pub fn from_json(text: &str) -> Result<Self> {
        let inventory: Inventory = serde_json::from_str(text).map_err(|err| {
            crate::error::Error::invalid("inventory", format!("not a ContextD inventory: {err}"))
        })?;
        if inventory.contextd_inventory > INVENTORY_VERSION {
            return Err(crate::error::Error::invalid(
                "inventory",
                format!(
                    "inventory format v{} is newer than this build supports (v{INVENTORY_VERSION})",
                    inventory.contextd_inventory
                ),
            ));
        }
        Ok(inventory)
    }
}

/// Survey the local store.
pub fn collect(app: &App) -> Result<Inventory> {
    let store = app.store();
    let mut totals = Totals::default();

    let mut projects = Vec::new();
    for project in store.list_projects(true)? {
        let stats = store.project_stats(&project.id)?;
        let scope = ProjectScope::Project(project.id.clone());

        totals.projects += 1;
        totals.memories += stats.memories;
        totals.active_memories += stats.active_memories;
        totals.superseded_memories += stats.superseded_memories;
        totals.decisions += stats.decisions;
        totals.checkpoints += stats.checkpoints;
        totals.sessions += stats.sessions;

        let last_checkpoint = store.latest_checkpoint(&project.id)?;
        let last_activity = [
            newest_memory(app, &scope)?,
            store.list_decisions(&project.id, true)?.first().map(|d| d.updated_at),
            last_checkpoint.as_ref().map(|c| c.created_at),
        ]
        .into_iter()
        .flatten()
        .max();

        projects.push(ProjectSummary {
            name: project.name,
            slug: project.slug,
            git_remote: project.git_remote,
            root_path: project.root_path.map(|p| p.display().to_string()),
            active: project.active,
            memories: stats.memories,
            active_memories: stats.active_memories,
            decisions: stats.decisions,
            checkpoints: stats.checkpoints,
            last_activity,
            last_checkpoint: last_checkpoint.map(|c| c.summary),
            categories: categories(app, &scope)?,
        });
    }

    let global_memories = store.count_memories(&MemoryFilter {
        statuses: crate::core::model::Status::ALL.to_vec(),
        ..MemoryFilter::for_scope(ProjectScope::GlobalOnly)
    })?;
    totals.memories += global_memories;
    totals.tombstones = store.tombstones(&ProjectScope::Any, None)?.len();

    let global = ScopeSummary {
        memories: global_memories,
        categories: categories(app, &ProjectScope::GlobalOnly)?,
        last_activity: newest_memory(app, &ProjectScope::GlobalOnly)?,
    };

    Ok(Inventory {
        contextd_inventory: INVENTORY_VERSION,
        version: crate::VERSION.to_string(),
        host: hostname(),
        home: app.paths().root().display().to_string(),
        schema_version: store.schema_version()?,
        generated_at: time::now(),
        totals,
        projects,
        global,
        embeddings: match app.embedder() {
            Some(provider) => format!("{} · {}", provider.id(), provider.model()),
            None => "disabled".to_string(),
        },
        vector_backend: app.config().vector.backend.clone(),
    })
}

fn categories(app: &App, scope: &ProjectScope) -> Result<Vec<CategoryCount>> {
    Ok(app
        .store()
        .category_counts(scope)?
        .into_iter()
        .map(|(category, count)| CategoryCount { category, count })
        .collect())
}

fn newest_memory(app: &App, scope: &ProjectScope) -> Result<Option<DateTime<Utc>>> {
    Ok(app
        .store()
        .list_memories(&MemoryFilter {
            statuses: crate::core::model::Status::ALL.to_vec(),
            order: MemoryOrder::RecentFirst,
            limit: Some(1),
            ..MemoryFilter::for_scope(scope.clone())
        })?
        .first()
        .map(|memory| memory.updated_at))
}

/// Best-effort machine name, for labelling the survey.
fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        })
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::core::checkpoint::{CheckpointService, NewCheckpoint};
    use crate::core::decision::{DecisionService, NewDecision};
    use crate::core::memory::{MemoryService, NewMemory};
    use crate::core::project::{AttachRequest, ProjectService};

    fn fixture() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let app =
            App::open_or_create(Paths::with_root(dir.path().join("home"))).unwrap().with_cwd(&repo);
        (dir, app)
    }

    #[test]
    fn an_empty_store_surveys_cleanly() {
        let (_dir, app) = fixture();
        let inventory = collect(&app).unwrap();
        assert!(inventory.is_empty());
        assert_eq!(inventory.totals.projects, 0);
        assert!(inventory.last_activity().is_none());
        assert_eq!(inventory.contextd_inventory, INVENTORY_VERSION);
    }

    #[test]
    fn a_populated_store_is_summarised_per_project() {
        let (_dir, app) = fixture();
        let (project, _) = ProjectService::new(&app)
            .attach(AttachRequest {
                dir: app.cwd().to_path_buf(),
                name: Some("FerroGrid".into()),
                description: None,
                bindings: vec![],
            })
            .unwrap();

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
                supersedes: Some(redis.id),
                ..NewMemory::new(Category::Architecture, "Task queue is NATS")
            })
            .unwrap();
        memories
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Convention, "Run clippy before pushing")
            })
            .unwrap();
        memories.add(NewMemory::new(Category::User, "Prefers small commits")).unwrap();
        DecisionService::new(&app).record(&project, NewDecision::new("Transport", "NATS")).unwrap();
        CheckpointService::new(&app)
            .create(
                &project,
                NewCheckpoint {
                    summary: "heartbeat done".into(),
                    skip_git: true,
                    ..Default::default()
                },
            )
            .unwrap();

        let inventory = collect(&app).unwrap();
        assert!(!inventory.is_empty());
        assert_eq!(inventory.totals.projects, 1);
        assert_eq!(inventory.totals.memories, 4, "three project memories plus one global");
        assert_eq!(inventory.totals.active_memories, 2);
        assert_eq!(inventory.totals.superseded_memories, 1);
        assert_eq!(inventory.totals.decisions, 1);
        assert_eq!(inventory.totals.checkpoints, 1);

        let summary = &inventory.projects[0];
        assert_eq!(summary.name, "FerroGrid");
        assert_eq!(summary.memories, 3);
        assert_eq!(summary.last_checkpoint.as_deref(), Some("heartbeat done"));
        assert!(summary.last_activity.is_some());
        assert!(summary
            .categories
            .iter()
            .any(|c| c.category == Category::Architecture && c.count == 1));

        assert_eq!(inventory.global.memories, 1);
        assert_eq!(inventory.global.categories[0].category, Category::User);
        assert!(inventory.last_activity().is_some());
    }

    #[test]
    fn inventories_round_trip_as_json() {
        let (_dir, app) = fixture();
        let inventory = collect(&app).unwrap();
        let parsed = Inventory::from_json(&inventory.to_json().unwrap()).unwrap();
        assert_eq!(parsed.home, inventory.home);
        assert_eq!(parsed.version, crate::VERSION);
    }

    #[test]
    fn a_newer_inventory_format_is_refused() {
        let text = r#"{"contextd_inventory":99,"version":"9.0","host":null,"home":"/x",
            "schema_version":4,"generated_at":"2026-01-01T00:00:00Z",
            "totals":{"projects":0,"memories":0,"active_memories":0,"superseded_memories":0,
            "decisions":0,"checkpoints":0,"sessions":0,"tombstones":0},"projects":[],
            "global":{"memories":0,"categories":[],"last_activity":null},
            "embeddings":"local","vector_backend":"sqlite"}"#;
        assert!(Inventory::from_json(text)
            .unwrap_err()
            .to_string()
            .contains("newer than this build"));
        assert!(Inventory::from_json("not json").is_err());
    }
}
