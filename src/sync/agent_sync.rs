//! Importing from and exporting to agent configuration files.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::agents::{self, markdown, AgentAdapter, AgentFile};
use crate::app::App;
use crate::core::context::{ContextBuilder, ContextRequest};
use crate::core::memory::{MemoryService, NewMemory};
use crate::core::model::{AgentBinding, Category, Memory, Project, Source};
use crate::error::{Error, Result};
use crate::storage::repository::{MemoryFilter, ProjectScope};
use crate::sync::{is_conflict, read_or_empty, write_atomic, FileOutcome, WriteStatus};
use crate::util::hash::content_hash;
use crate::util::{ids, time};

/// Options for [`AgentSync::export`].
#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub agent: String,
    /// Write somewhere other than the adapter's default.
    pub path: Option<PathBuf>,
    /// Overwrite even when the managed block was edited by hand.
    pub force: bool,
    /// Render but do not write.
    pub dry_run: bool,
    /// Narrow the exported context to a question.
    pub query: Option<String>,
}

impl ExportOptions {
    pub fn new(agent: impl Into<String>) -> Self {
        Self { agent: agent.into(), path: None, force: false, dry_run: false, query: None }
    }
}

/// Result of an export.
#[derive(Debug, Clone, Serialize)]
pub struct ExportResult {
    pub agent: String,
    pub outcome: FileOutcome,
    /// The rendered content, so `--dry-run` can print it.
    pub content: String,
    pub memories_included: usize,
    pub tokens: usize,
    pub dropped: usize,
}

/// Result of an import.
#[derive(Debug, Clone, Serialize)]
pub struct ImportResult {
    pub agent: String,
    pub imported: Vec<String>,
    pub skipped_duplicates: usize,
    pub files: Vec<PathBuf>,
}

/// Moves context between ContextD and agent files.
pub struct AgentSync<'a> {
    app: &'a App,
}

