//! Working sessions: who worked on what, when, and what came of it.
//!
//! A session is one stretch of work on a project by one agent (or by the
//! developer directly). It exists so that the question an agent asks at the
//! start of a session — *what happened last time?* — has a better answer than
//! a flat list of memories: the last session says which agent it was, how long
//! it ran, which checkpoints it produced and what was learned during it.
//!
//! Sessions are local to a machine. They describe activity here, not knowledge,
//! so they deliberately do not travel in `contextd bundle`.

use chrono::{DateTime, Duration, Utc};

use serde::Serialize;

use crate::app::App;
use crate::core::model::{Checkpoint, Decision, Memory, Project, Session, Status};
use crate::error::{Error, Result};
use crate::storage::repository::{MemoryFilter, ProjectScope};
use crate::util::{ids, time};

/// What happened during a session.
#[derive(Debug, Clone, Serialize)]
pub struct SessionActivity {
    pub session: Session,
    pub checkpoints: Vec<Checkpoint>,
    /// Memories recorded while the session was running.
    pub memories: Vec<Memory>,
    /// Decisions taken while the session was running.
    pub decisions: Vec<Decision>,
}

impl SessionActivity {
    /// How long the session ran, or has been running.
    pub fn duration(&self) -> Duration {
        self.session.ended_at.unwrap_or_else(time::now) - self.session.started_at
    }

    /// True while the session is still open.
    pub fn is_open(&self) -> bool {
        self.session.ended_at.is_none()
    }

    /// Whether anything at all was recorded.
    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty() && self.memories.is_empty() && self.decisions.is_empty()
    }

    /// One-line description, used in status output and by MCP.
    pub fn headline(&self) -> String {
        let agent = self.session.agent.clone().unwrap_or_else(|| "cli".into());
        let age = humanize_duration(self.duration());
        if self.is_open() {
            format!("{agent}, open for {age}")
        } else {
            format!("{agent}, {age}")
        }
    }
}

/// Session operations.
pub struct SessionService<'a> {
    app: &'a App,
}

