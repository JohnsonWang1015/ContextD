//! Checkpoints: "where was I, and what comes next?"

use std::path::Path;

use crate::app::App;
use crate::core::model::{Checkpoint, Project};
use crate::error::{Error, Result};
use crate::util::git::GitSnapshot;

/// Input for [`CheckpointService::create`].
#[derive(Debug, Clone, Default)]
pub struct NewCheckpoint {
    pub summary: String,
    pub current_goal: Option<String>,
    pub completed: Vec<String>,
    pub current_state: Option<String>,
    pub next_steps: Vec<String>,
    pub open_problems: Vec<String>,
    pub related_files: Vec<String>,
    /// Skip git inspection (useful in tests and non-repo directories).
    pub skip_git: bool,
}

/// Checkpoint operations.
pub struct CheckpointService<'a> {
    app: &'a App,
}

impl<'a> CheckpointService<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    /// Save a checkpoint, capturing git state from the project root.
    ///
    /// Fields the caller left empty are carried over from the previous
    /// checkpoint: in practice a developer types `contextd checkpoint "did X"`
    /// and expects the goal and open problems to persist until they say
    /// otherwise, rather than being silently dropped.
    pub fn create(&self, project: &Project, input: NewCheckpoint) -> Result<Checkpoint> {
        if input.summary.trim().is_empty() {
            return Err(Error::invalid("summary", "must not be empty"));
        }
        let store = self.app.store();
        let previous = store.latest_checkpoint(&project.id)?;

        let git = if input.skip_git {
            GitSnapshot::default()
        } else {
            let root: &Path = project.root_path.as_deref().unwrap_or_else(|| self.app.cwd());
            GitSnapshot::capture(root)
        };

        let mut checkpoint = Checkpoint::new(&project.id, input.summary.trim());
        checkpoint.current_goal =
            input.current_goal.or_else(|| previous.as_ref().and_then(|p| p.current_goal.clone()));
        checkpoint.current_state = input.current_state;
        checkpoint.completed = input.completed;
        checkpoint.next_steps = if input.next_steps.is_empty() {
            previous.as_ref().map(|p| p.next_steps.clone()).unwrap_or_default()
        } else {
            input.next_steps
        };
        checkpoint.open_problems = if input.open_problems.is_empty() {
            previous.as_ref().map(|p| p.open_problems.clone()).unwrap_or_default()
        } else {
            input.open_problems
        };
        checkpoint.related_files = input.related_files;
        checkpoint.git_branch = git.branch.clone();
        checkpoint.git_commit = git.commit.clone();
        checkpoint.dirty_files = git.dirty_files.clone();

        store.create_checkpoint(&checkpoint)?;
        Ok(checkpoint)
    }

    pub fn latest(&self, project: &Project) -> Result<Option<Checkpoint>> {
        self.app.store().latest_checkpoint(&project.id)
    }

    pub fn history(&self, project: &Project, limit: usize) -> Result<Vec<Checkpoint>> {
        self.app.store().list_checkpoints(&project.id, limit)
    }

    pub fn delete(&self, id: &str) -> Result<bool> {
        self.app.store().delete_checkpoint(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::core::project::{AttachRequest, ProjectService};

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
    fn goal_and_open_problems_carry_forward() {
        let (_dir, app, project) = setup();
        let service = CheckpointService::new(&app);
        service
            .create(
                &project,
                NewCheckpoint {
                    summary: "coordinator done".into(),
                    current_goal: Some("Implement distributed GPU scheduling".into()),
                    open_problems: vec!["Worker reconnect".into()],
                    next_steps: vec!["Heartbeat".into()],
                    skip_git: true,
                    ..Default::default()
                },
            )
            .unwrap();

        let second = service
            .create(
                &project,
                NewCheckpoint {
                    summary: "heartbeat done".into(),
                    completed: vec!["Heartbeat".into()],
                    skip_git: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(second.current_goal.as_deref(), Some("Implement distributed GPU scheduling"));
        assert_eq!(second.open_problems, vec!["Worker reconnect".to_string()]);
        assert_eq!(second.completed, vec!["Heartbeat".to_string()]);
        assert_eq!(service.latest(&project).unwrap().unwrap().summary, "heartbeat done");
        assert_eq!(service.history(&project, 10).unwrap().len(), 2);
    }

    #[test]
    fn explicit_values_override_carry_forward() {
        let (_dir, app, project) = setup();
        let service = CheckpointService::new(&app);
        service
            .create(
                &project,
                NewCheckpoint {
                    summary: "one".into(),
                    open_problems: vec!["old".into()],
                    skip_git: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let next = service
            .create(
                &project,
                NewCheckpoint {
                    summary: "two".into(),
                    open_problems: vec!["new".into()],
                    skip_git: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(next.open_problems, vec!["new".to_string()]);
    }

    #[test]
    fn empty_summary_is_rejected() {
        let (_dir, app, project) = setup();
        let err = CheckpointService::new(&app)
            .create(&project, NewCheckpoint { summary: " ".into(), ..Default::default() });
        assert!(err.is_err());
    }

    #[test]
    fn git_metadata_is_captured_when_available() {
        let (_dir, app, project) = setup();
        let root = project.root_path.clone().unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return; // git is not installed in this environment
        }
        std::fs::write(root.join("file.txt"), "hello").unwrap();
        let checkpoint = CheckpointService::new(&app)
            .create(&project, NewCheckpoint { summary: "wip".into(), ..Default::default() })
            .unwrap();
        assert!(checkpoint.git_branch.is_some());
        assert!(checkpoint.dirty_files.iter().any(|f| f.contains("file.txt")));
    }
}
