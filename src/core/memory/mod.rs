//! Memory operations: the CRUD layer plus the rules that keep *current truth*
//! distinguishable from *historical truth*.

use crate::app::App;
use crate::core::model::{Category, Memory, Project, Source, Status, DEFAULT_PRIORITY};
use crate::error::{Error, Result};
use crate::storage::repository::{MemoryFilter, ProjectScope};
use crate::util::text;
use crate::util::time;

/// Input for [`MemoryService::add`].
#[derive(Debug, Clone)]
pub struct NewMemory {
    pub project: Option<Project>,
    pub category: Category,
    /// Derived from the content when absent.
    pub title: Option<String>,
    pub content: String,
    pub priority: Option<i64>,
    pub tags: Vec<String>,
    pub files: Vec<String>,
    pub commit: Option<String>,
    pub symbol: Option<String>,
    pub source: Source,
    /// Memory this one replaces; it is marked superseded atomically.
    pub supersedes: Option<String>,
}

impl NewMemory {
    /// A project-scoped manual memory, the common case.
    pub fn new(category: Category, content: impl Into<String>) -> Self {
        Self {
            project: None,
            category,
            title: None,
            content: content.into(),
            priority: None,
            tags: Vec::new(),
            files: Vec::new(),
            commit: None,
            symbol: None,
            source: Source::Manual,
            supersedes: None,
        }
    }
}

/// Fields to change in [`MemoryService::edit`]; `None` leaves them alone.
#[derive(Debug, Clone, Default)]
pub struct MemoryPatch {
    pub title: Option<String>,
    pub content: Option<String>,
    pub category: Option<Category>,
    pub priority: Option<i64>,
    pub status: Option<Status>,
    pub tags: Option<Vec<String>>,
    pub files: Option<Vec<String>>,
    pub commit: Option<String>,
    pub symbol: Option<String>,
}

impl MemoryPatch {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.content.is_none()
            && self.category.is_none()
            && self.priority.is_none()
            && self.status.is_none()
            && self.tags.is_none()
            && self.files.is_none()
            && self.commit.is_none()
            && self.symbol.is_none()
    }
}

/// Memory operations.
pub struct MemoryService<'a> {
    app: &'a App,
}

