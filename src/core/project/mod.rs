//! Project lifecycle: detect, attach, detach, describe.

use std::path::{Path, PathBuf};

use crate::app::App;
use crate::core::model::{AgentBinding, Checkpoint, Project};
use crate::error::{Error, Result};
use crate::storage::repository::ProjectStats;
use crate::util::git::GitSnapshot;
use crate::util::{ids, time};

/// What ContextD could work out about a directory before attaching it.
#[derive(Debug, Clone)]
pub struct Detection {
    /// Repository root if the directory is inside a git repository, otherwise
    /// the directory itself.
    pub root: PathBuf,
    /// Suggested project name.
    pub name: String,
    pub git: GitSnapshot,
    /// Agent configuration files found in the repository.
    pub agent_files: Vec<PathBuf>,
}

/// Request to attach a directory.
#[derive(Debug, Clone)]
pub struct AttachRequest {
    pub dir: PathBuf,
    /// Override the detected name.
    pub name: Option<String>,
    pub description: Option<String>,
    /// `(agent, path)` pairs to record as bindings.
    pub bindings: Vec<(String, PathBuf)>,
}

/// A project plus the numbers `contextd status` prints.
#[derive(Debug, Clone)]
pub struct StatusReport {
    pub project: Option<Project>,
    pub stats: ProjectStats,
    pub git: GitSnapshot,
    pub latest_checkpoint: Option<Checkpoint>,
    pub bindings: Vec<AgentBinding>,
    /// Records with a current vector, and the total that could have one.
    pub embedded: (usize, usize),
    pub embedding_provider: String,
}

/// Project operations.
pub struct ProjectService<'a> {
    app: &'a App,
}

impl<'a> ProjectService<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    /// Inspect a directory without changing anything.
    ///
    /// The project name comes from the git remote when there is one (so a
    /// clone into `/tmp/x` is still called `FerroGrid`), otherwise from the
    /// repository root's directory name.
    pub fn detect(&self, dir: &Path, candidate_files: &[PathBuf]) -> Detection {
        let git = GitSnapshot::capture(dir);
        let root = git.root.clone().unwrap_or_else(|| dir.to_path_buf());
        let name = git
            .remote
            .as_deref()
            .and_then(name_from_remote)
            .or_else(|| {
                root.file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| n != "/")
            })
            .unwrap_or_else(|| "project".to_string());
        let agent_files =
            candidate_files.iter().filter(|p| p.exists()).map(|p| p.to_path_buf()).collect();
        Detection { root, name, git, agent_files }
    }

    /// Attach a directory, or return the project already attached there.
    ///
    /// Re-attaching is idempotent: it refreshes git metadata and bindings
    /// rather than creating a second project for the same repository.
    pub fn attach(&self, request: AttachRequest) -> Result<(Project, bool)> {
        let detection = self.detect(&request.dir, &[]);
        let store = self.app.store();

        if let Some(mut existing) = store.find_project_by_path(&detection.root)? {
            existing.git_remote = detection.git.remote.clone().or(existing.git_remote);
            existing.default_branch = detection.git.branch.clone().or(existing.default_branch);
            if let Some(description) = request.description {
                existing.description = Some(description);
            }
            existing.active = true;
            existing.updated_at = time::now();
            store.update_project(&existing)?;
            self.record_bindings(&existing, &request.bindings)?;
            return Ok((existing, false));
        }

        let name = request.name.unwrap_or(detection.name);
        let project = Project {
            id: ids::new_id(),
            slug: self.unique_slug(&name)?,
            name,
            root_path: Some(detection.root.clone()),
            description: request.description,
            git_remote: detection.git.remote.clone(),
            default_branch: detection.git.branch.clone(),
            created_at: time::now(),
            updated_at: time::now(),
            active: true,
        };
        store.create_project(&project)?;
        self.record_bindings(&project, &request.bindings)?;
        Ok((project, true))
    }

    /// Detach a project. By default the memories are kept and the project is
    /// merely deactivated; `purge` deletes everything belonging to it.
    pub fn detach(&self, project: &Project, purge: bool) -> Result<()> {
        let store = self.app.store();
        if purge {
            store.delete_project(&project.id)?;
            return Ok(());
        }
        let mut project = project.clone();
        project.active = false;
        project.updated_at = time::now();
        store.update_project(&project)
    }

    /// Reactivate a previously detached project.
    pub fn reattach(&self, project: &Project) -> Result<Project> {
        let mut project = project.clone();
        project.active = true;
        project.updated_at = time::now();
        self.app.store().update_project(&project)?;
        Ok(project)
    }

    /// Everything `contextd status` needs.
    pub fn status(&self, project: Option<&Project>) -> Result<StatusReport> {
        let store = self.app.store();
        let (stats, latest_checkpoint, bindings) = match project {
            Some(p) => (
                store.project_stats(&p.id)?,
                store.latest_checkpoint(&p.id)?,
                store.list_bindings(&p.id)?,
            ),
            None => (ProjectStats::default(), None, Vec::new()),
        };

        // Coverage must be measured against the *provider's* model, not the
        // configured model name: the local embedder reports its own model, and
        // comparing against config would mark every record as stale.
        let scope = self.app.scope_for(project);
        let embedded = crate::search::IndexService::new(self.app).coverage(&scope)?;
        let provider = match self.app.embedder() {
            Some(provider) => format!("{} · {}", provider.id(), provider.model()),
            None => "disabled".to_string(),
        };

        Ok(StatusReport {
            project: project.cloned(),
            stats,
            git: project
                .and_then(|p| p.root_path.clone())
                .map(|root| GitSnapshot::capture(&root))
                .unwrap_or_default(),
            latest_checkpoint,
            bindings,
            embedded,
            embedding_provider: provider,
        })
    }

    /// Record agent bindings, preserving prior export/import timestamps.
    fn record_bindings(&self, project: &Project, bindings: &[(String, PathBuf)]) -> Result<()> {
        for (agent, path) in bindings {
            self.app.store().upsert_binding(&AgentBinding {
                id: ids::new_id(),
                project_id: project.id.clone(),
                agent: agent.clone(),
                path: path.clone(),
                last_hash: None,
                last_exported_at: None,
                last_imported_at: None,
            })?;
        }
        Ok(())
    }

    /// Derive a slug that is not taken yet (`ferrogrid`, `ferrogrid-2`, …).
    fn unique_slug(&self, name: &str) -> Result<String> {
        let base = ids::slugify(name);
        let base = if base.is_empty() { "project".to_string() } else { base };
        if self.app.store().find_project_by_slug(&base)?.is_none() {
            return Ok(base);
        }
        for n in 2..1000 {
            let candidate = format!("{base}-{n}");
            if self.app.store().find_project_by_slug(&candidate)?.is_none() {
                return Ok(candidate);
            }
        }
        Err(Error::invalid("name", format!("cannot derive a free slug from `{name}`")))
    }
}

