//! Application context shared by the CLI and the MCP server.
//!
//! [`App`] owns the resolved paths, the configuration and the storage handle,
//! and knows how to answer the one question every command starts with: *which
//! project am I in?* Nothing here is CLI-specific — the MCP server builds the
//! same `App`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{Config, Paths};
use crate::core::model::Project;
use crate::embeddings::EmbeddingProvider;
use crate::error::{Error, Result};
use crate::storage::repository::{ProjectScope, Storage};
use crate::storage::SqliteStore;

/// Everything a command needs to run.
#[derive(Clone)]
pub struct App {
    paths: Paths,
    config: Config,
    store: Arc<dyn Storage>,
    /// `None` when embeddings are switched off in configuration.
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    cwd: PathBuf,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App").field("root", &self.paths.root()).field("cwd", &self.cwd).finish()
    }
}

impl App {
    /// Open an existing ContextD home. Fails if `contextd init` has not run.
    pub fn open(paths: Paths) -> Result<Self> {
        paths.require_initialised()?;
        Self::open_or_create(paths)
    }

    /// Open, creating the home directory and database if necessary.
    ///
    /// This is what `contextd init` uses; every other command goes through
    /// [`App::open`] so that a typo in `$CONTEXTD_HOME` reports a clear error
    /// instead of silently starting from an empty memory.
    pub fn open_or_create(paths: Paths) -> Result<Self> {
        paths.ensure_dirs()?;
        let config = Config::load(&paths.config_file())?;
        config.validate()?;
        let store = SqliteStore::open(&paths.database())?;
        let embedder = crate::embeddings::build(&config.embeddings)?;
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Ok(Self { paths, config, store: Arc::new(store), embedder, cwd })
    }

    /// Build from parts — used by tests and by embedders of the library.
    pub fn from_parts(paths: Paths, config: Config, store: Arc<dyn Storage>, cwd: PathBuf) -> Self {
        let embedder = crate::embeddings::build(&config.embeddings).unwrap_or(None);
        Self { paths, config, store, embedder, cwd }
    }

    /// The configured embedding provider, if embeddings are enabled.
    pub fn embedder(&self) -> Option<&dyn EmbeddingProvider> {
        self.embedder.as_deref()
    }

    /// Replace the embedding provider (used when a command overrides it).
    pub fn set_embedder(&mut self, embedder: Option<Arc<dyn EmbeddingProvider>>) {
        self.embedder = embedder;
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.config
    }

    pub fn store(&self) -> &dyn Storage {
        self.store.as_ref()
    }

    pub fn store_arc(&self) -> Arc<dyn Storage> {
        Arc::clone(&self.store)
    }

    /// Directory the command was invoked from.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Override the working directory (tests, `--path`).
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }

    /// Resolve a project the way every command should: an explicit `--project`
    /// name or slug first, otherwise the project attached to the current
    /// directory.
    pub fn resolve_project(&self, requested: Option<&str>) -> Result<Option<Project>> {
        match requested {
            Some(ident) => self.lookup_project(ident).map(Some),
            None => self.store.find_project_by_path(&self.cwd),
        }
    }

    /// Like [`App::resolve_project`] but fails when there is nothing to work on.
    pub fn require_project(&self, requested: Option<&str>) -> Result<Project> {
        self.resolve_project(requested)?.ok_or_else(|| Error::NoProjectHere(self.cwd.clone()))
    }

    /// Find a project by slug, exact name, or unique case-insensitive prefix.
    pub fn lookup_project(&self, ident: &str) -> Result<Project> {
        let ident = ident.trim();
        if let Some(project) = self.store.find_project_by_slug(ident)? {
            return Ok(project);
        }
        if let Some(project) = self.store.find_project_by_slug(&crate::util::ids::slugify(ident))? {
            return Ok(project);
        }
        let all = self.store.list_projects(true)?;
        let matches: Vec<Project> = all
            .into_iter()
            .filter(|p| {
                p.id == ident
                    || p.name.eq_ignore_ascii_case(ident)
                    || p.slug.starts_with(&ident.to_lowercase())
            })
            .collect();
        match matches.len() {
            1 => Ok(matches.into_iter().next().expect("length checked")),
            0 => Err(Error::ProjectNotFound(ident.to_string())),
            n => Err(Error::Ambiguous { ident: ident.to_string(), count: n }),
        }
    }

    /// Scope for retrieval: the given project plus global memories, or global
    /// only when no project is in play.
    pub fn scope_for(&self, project: Option<&Project>) -> ProjectScope {
        match project {
            Some(p) => ProjectScope::ProjectWithGlobal(p.id.clone()),
            None => ProjectScope::Any,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::Project;
    use crate::util::time;

    fn app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let app = App::open_or_create(Paths::with_root(dir.path().join("home"))).unwrap();
        (dir, app)
    }

    fn project(name: &str, root: Option<&Path>) -> Project {
        Project {
            id: crate::util::ids::new_id(),
            name: name.into(),
            slug: crate::util::ids::slugify(name),
            root_path: root.map(Path::to_path_buf),
            description: None,
            git_remote: None,
            default_branch: None,
            created_at: time::now(),
            updated_at: time::now(),
            active: true,
        }
    }

    #[test]
    fn open_requires_init() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().join("missing"));
        assert!(matches!(App::open(paths), Err(Error::NotInitialised)));
    }

    #[test]
    fn open_or_create_initialises() {
        let (_dir, app) = app();
        assert!(app.paths().is_initialised());
        assert!(app.paths().projects_dir().is_dir());
    }

    #[test]
    fn lookup_by_slug_name_and_prefix() {
        let (_dir, app) = app();
        app.store().create_project(&project("FerroGrid", None)).unwrap();
        assert_eq!(app.lookup_project("ferrogrid").unwrap().name, "FerroGrid");
        assert_eq!(app.lookup_project("FerroGrid").unwrap().name, "FerroGrid");
        assert_eq!(app.lookup_project("ferro").unwrap().name, "FerroGrid");
        assert!(matches!(app.lookup_project("nope"), Err(Error::ProjectNotFound(_))));
    }

    #[test]
    fn ambiguous_prefix_is_reported() {
        let (_dir, app) = app();
        app.store().create_project(&project("Ferro One", None)).unwrap();
        app.store().create_project(&project("Ferro Two", None)).unwrap();
        assert!(matches!(app.lookup_project("ferro"), Err(Error::Ambiguous { .. })));
    }

    #[test]
    fn cwd_resolves_the_attached_project() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let app =
            App::open_or_create(Paths::with_root(dir.path().join("home"))).unwrap().with_cwd(&repo);
        app.store().create_project(&project("Repo", Some(&repo))).unwrap();

        assert_eq!(app.require_project(None).unwrap().name, "Repo");
        let outside = app.clone().with_cwd(dir.path().join("elsewhere"));
        assert!(matches!(outside.require_project(None), Err(Error::NoProjectHere(_))));
    }
}