impl<'a> SessionService<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    /// Open a session, closing any that was left running.
    ///
    /// A session left open by a crashed agent would otherwise collect a second
    /// agent's work; closing it here keeps the record honest without needing a
    /// timeout or a daemon.
    pub fn start(
        &self,
        project: &Project,
        agent: Option<&str>,
    ) -> Result<(Session, Option<Session>)> {
        let store = self.app.store();
        let closed = match store.open_session(&project.id)? {
            Some(previous) => {
                store.end_session(&previous.id, None)?;
                store.get_session(&previous.id)?
            }
            None => None,
        };

        let session = Session {
            id: ids::new_id(),
            project_id: project.id.clone(),
            agent: agent.map(|a| a.trim().to_lowercase()).filter(|a| !a.is_empty()),
            started_at: time::now(),
            ended_at: None,
            summary: None,
        };
        store.start_session(&session)?;
        tracing::info!(session = %session.id, agent = ?session.agent, "session started");
        Ok((session, closed))
    }

    /// Close the open session, if there is one.
    pub fn end(&self, project: &Project, summary: Option<&str>) -> Result<Option<Session>> {
        let store = self.app.store();
        let Some(open) = store.open_session(&project.id)? else {
            return Ok(None);
        };
        let summary = summary.map(str::trim).filter(|s| !s.is_empty());
        store.end_session(&open.id, summary)?;
        tracing::info!(session = %open.id, "session ended");
        store.get_session(&open.id)
    }

    /// Record what the open session has achieved, leaving it open.
    ///
    /// An agent that says "here is what I did" mid-connection should not have
    /// its session closed underneath it; the connection closing does that.
    pub fn summarize(&self, project: &Project, summary: &str) -> Result<Option<Session>> {
        let summary = summary.trim();
        if summary.is_empty() {
            return Err(Error::invalid("summary", "must not be empty"));
        }
        let store = self.app.store();
        let Some(open) = store.open_session(&project.id)? else {
            return Ok(None);
        };
        store.summarize_session(&open.id, summary)?;
        store.get_session(&open.id)
    }

    /// The session currently running for this project.
    pub fn current(&self, project: &Project) -> Result<Option<Session>> {
        self.app.store().open_session(&project.id)
    }

    /// The most recent session, running or finished.
    pub fn latest(&self, project: &Project) -> Result<Option<Session>> {
        self.app.store().latest_session(&project.id)
    }

    pub fn history(&self, project: &Project, limit: usize) -> Result<Vec<Session>> {
        self.app.store().list_sessions(&project.id, limit)
    }

    /// Resolve a session by full id or unique prefix within the project.
    pub fn resolve(&self, project: &Project, ident: &str) -> Result<Session> {
        let ident = ident.trim();
        if let Some(session) = self.app.store().get_session(ident)? {
            return Ok(session);
        }
        let matches: Vec<Session> = self
            .history(project, 500)?
            .into_iter()
            .filter(|session| session.id.starts_with(ident))
            .collect();
        match matches.len() {
            1 => Ok(matches.into_iter().next().expect("length checked")),
            0 => Err(Error::invalid("session", format!("no session matches `{ident}`"))),
            n => Err(Error::Ambiguous { ident: ident.to_string(), count: n }),
        }
    }

    /// Everything recorded during a session.
    ///
    /// Checkpoints are linked explicitly; memories and decisions are found by
    /// the time window, which needs no extra column and stays correct for
    /// records written by the CLI while an agent's session was open.
    pub fn activity(&self, session: &Session) -> Result<SessionActivity> {
        let store = self.app.store();
        let until = session.ended_at.unwrap_or_else(time::now);

        let memories = store.list_memories(&MemoryFilter {
            statuses: Status::ALL.to_vec(),
            created_from: Some(session.started_at),
            created_to: Some(until),
            ..MemoryFilter::for_scope(ProjectScope::Project(session.project_id.clone()))
        })?;

        let decisions = store
            .list_decisions(&session.project_id, true)?
            .into_iter()
            .filter(|decision| {
                decision.created_at >= session.started_at && decision.created_at < until
            })
            .collect();

        Ok(SessionActivity {
            checkpoints: store.checkpoints_for_session(&session.id)?,
            memories,
            decisions,
            session: session.clone(),
        })
    }
}

