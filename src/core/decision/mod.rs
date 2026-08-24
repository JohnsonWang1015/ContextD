//! Architecture decision records.
//!
//! Decisions are kept separate from memories because they have a lifecycle of
//! their own: one decision explicitly replaces another, and an agent asking
//! "what is the current architecture?" must get the survivor, never the
//! loudest or most repeated option.

use crate::app::App;
use crate::core::model::{Decision, DecisionStatus, Project};
use crate::error::{Error, Result};
use crate::util::{ids, time};

/// Input for [`DecisionService::record`].
#[derive(Debug, Clone)]
pub struct NewDecision {
    pub title: String,
    pub decision: String,
    pub context: Option<String>,
    pub consequences: Option<String>,
    pub alternatives: Vec<String>,
    pub status: DecisionStatus,
    /// Id (or prefix) of the decision this one replaces.
    pub supersedes: Option<String>,
}

impl NewDecision {
    pub fn new(title: impl Into<String>, decision: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            decision: decision.into(),
            context: None,
            consequences: None,
            alternatives: Vec::new(),
            status: DecisionStatus::Accepted,
            supersedes: None,
        }
    }
}

/// Decision operations.
pub struct DecisionService<'a> {
    app: &'a App,
}

impl<'a> DecisionService<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    /// Record a decision, closing out the one it replaces.
    pub fn record(&self, project: &Project, input: NewDecision) -> Result<Decision> {
        let store = self.app.store();
        let supersedes = match input.supersedes {
            Some(ident) => Some(
                store
                    .resolve_decision(&ident)?
                    .ok_or_else(|| {
                        Error::invalid("supersedes", format!("no decision matches `{ident}`"))
                    })?
                    .id,
            ),
            None => None,
        };

        let now = time::now();
        let decision = Decision {
            id: ids::new_id(),
            project_id: project.id.clone(),
            title: input.title.trim().to_string(),
            context: input.context,
            decision: input.decision.trim().to_string(),
            consequences: input.consequences,
            alternatives: input.alternatives,
            status: input.status,
            supersedes,
            superseded_by: None,
            decided_at: now,
            created_at: now,
            updated_at: now,
        };
        store.create_decision(&decision)?;
        Ok(decision)
    }

    /// Decisions that describe the architecture as it stands.
    pub fn current(&self, project: &Project) -> Result<Vec<Decision>> {
        self.app.store().list_decisions(&project.id, false)
    }

    /// Every decision, including replaced ones (the history).
    pub fn all(&self, project: &Project) -> Result<Vec<Decision>> {
        self.app.store().list_decisions(&project.id, true)
    }

    pub fn get(&self, ident: &str) -> Result<Decision> {
        self.app
            .store()
            .resolve_decision(ident)?
            .ok_or_else(|| Error::invalid("decision", format!("no decision matches `{ident}`")))
    }

    pub fn supersede(&self, old_ident: &str, new_ident: &str) -> Result<(Decision, Decision)> {
        let old = self.get(old_ident)?;
        let new = self.get(new_ident)?;
        self.app.store().supersede_decision(&old.id, &new.id)?;
        Ok((self.get(&old.id)?, self.get(&new.id)?))
    }

    pub fn delete(&self, ident: &str) -> Result<Decision> {
        let decision = self.get(ident)?;
        self.app.store().delete_decision(&decision.id)?;
        Ok(decision)
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
    fn history_chain_keeps_one_current_answer() {
        let (_dir, app, project) = setup();
        let service = DecisionService::new(&app);

        let redis = service.record(&project, NewDecision::new("Task queue", "Redis")).unwrap();
        let postgres = service
            .record(
                &project,
                NewDecision {
                    supersedes: Some(redis.id[..8].to_string()),
                    ..NewDecision::new("Task queue", "PostgreSQL LISTEN/NOTIFY")
                },
            )
            .unwrap();
        let nats = service
            .record(
                &project,
                NewDecision {
                    supersedes: Some(postgres.id.clone()),
                    ..NewDecision::new("Task queue", "NATS")
                },
            )
            .unwrap();

        let current = service.current(&project).unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].id, nats.id);
        assert_eq!(service.all(&project).unwrap().len(), 3);
        assert_eq!(
            service.get(&redis.id).unwrap().superseded_by.as_deref(),
            Some(postgres.id.as_str())
        );
    }

    #[test]
    fn superseding_unknown_decision_fails() {
        let (_dir, app, project) = setup();
        let err = DecisionService::new(&app).record(
            &project,
            NewDecision { supersedes: Some("nope".into()), ..NewDecision::new("t", "d") },
        );
        assert!(err.is_err());
    }

    #[test]
    fn supersede_after_the_fact() {
        let (_dir, app, project) = setup();
        let service = DecisionService::new(&app);
        let a = service.record(&project, NewDecision::new("Q", "Redis")).unwrap();
        let b = service.record(&project, NewDecision::new("Q", "NATS")).unwrap();
        let (old, new) = service.supersede(&a.id, &b.id).unwrap();
        assert_eq!(old.status, DecisionStatus::Superseded);
        assert_eq!(new.supersedes.as_deref(), Some(a.id.as_str()));
        assert_eq!(service.current(&project).unwrap().len(), 1);
    }
}