impl<'a> MemoryService<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    /// Store a new memory.
    pub fn add(&self, input: NewMemory) -> Result<Memory> {
        let content = input.content.trim().to_string();
        if content.is_empty() {
            return Err(Error::invalid("content", "must not be empty"));
        }
        let title = match input.title {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => derive_title(&content),
        };

        let memory = Memory {
            id: crate::util::ids::new_id(),
            project_id: input.project.as_ref().map(|p| p.id.clone()),
            category: input.category,
            title,
            content,
            source: input.source,
            priority: input.priority.unwrap_or(DEFAULT_PRIORITY),
            status: Status::Active,
            superseded_by: None,
            tags: normalise_tags(input.tags),
            files: input.files,
            commit: input.commit,
            symbol: input.symbol,
            created_at: time::now(),
            updated_at: time::now(),
        };
        memory.validate()?;

        let store = self.app.store();
        store.create_memory(&memory)?;
        if let Some(previous) = input.supersedes {
            let old = store
                .resolve_memory(&previous)?
                .ok_or_else(|| Error::MemoryNotFound(previous.clone()))?;
            store.supersede_memory(&old.id, &memory.id)?;
        }
        Ok(memory)
    }

    /// Look up by id or unique prefix.
    pub fn get(&self, ident: &str) -> Result<Memory> {
        self.app
            .store()
            .resolve_memory(ident)?
            .ok_or_else(|| Error::MemoryNotFound(ident.to_string()))
    }

    /// Apply a patch.
    pub fn edit(&self, ident: &str, patch: MemoryPatch) -> Result<Memory> {
        if patch.is_empty() {
            return Err(Error::invalid("patch", "no fields to change"));
        }
        let mut memory = self.get(ident)?;
        if let Some(title) = patch.title {
            memory.title = title.trim().to_string();
        }
        if let Some(content) = patch.content {
            memory.content = content.trim().to_string();
        }
        if let Some(category) = patch.category {
            memory.category = category;
        }
        if let Some(priority) = patch.priority {
            memory.priority = priority;
        }
        if let Some(status) = patch.status {
            // Clearing the superseded state must also clear the pointer, or a
            // reactivated memory would keep claiming it was replaced.
            if status.is_current() {
                memory.superseded_by = None;
            }
            memory.status = status;
        }
        if let Some(tags) = patch.tags {
            memory.tags = normalise_tags(tags);
        }
        if let Some(files) = patch.files {
            memory.files = files;
        }
        if let Some(commit) = patch.commit {
            memory.commit = Some(commit);
        }
        if let Some(symbol) = patch.symbol {
            memory.symbol = Some(symbol);
        }
        memory.updated_at = time::now();
        memory.validate()?;
        self.app.store().update_memory(&memory)?;
        Ok(memory)
    }

    /// Delete permanently. Prefer [`MemoryService::archive`] for history.
    pub fn delete(&self, ident: &str) -> Result<Memory> {
        let memory = self.get(ident)?;
        self.app.store().delete_memory(&memory.id)?;
        Ok(memory)
    }

    /// Move to `archived`: kept for the record, out of normal retrieval.
    pub fn archive(&self, ident: &str) -> Result<Memory> {
        self.edit(ident, MemoryPatch { status: Some(Status::Archived), ..Default::default() })
    }

    /// Mark `old` as replaced by `new`.
    pub fn supersede(&self, old_ident: &str, new_ident: &str) -> Result<(Memory, Memory)> {
        let old = self.get(old_ident)?;
        let new = self.get(new_ident)?;
        self.app.store().supersede_memory(&old.id, &new.id)?;
        Ok((self.get(&old.id)?, new))
    }

    /// List with a filter.
    pub fn list(&self, filter: &MemoryFilter) -> Result<Vec<Memory>> {
        self.app.store().list_memories(filter)
    }

    /// Memories that apply while working on `project`: its own plus global.
    pub fn for_project(&self, project: Option<&Project>, limit: usize) -> Result<Vec<Memory>> {
        let scope = match project {
            Some(p) => ProjectScope::ProjectWithGlobal(p.id.clone()),
            None => ProjectScope::GlobalOnly,
        };
        self.list(&MemoryFilter::for_scope(scope).with_limit(limit))
    }
}

/// First sentence or line of the content, capped to a sensible title length.
fn derive_title(content: &str) -> String {
    let first_line = content.lines().find(|l| !l.trim().is_empty()).unwrap_or(content).trim();
    let stripped = first_line.trim_start_matches(['#', '-', '*', ' ']);
    let sentence = stripped
        .split_inclusive(['.', '。', '!', '?', '！', '？'])
        .next()
        .unwrap_or(stripped)
        .trim()
        .trim_end_matches(['.', '。']);
    let candidate = if sentence.is_empty() { stripped } else { sentence };
    text::truncate_chars(candidate, 72)
}