/// `git@github.com:acme/FerroGrid.git` → `FerroGrid`.
fn name_from_remote(remote: &str) -> Option<String> {
    let trimmed = remote.trim_end_matches('/').trim_end_matches(".git");
    let last = trimmed.rsplit(['/', ':']).find(|s| !s.is_empty())?;
    (!last.is_empty()).then(|| last.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

    fn app_in(dir: &Path) -> App {
        App::open_or_create(Paths::with_root(dir.join("home"))).unwrap()
    }

    #[test]
    fn remote_names() {
        assert_eq!(name_from_remote("git@github.com:acme/FerroGrid.git").unwrap(), "FerroGrid");
        assert_eq!(name_from_remote("https://github.com/acme/ferro-grid").unwrap(), "ferro-grid");
        assert_eq!(name_from_remote("https://x.dev/a/b.git/").unwrap(), "b");
    }

    #[test]
    fn detect_uses_directory_name_without_git() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("FerroGrid");
        std::fs::create_dir_all(&repo).unwrap();
        let app = app_in(dir.path());
        let detection = ProjectService::new(&app).detect(&repo, &[]);
        assert_eq!(detection.name, "FerroGrid");
        assert_eq!(detection.root, repo);
        assert!(detection.git.is_empty());
    }

    #[test]
    fn detect_reports_existing_agent_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("CLAUDE.md"), "# hi").unwrap();
        let app = app_in(dir.path());
        let detection = ProjectService::new(&app)
            .detect(&repo, &[repo.join("CLAUDE.md"), repo.join("AGENTS.md")]);
        assert_eq!(detection.agent_files, vec![repo.join("CLAUDE.md")]);
    }

    #[test]
    fn attach_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let app = app_in(dir.path());
        let service = ProjectService::new(&app);

        let request = || AttachRequest {
            dir: repo.clone(),
            name: None,
            description: None,
            bindings: vec![("claude".into(), repo.join("CLAUDE.md"))],
        };
        let (first, created) = service.attach(request()).unwrap();
        assert!(created);
        let (second, created_again) = service.attach(request()).unwrap();
        assert!(!created_again);
        assert_eq!(first.id, second.id);
        assert_eq!(app.store().list_projects(true).unwrap().len(), 1);
        assert_eq!(app.store().list_bindings(&first.id).unwrap().len(), 1);
    }

    #[test]
    fn slugs_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_in(dir.path());
        let service = ProjectService::new(&app);
        let a = dir.path().join("a/repo");
        let b = dir.path().join("b/repo");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let (p1, _) = service
            .attach(AttachRequest { dir: a, name: None, description: None, bindings: vec![] })
            .unwrap();
        let (p2, _) = service
            .attach(AttachRequest { dir: b, name: None, description: None, bindings: vec![] })
            .unwrap();
        assert_eq!(p1.slug, "repo");
        assert_eq!(p2.slug, "repo-2");
    }

    #[test]
    fn detach_keeps_data_unless_purged() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let app = app_in(dir.path());
        let service = ProjectService::new(&app);
        let (project, _) = service
            .attach(AttachRequest {
                dir: repo.clone(),
                name: None,
                description: None,
                bindings: vec![],
            })
            .unwrap();

        service.detach(&project, false).unwrap();
        assert!(app.store().get_project(&project.id).unwrap().is_some());
        assert!(app.store().list_projects(false).unwrap().is_empty());

        service.detach(&project, true).unwrap();
        assert!(app.store().get_project(&project.id).unwrap().is_none());
    }

    #[test]
    fn status_without_project_is_empty_but_valid() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_in(dir.path());
        let report = ProjectService::new(&app).status(None).unwrap();
        assert!(report.project.is_none());
        assert_eq!(report.stats.memories, 0);
    }
}