/// "3 hours", "12 minutes" — durations, as opposed to points in time.
pub fn humanize_duration(duration: Duration) -> String {
    let seconds = duration.num_seconds().max(0);
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d {}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// Format a session's window for display.
pub fn window(session: &Session) -> String {
    let started = time::to_storage(&session.started_at);
    match session.ended_at {
        Some(ended) => format!("{started} → {}", time::to_storage(&ended)),
        None => format!("{started} → open"),
    }
}

/// Age of a timestamp, for status lines.
pub fn since(timestamp: &DateTime<Utc>) -> String {
    time::humanize_since(timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::core::checkpoint::{CheckpointService, NewCheckpoint};
    use crate::core::decision::{DecisionService, NewDecision};
    use crate::core::memory::{MemoryService, NewMemory};
    use crate::core::model::Category;
    use crate::core::project::{AttachRequest, ProjectService};

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

    #[test]
    fn start_and_end_a_session() {
        let (_dir, app, project) = fixture();
        let sessions = SessionService::new(&app);

        assert!(sessions.current(&project).unwrap().is_none());
        let (session, closed) = sessions.start(&project, Some("Claude Code")).unwrap();
        assert!(closed.is_none());
        assert_eq!(session.agent.as_deref(), Some("claude code"), "agent names are normalised");
        assert_eq!(sessions.current(&project).unwrap().unwrap().id, session.id);

        let ended = sessions.end(&project, Some("wired up heartbeat")).unwrap().unwrap();
        assert!(ended.ended_at.is_some());
        assert_eq!(ended.summary.as_deref(), Some("wired up heartbeat"));
        assert!(sessions.current(&project).unwrap().is_none());
        assert!(sessions.end(&project, None).unwrap().is_none(), "ending twice is harmless");
    }

    #[test]
    fn starting_closes_a_session_left_open() {
        let (_dir, app, project) = fixture();
        let sessions = SessionService::new(&app);
        let (first, _) = sessions.start(&project, Some("claude")).unwrap();
        let (second, closed) = sessions.start(&project, Some("codex")).unwrap();

        assert_eq!(closed.unwrap().id, first.id);
        assert!(app.store().get_session(&first.id).unwrap().unwrap().ended_at.is_some());
        assert_eq!(sessions.current(&project).unwrap().unwrap().id, second.id);
        assert_eq!(sessions.history(&project, 10).unwrap().len(), 2);
    }

    #[test]
    fn activity_collects_what_happened_inside_the_window() {
        let (_dir, app, project) = fixture();
        let sessions = SessionService::new(&app);

        // Recorded before the session opens: must not be attributed to it.
        // Back-dated explicitly, because a test writes both within the same
        // millisecond and the window boundary is inclusive.
        let earlier = MemoryService::new(&app)
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Knowledge, "earlier note")
            })
            .unwrap();
        let mut earlier = app.store().get_memory(&earlier.id).unwrap().unwrap();
        earlier.created_at = time::now() - Duration::hours(1);
        app.store().update_memory(&earlier).unwrap();

        let (session, _) = sessions.start(&project, Some("claude")).unwrap();
        MemoryService::new(&app)
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Architecture, "Scheduler transport is NATS")
            })
            .unwrap();
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

        let activity = sessions.activity(&session).unwrap();
        assert!(activity.is_open());
        assert!(!activity.is_empty());
        assert_eq!(activity.checkpoints.len(), 1, "checkpoints link to the open session");
        assert_eq!(activity.memories.len(), 1, "only memories from inside the window");
        assert_eq!(activity.decisions.len(), 1);
        assert!(activity.headline().starts_with("claude, open for"));
    }

    #[test]
    fn checkpoints_made_outside_a_session_have_none() {
        let (_dir, app, project) = fixture();
        let checkpoint = CheckpointService::new(&app)
            .create(
                &project,
                NewCheckpoint { summary: "solo work".into(), skip_git: true, ..Default::default() },
            )
            .unwrap();
        assert!(checkpoint.session_id.is_none());
    }

    #[test]
    fn summarise_keeps_the_session_open() {
        let (_dir, app, project) = fixture();
        let sessions = SessionService::new(&app);
        assert!(sessions.summarize(&project, "nothing open").unwrap().is_none());

        sessions.start(&project, Some("claude")).unwrap();
        let summarised = sessions.summarize(&project, "  wired up heartbeat  ").unwrap().unwrap();
        assert_eq!(summarised.summary.as_deref(), Some("wired up heartbeat"));
        assert!(sessions.current(&project).unwrap().is_some());
        assert!(sessions.summarize(&project, "   ").is_err());
    }

    #[test]
    fn resolve_accepts_a_prefix() {
        let (_dir, app, project) = fixture();
        let sessions = SessionService::new(&app);
        let (session, _) = sessions.start(&project, None).unwrap();
        assert_eq!(sessions.resolve(&project, &session.id[..8]).unwrap().id, session.id);
        assert!(sessions.resolve(&project, "zzzzzzzz").is_err());
    }

    #[test]
    fn duration_reads_naturally() {
        assert_eq!(humanize_duration(Duration::seconds(30)), "30s");
        assert_eq!(humanize_duration(Duration::minutes(5)), "5m");
        assert_eq!(humanize_duration(Duration::minutes(150)), "2h 30m");
        assert_eq!(humanize_duration(Duration::hours(30)), "1d 6h");
        assert_eq!(humanize_duration(Duration::seconds(-5)), "0s");
    }
}