impl<'a> AgentSync<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    /// Render project context into the agent's file.
    pub async fn export(
        &self,
        project: Option<&Project>,
        options: &ExportOptions,
    ) -> Result<ExportResult> {
        let adapter = agents::get(&options.agent)?;
        let mut request = ContextRequest::from_config(self.app, project.cloned());
        if let Some(query) = &options.query {
            request = request.with_query(query.clone());
        }
        let bundle = ContextBuilder::new(self.app).build(&request).await?;
        let generated = adapter.render(&bundle);

        let path = match &options.path {
            Some(path) => path.clone(),
            None => {
                let root = project
                    .and_then(|p| p.root_path.clone())
                    .unwrap_or_else(|| self.app.cwd().to_path_buf());
                adapter.export_path(&root)
            }
        };

        let existing = read_or_empty(&path)?;
        let binding = self.binding_for(project, adapter.as_ref(), &path)?;
        let previously_written = binding.as_ref().and_then(|b| b.last_hash.clone());

        // For a file ContextD owns outright the whole content is compared;
        // for a shared file only the marked block is.
        let owns_file = adapter.owns_file();
        let current_block = if owns_file {
            (!existing.trim().is_empty()).then(|| existing.trim().to_string())
        } else {
            markdown::managed_block(&existing)
        };

        // A shared file with no managed block at all is safe to append to; only
        // an *edited* block, or an unknown block we never wrote, is a conflict.
        let conflicted = current_block.is_some()
            && is_conflict(previously_written.as_deref(), current_block.as_deref());
        if conflicted && !options.force {
            return Ok(ExportResult {
                agent: adapter.id().to_string(),
                outcome: FileOutcome::new(path, WriteStatus::Conflict).with_detail(
                    "the ContextD block was edited by hand; re-run with --force to overwrite",
                ),
                content: generated,
                memories_included: bundle.memories.len(),
                tokens: bundle.budget.used_tokens,
                dropped: bundle.budget.dropped,
            });
        }

        let updated = if owns_file {
            format!("{}\n", generated.trim_end())
        } else {
            markdown::splice_managed_block(&existing, &generated)
        };
        let status = if options.dry_run {
            WriteStatus::Skipped
        } else if existing.trim().is_empty() {
            WriteStatus::Created
        } else if updated == existing {
            WriteStatus::Unchanged
        } else {
            WriteStatus::Updated
        };

        let hash = content_hash(&generated);
        if !options.dry_run && status != WriteStatus::Unchanged {
            write_atomic(&path, &updated)?;
        }
        if !options.dry_run {
            self.record_export(project, adapter.as_ref(), &path, &hash)?;
        }

        Ok(ExportResult {
            agent: adapter.id().to_string(),
            outcome: FileOutcome::new(path, status).with_hash(hash),
            content: generated,
            memories_included: bundle.memories.len(),
            tokens: bundle.budget.used_tokens,
            dropped: bundle.budget.dropped,
        })
    }

    /// Read an agent's files into memories.
    ///
    /// Content ContextD generated is skipped (it lives inside the managed
    /// block), so importing after an export does not duplicate memories back
    /// into the database.
    pub fn import(
        &self,
        project: Option<&Project>,
        agent: &str,
        include_global: bool,
        dry_run: bool,
    ) -> Result<ImportResult> {
        let adapter = agents::get(agent)?;
        let root = project
            .and_then(|p| p.root_path.clone())
            .unwrap_or_else(|| self.app.cwd().to_path_buf());
        // Never import from a file ContextD generated: it would round-trip
        // exported context back in as new memories.
        let own_export = adapter.export_path(&root);
        let owns_file = adapter.owns_file();
        let files: Vec<AgentFile> = adapter
            .detect(&root, include_global)
            .into_iter()
            .filter(|f| f.exists)
            .filter(|f| !(owns_file && f.path == own_export))
            .collect();
        let items = adapter.import(&files)?;

        let existing = self.existing_memories(project)?;
        let memories = MemoryService::new(self.app);
        let mut imported = Vec::new();
        let mut skipped = 0;

        for item in items {
            if existing.iter().any(|m| is_same(m, &item.title, &item.content)) {
                skipped += 1;
                continue;
            }
            if dry_run {
                imported.push(item.title.clone());
                continue;
            }
            let memory = memories.add(NewMemory {
                // A global agent file describes the developer, not the project.
                project: if item.global { None } else { project.cloned() },
                category: if item.global && item.category == Category::Project {
                    Category::User
                } else {
                    item.category
                },
                title: Some(item.title.clone()),
                content: item.content.clone(),
                source: Source::Import {
                    agent: adapter.id().to_string(),
                    path: Some(item.source.to_string_lossy().into_owned()),
                },
                ..NewMemory::new(item.category, item.content.clone())
            })?;
            imported.push(memory.title);
        }

        if !dry_run {
            for file in &files {
                self.record_import(project, adapter.as_ref(), &file.path)?;
            }
        }

        Ok(ImportResult {
            agent: adapter.id().to_string(),
            imported,
            skipped_duplicates: skipped,
            files: files.into_iter().map(|f| f.path).collect(),
        })
    }

    fn existing_memories(&self, project: Option<&Project>) -> Result<Vec<Memory>> {
        let scope = match project {
            Some(p) => ProjectScope::ProjectWithGlobal(p.id.clone()),
            None => ProjectScope::GlobalOnly,
        };
        self.app.store().list_memories(&MemoryFilter {
            statuses: crate::core::model::Status::ALL.to_vec(),
            ..MemoryFilter::for_scope(scope)
        })
    }

    fn binding_for(
        &self,
        project: Option<&Project>,
        adapter: &dyn AgentAdapter,
        path: &Path,
    ) -> Result<Option<AgentBinding>> {
        let Some(project) = project else { return Ok(None) };
        self.app.store().find_binding(&project.id, adapter.id(), path)
    }

    fn record_export(
        &self,
        project: Option<&Project>,
        adapter: &dyn AgentAdapter,
        path: &Path,
        hash: &str,
    ) -> Result<()> {
        let Some(project) = project else { return Ok(()) };
        self.app.store().upsert_binding(&AgentBinding {
            id: ids::new_id(),
            project_id: project.id.clone(),
            agent: adapter.id().to_string(),
            path: path.to_path_buf(),
            last_hash: Some(hash.to_string()),
            last_exported_at: Some(time::now()),
            last_imported_at: None,
        })
    }

    fn record_import(
        &self,
        project: Option<&Project>,
        adapter: &dyn AgentAdapter,
        path: &Path,
    ) -> Result<()> {
        let Some(project) = project else { return Ok(()) };
        self.app.store().upsert_binding(&AgentBinding {
            id: ids::new_id(),
            project_id: project.id.clone(),
            agent: adapter.id().to_string(),
            path: path.to_path_buf(),
            last_hash: None,
            last_exported_at: None,
            last_imported_at: Some(time::now()),
        })
    }
}