fn normalise_tags(tags: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = tags
        .into_iter()
        .flat_map(|t| t.split(',').map(str::to_string).collect::<Vec<_>>())
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::core::project::{AttachRequest, ProjectService};
    use std::path::Path;

    fn setup() -> (tempfile::TempDir, App, Project) {
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

    #[test]
    fn title_is_derived_from_content() {
        assert_eq!(
            derive_title("GPU scheduler uses NATS. It replaced Redis."),
            "GPU scheduler uses NATS"
        );
        assert_eq!(derive_title("# Heading\nbody"), "Heading");
        assert_eq!(derive_title("排程器使用 NATS。之後再說"), "排程器使用 NATS");
        assert_eq!(derive_title(&"x".repeat(200)).chars().count(), 73); // 72 + ellipsis
    }

    #[test]
    fn tags_are_normalised() {
        assert_eq!(
            normalise_tags(vec!["  NATS ".into(), "infra,queue".into(), "nats".into(), "".into()]),
            vec!["infra".to_string(), "nats".to_string(), "queue".to_string()]
        );
    }

    #[test]
    fn add_and_get() {
        let (_dir, app, project) = setup();
        let service = MemoryService::new(&app);
        let memory = service
            .add(NewMemory {
                project: Some(project.clone()),
                tags: vec!["NATS".into()],
                ..NewMemory::new(Category::Architecture, "GPU scheduler uses NATS for transport")
            })
            .unwrap();

        assert_eq!(memory.title, "GPU scheduler uses NATS for transport");
        assert_eq!(memory.project_id.as_deref(), Some(project.id.as_str()));
        assert_eq!(service.get(&memory.id[..8]).unwrap().id, memory.id);
    }

    #[test]
    fn empty_content_is_rejected() {
        let (_dir, app, _project) = setup();
        let err = MemoryService::new(&app).add(NewMemory::new(Category::Task, "   "));
        assert!(err.is_err());
    }

    #[test]
    fn add_with_supersedes_closes_the_old_memory() {
        let (_dir, app, project) = setup();
        let service = MemoryService::new(&app);
        let redis = service
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Architecture, "Task queue is Redis")
            })
            .unwrap();
        let nats = service
            .add(NewMemory {
                project: Some(project.clone()),
                supersedes: Some(redis.id[..8].to_string()),
                ..NewMemory::new(Category::Architecture, "Task queue is NATS")
            })
            .unwrap();

        let old = service.get(&redis.id).unwrap();
        assert_eq!(old.status, Status::Superseded);
        assert_eq!(old.superseded_by.as_deref(), Some(nats.id.as_str()));

        let current = service.for_project(Some(&project), 10).unwrap();
        assert_eq!(current.len(), 1);
        assert!(current[0].content.contains("NATS"));
    }

    #[test]
    fn superseding_unknown_memory_fails() {
        let (_dir, app, project) = setup();
        let err = MemoryService::new(&app).add(NewMemory {
            project: Some(project),
            supersedes: Some("does-not-exist".into()),
            ..NewMemory::new(Category::Architecture, "x")
        });
        assert!(matches!(err, Err(Error::MemoryNotFound(_))));
    }

    #[test]
    fn edit_applies_only_given_fields() {
        let (_dir, app, project) = setup();
        let service = MemoryService::new(&app);
        let memory = service
            .add(NewMemory {
                project: Some(project),
                priority: Some(2),
                ..NewMemory::new(Category::Task, "Wire up heartbeat")
            })
            .unwrap();

        let edited = service
            .edit(
                &memory.id,
                MemoryPatch {
                    priority: Some(5),
                    tags: Some(vec!["urgent".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(edited.priority, 5);
        assert_eq!(edited.tags, vec!["urgent".to_string()]);
        assert_eq!(edited.content, "Wire up heartbeat");
        assert!(service.edit(&memory.id, MemoryPatch::default()).is_err());
    }

    #[test]
    fn reactivating_clears_the_supersede_pointer() {
        let (_dir, app, project) = setup();
        let service = MemoryService::new(&app);
        let a = service
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Task, "a")
            })
            .unwrap();
        let b = service
            .add(NewMemory { project: Some(project), ..NewMemory::new(Category::Task, "b") })
            .unwrap();
        service.supersede(&a.id, &b.id).unwrap();

        let revived = service
            .edit(&a.id, MemoryPatch { status: Some(Status::Active), ..Default::default() })
            .unwrap();
        assert!(revived.superseded_by.is_none());
        assert_eq!(revived.status, Status::Active);
    }

    #[test]
    fn archive_and_delete() {
        let (_dir, app, project) = setup();
        let service = MemoryService::new(&app);
        let memory = service
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Task, "x")
            })
            .unwrap();
        assert_eq!(service.archive(&memory.id).unwrap().status, Status::Archived);
        assert!(service.for_project(Some(&project), 10).unwrap().is_empty());
        service.delete(&memory.id).unwrap();
        assert!(matches!(service.get(&memory.id), Err(Error::MemoryNotFound(_))));
    }

    #[test]
    fn global_memories_apply_to_every_project() {
        let (_dir, app, project) = setup();
        let service = MemoryService::new(&app);
        service.add(NewMemory::new(Category::User, "Prefers small commits")).unwrap();
        service
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Task, "x")
            })
            .unwrap();
        assert_eq!(service.for_project(Some(&project), 10).unwrap().len(), 2);
        assert_eq!(service.for_project(None, 10).unwrap().len(), 1);
        let _ = Path::new("/");
    }
}