/// Two memories are "the same" when title and body match after whitespace
/// normalisation — enough to make repeated imports idempotent without
/// pretending to understand the content.
fn is_same(memory: &Memory, title: &str, content: &str) -> bool {
    fn norm(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }
    norm(&memory.title) == norm(title) && norm(&memory.content) == norm(content)
}

/// Convenience for the CLI: export to every agent bound to the project.
pub async fn export_bound_agents(
    app: &App,
    project: &Project,
    force: bool,
) -> Result<Vec<ExportResult>> {
    let bindings = app.store().list_bindings(&project.id)?;
    let sync = AgentSync::new(app);
    let mut results = Vec::new();
    for binding in bindings {
        // A binding may name an agent this build does not know (a newer
        // ContextD wrote it); skip rather than fail the whole sync.
        if agents::get(&binding.agent).is_err() {
            continue;
        }
        results.push(
            sync.export(
                Some(project),
                &ExportOptions {
                    path: Some(binding.path.clone()),
                    force,
                    ..ExportOptions::new(binding.agent)
                },
            )
            .await?,
        );
    }
    Ok(results)
}

/// Error helper used when a caller insists on an unknown agent.
pub fn ensure_known_agent(agent: &str) -> Result<()> {
    agents::get(agent).map(|_| ()).map_err(|_| Error::UnknownAgent(agent.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::core::model::Category;
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

    fn add_memory(app: &App, project: &Project, category: Category, content: &str) {
        MemoryService::new(app)
            .add(NewMemory { project: Some(project.clone()), ..NewMemory::new(category, content) })
            .unwrap();
    }

    #[tokio::test]
    async fn export_creates_then_updates_the_managed_block() {
        let (_dir, app, project) = fixture().await;
        add_memory(&app, &project, Category::Architecture, "Scheduler transport is NATS");
        let sync = AgentSync::new(&app);

        let first = sync.export(Some(&project), &ExportOptions::new("claude")).await.unwrap();
        assert_eq!(first.outcome.status, WriteStatus::Created);
        let path = first.outcome.path.clone();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("NATS"));
        assert!(text.contains(markdown::BEGIN_MARKER));

        add_memory(&app, &project, Category::Convention, "Run cargo clippy before pushing");
        let second = sync.export(Some(&project), &ExportOptions::new("claude")).await.unwrap();
        assert_eq!(second.outcome.status, WriteStatus::Updated);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("clippy"));
        assert_eq!(text.matches(markdown::BEGIN_MARKER).count(), 1);
    }

    #[tokio::test]
    async fn export_preserves_hand_written_content() {
        let (_dir, app, project) = fixture().await;
        add_memory(&app, &project, Category::Architecture, "Scheduler transport is NATS");
        let path = project.root_path.clone().unwrap().join("CLAUDE.md");
        std::fs::write(&path, "# My own instructions\nAlways ask before deleting files.\n")
            .unwrap();

        AgentSync::new(&app).export(Some(&project), &ExportOptions::new("claude")).await.unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("Always ask before deleting files."));
        assert!(text.contains("NATS"));
    }

    #[tokio::test]
    async fn a_hand_edited_block_is_a_conflict_until_forced() {
        let (_dir, app, project) = fixture().await;
        add_memory(&app, &project, Category::Architecture, "Scheduler transport is NATS");
        let sync = AgentSync::new(&app);
        let first = sync.export(Some(&project), &ExportOptions::new("claude")).await.unwrap();
        let path = first.outcome.path.clone();

        let edited = std::fs::read_to_string(&path).unwrap().replace("NATS", "Kafka (edited)");
        std::fs::write(&path, &edited).unwrap();

        let blocked = sync.export(Some(&project), &ExportOptions::new("claude")).await.unwrap();
        assert_eq!(blocked.outcome.status, WriteStatus::Conflict);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited, "file must be untouched");

        let forced = sync
            .export(Some(&project), &ExportOptions { force: true, ..ExportOptions::new("claude") })
            .await
            .unwrap();
        assert_eq!(forced.outcome.status, WriteStatus::Updated);
        assert!(std::fs::read_to_string(&path).unwrap().contains("NATS"));
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let (_dir, app, project) = fixture().await;
        add_memory(&app, &project, Category::Architecture, "NATS");
        let result = AgentSync::new(&app)
            .export(Some(&project), &ExportOptions { dry_run: true, ..ExportOptions::new("codex") })
            .await
            .unwrap();
        assert_eq!(result.outcome.status, WriteStatus::Skipped);
        assert!(!result.outcome.path.exists());
        assert!(result.content.contains("NATS"));
    }

    #[tokio::test]
    async fn import_reads_sections_and_is_idempotent() {
        let (_dir, app, project) = fixture().await;
        let path = project.root_path.clone().unwrap().join("AGENTS.md");
        std::fs::write(
            &path,
            "# Coding conventions\nUse rustfmt and clippy.\n\n# Architecture\nWorkers pull leases.\n",
        )
        .unwrap();

        let sync = AgentSync::new(&app);
        let first = sync.import(Some(&project), "codex", false, false).unwrap();
        assert_eq!(first.imported.len(), 2);

        let second = sync.import(Some(&project), "codex", false, false).unwrap();
        assert!(second.imported.is_empty());
        assert_eq!(second.skipped_duplicates, 2);

        let categories: Vec<Category> = MemoryService::new(&app)
            .for_project(Some(&project), 10)
            .unwrap()
            .iter()
            .map(|m| m.category)
            .collect();
        assert!(categories.contains(&Category::Convention));
        assert!(categories.contains(&Category::Architecture));
    }

    #[tokio::test]
    async fn exported_content_is_not_reimported() {
        let (_dir, app, project) = fixture().await;
        add_memory(&app, &project, Category::Architecture, "Scheduler transport is NATS");
        let sync = AgentSync::new(&app);
        sync.export(Some(&project), &ExportOptions::new("claude")).await.unwrap();

        let result = sync.import(Some(&project), "claude", false, false).unwrap();
        assert!(result.imported.is_empty(), "the managed block must not round-trip");
        assert_eq!(MemoryService::new(&app).for_project(Some(&project), 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cursor_files_are_written_whole_with_front_matter_first() {
        let (_dir, app, project) = fixture().await;
        add_memory(&app, &project, Category::Architecture, "Scheduler transport is NATS");
        let sync = AgentSync::new(&app);

        let result = sync.export(Some(&project), &ExportOptions::new("cursor")).await.unwrap();
        let text = std::fs::read_to_string(&result.outcome.path).unwrap();
        assert!(text.starts_with("---\n"), "front matter must lead the file: {text}");
        assert!(!text.contains(markdown::BEGIN_MARKER));
        assert!(text.contains("NATS"));

        // Re-exporting is idempotent rather than appending a second document.
        let again = sync.export(Some(&project), &ExportOptions::new("cursor")).await.unwrap();
        assert_eq!(again.outcome.status, WriteStatus::Unchanged);
        assert_eq!(std::fs::read_to_string(&result.outcome.path).unwrap(), text);

        // And an edit to the generated file is still a conflict.
        std::fs::write(&result.outcome.path, text.replace("NATS", "Kafka")).unwrap();
        let blocked = sync.export(Some(&project), &ExportOptions::new("cursor")).await.unwrap();
        assert_eq!(blocked.outcome.status, WriteStatus::Conflict);
    }

    #[tokio::test]
    async fn cursor_import_ignores_contextd_own_file_but_reads_user_rules() {
        let (_dir, app, project) = fixture().await;
        add_memory(&app, &project, Category::Architecture, "Scheduler transport is NATS");
        let sync = AgentSync::new(&app);
        sync.export(Some(&project), &ExportOptions::new("cursor")).await.unwrap();

        let rules_dir = project.root_path.clone().unwrap().join(".cursor").join("rules");
        std::fs::write(
            rules_dir.join("style.mdc"),
            "---\ndescription: style\n---\n\n# Naming\n\nPrefer explicit names.\n",
        )
        .unwrap();

        let result = sync.import(Some(&project), "cursor", false, false).unwrap();
        assert_eq!(result.imported, vec!["Naming".to_string()]);
        assert!(result.files.iter().all(|p| p.file_name().unwrap() != "contextd.mdc"));
    }

    #[tokio::test]
    async fn export_to_every_bound_agent() {
        let (_dir, app, project) = fixture().await;
        add_memory(&app, &project, Category::Architecture, "NATS");
        let sync = AgentSync::new(&app);
        sync.export(Some(&project), &ExportOptions::new("claude")).await.unwrap();
        sync.export(Some(&project), &ExportOptions::new("codex")).await.unwrap();

        let results = export_bound_agents(&app, &project, false).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.outcome.status == WriteStatus::Unchanged));
    }

    #[test]
    fn unknown_agents_are_rejected() {
        assert!(ensure_known_agent("claude").is_ok());
        assert!(matches!(ensure_known_agent("vim"), Err(Error::UnknownAgent(_))));
    }
}
