//! Transferable snapshots of memory.
//!
//! A bundle is the wire format between machines: plain JSON holding projects,
//! memories, decisions and checkpoints. It is deliberately *not* a copy of the
//! SQLite file — two machines that both wrote since the last exchange must
//! both keep their work, which a file copy cannot do.
//!
//! Merging is idempotent because every record carries a UUID: importing the
//! same bundle twice changes nothing the second time. Where the same record
//! exists on both sides, the newer `updated_at` wins and any genuine
//! disagreement is reported rather than silently resolved.
//!
//! Embeddings are not transferred. They are derived data, they may come from a
//! different provider on the other machine, and `contextd refresh` recreates
//! them locally in less time than shipping them would take.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::app::App;
use crate::core::model::{Checkpoint, Decision, Memory, Project, Status};
use crate::error::{Error, Result};
use crate::storage::repository::{MemoryFilter, ProjectScope};
use crate::util::time;

/// Format version. Bumped only for a breaking change; readers refuse newer.
pub const BUNDLE_VERSION: u32 = 1;

/// How many checkpoints per project a bundle carries.
const CHECKPOINT_LIMIT: usize = 500;

/// A snapshot of memory, ready to send to another machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    /// Format version, named so a stray JSON file is recognisable.
    pub contextd_bundle: u32,
    pub generated_at: DateTime<Utc>,
    pub source: BundleSource,
    pub projects: Vec<Project>,
    pub memories: Vec<Memory>,
    pub decisions: Vec<Decision>,
    pub checkpoints: Vec<Checkpoint>,
}

/// Where a bundle came from, for the merge report.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BundleSource {
    pub host: Option<String>,
    pub version: String,
}

/// What to include when building a bundle.
#[derive(Debug, Clone, Default)]
pub struct BundleOptions {
    /// Limit to one project (by id); otherwise everything.
    pub project_id: Option<String>,
    /// Only records changed at or after this instant.
    pub since: Option<DateTime<Utc>>,
    pub include_checkpoints: bool,
    /// Include global (project-less) memories.
    pub include_global: bool,
}

impl BundleOptions {
    /// Everything worth sending: all projects, global memories, checkpoints.
    pub fn everything() -> Self {
        Self { include_checkpoints: true, include_global: true, ..Default::default() }
    }
}

impl Bundle {
    /// Read a bundle, rejecting formats this build does not understand.
    pub fn from_json(text: &str) -> Result<Self> {
        let bundle: Bundle = serde_json::from_str(text)
            .map_err(|err| Error::invalid("bundle", format!("not a ContextD bundle: {err}")))?;
        if bundle.contextd_bundle > BUNDLE_VERSION {
            return Err(Error::invalid(
                "bundle",
                format!(
                    "bundle format v{} is newer than this build supports (v{BUNDLE_VERSION}); \
                     upgrade contextd",
                    bundle.contextd_bundle
                ),
            ));
        }
        Ok(bundle)
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// Total records carried.
    pub fn len(&self) -> usize {
        self.memories.len() + self.decisions.len() + self.checkpoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Collect local memory into a bundle.
pub fn build(app: &App, options: &BundleOptions) -> Result<Bundle> {
    let store = app.store();

    let projects: Vec<Project> = store
        .list_projects(true)?
        .into_iter()
        .filter(|p| options.project_id.as_ref().is_none_or(|id| *id == p.id))
        .collect();

    let scope = match (&options.project_id, options.include_global) {
        (Some(id), true) => ProjectScope::ProjectWithGlobal(id.clone()),
        (Some(id), false) => ProjectScope::Project(id.clone()),
        (None, _) => ProjectScope::Any,
    };

    // Every status travels: the fact that Redis was superseded is exactly the
    // kind of thing the other machine must not rediscover the hard way.
    let memories: Vec<Memory> = store
        .list_memories(&MemoryFilter {
            statuses: Status::ALL.to_vec(),
            ..MemoryFilter::for_scope(scope)
        })?
        .into_iter()
        .filter(|m| changed_since(&m.updated_at, options.since))
        .collect();

    let mut decisions = Vec::new();
    let mut checkpoints = Vec::new();
    for project in &projects {
        decisions.extend(
            store
                .list_decisions(&project.id, true)?
                .into_iter()
                .filter(|d| changed_since(&d.updated_at, options.since)),
        );
        if options.include_checkpoints {
            checkpoints.extend(
                store
                    .list_checkpoints(&project.id, CHECKPOINT_LIMIT)?
                    .into_iter()
                    .filter(|c| changed_since(&c.created_at, options.since)),
            );
        }
    }

    Ok(Bundle {
        contextd_bundle: BUNDLE_VERSION,
        generated_at: time::now(),
        source: BundleSource { host: hostname(), version: crate::VERSION.to_string() },
        projects,
        memories,
        decisions,
        checkpoints,
    })
}

fn changed_since(timestamp: &DateTime<Utc>, since: Option<DateTime<Utc>>) -> bool {
    since.is_none_or(|since| *timestamp >= since)
}

/// Best-effort machine name, used only to label a bundle.
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

/// What a merge did, or would do.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MergeReport {
    pub projects_created: Vec<String>,
    pub projects_matched: Vec<String>,
    pub memories_added: usize,
    pub memories_updated: usize,
    pub memories_unchanged: usize,
    pub decisions_added: usize,
    pub decisions_updated: usize,
    pub checkpoints_added: usize,
    /// Records where both sides changed and the local copy was kept.
    pub conflicts: Vec<Conflict>,
    pub dry_run: bool,
    pub source: Option<String>,
}

impl MergeReport {
    /// Records actually written.
    pub fn written(&self) -> usize {
        self.memories_added
            + self.memories_updated
            + self.decisions_added
            + self.decisions_updated
            + self.checkpoints_added
    }
}

/// A record that differs on both sides with no clear winner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub kind: String,
    pub id: String,
    pub title: String,
    pub detail: String,
}

/// Merge a bundle into the local store.
pub fn merge(app: &App, bundle: &Bundle, dry_run: bool) -> Result<MergeReport> {
    let store = app.store();
    let mut report =
        MergeReport { dry_run, source: bundle.source.host.clone(), ..Default::default() };

    // Remote project ids are not local project ids: the same repository
    // attached on two machines gets a different UUID on each. Map them by
    // git remote first (the strongest identity a repository has), then slug.
    let mut project_map: HashMap<String, String> = HashMap::new();
    let local_projects = store.list_projects(true)?;

    for remote in &bundle.projects {
        if let Some(local) = match_project(&local_projects, remote) {
            project_map.insert(remote.id.clone(), local.id.clone());
            if local.id != remote.id {
                report.projects_matched.push(format!("{} → {}", remote.name, local.name));
            } else {
                report.projects_matched.push(remote.name.clone());
            }
            continue;
        }

        // New here. The remote's root_path belongs to the other machine, so it
        // is dropped: a later `contextd attach` in the local checkout adopts
        // this project instead of creating a second one for the same repo.
        let mut created = remote.clone();
        created.root_path = None;
        created.slug = free_slug(&local_projects, &created.slug);
        project_map.insert(remote.id.clone(), created.id.clone());
        report.projects_created.push(created.name.clone());
        if !dry_run {
            store.create_project(&created)?;
        }
    }

    // Pass one: bodies, with supersede pointers withheld until every record
    // they might point at exists.
    let mut deferred_memories: Vec<Memory> = Vec::new();
    for remote in &bundle.memories {
        let mut incoming = remote.clone();
        // A memory whose project was not in the bundle would violate the
        // foreign key; it becomes a global memory rather than being dropped.
        incoming.project_id =
            remote.project_id.as_ref().and_then(|id| project_map.get(id).cloned());
        let pointer = incoming.superseded_by.take();

        match store.get_memory(&incoming.id)? {
            None => {
                report.memories_added += 1;
                if !dry_run {
                    store.create_memory(&incoming)?;
                }
                if pointer.is_some() {
                    incoming.superseded_by = pointer;
                    deferred_memories.push(incoming);
                }
            }
            Some(local) => match compare(&local.updated_at, &incoming.updated_at) {
                Ordering::RemoteNewer => {
                    report.memories_updated += 1;
                    if !dry_run {
                        store.update_memory(&incoming)?;
                    }
                    if pointer.is_some() {
                        incoming.superseded_by = pointer;
                        deferred_memories.push(incoming);
                    }
                }
                Ordering::Same => report.memories_unchanged += 1,
                Ordering::LocalNewer => {
                    report.memories_unchanged += 1;
                    if differs(&local, remote) {
                        report.conflicts.push(Conflict {
                            kind: "memory".into(),
                            id: local.id.clone(),
                            title: local.title.clone(),
                            detail: "both sides changed; the local copy is newer and was kept"
                                .into(),
                        });
                    }
                }
            },
        }
    }

    // Pass two: supersede pointers, now that their targets are present.
    if !dry_run {
        for mut memory in deferred_memories {
            let target = memory.superseded_by.clone();
            if target.as_ref().is_some_and(|id| store.get_memory(id).ok().flatten().is_some()) {
                store.update_memory(&memory)?;
            } else {
                // The successor did not travel with the bundle; keeping a
                // dangling pointer would break the foreign key.
                memory.superseded_by = None;
                memory.status = Status::Deprecated;
                store.update_memory(&memory)?;
            }
        }
    }

    let mut deferred_decisions: Vec<Decision> = Vec::new();
    for remote in &bundle.decisions {
        let Some(local_project) = project_map.get(&remote.project_id) else {
            continue; // decision for a project that did not travel
        };
        let mut incoming = remote.clone();
        incoming.project_id = local_project.clone();
        let supersedes = incoming.supersedes.take();
        let superseded_by = incoming.superseded_by.take();

        match store.get_decision(&incoming.id)? {
            None => {
                report.decisions_added += 1;
                if !dry_run {
                    store.create_decision(&incoming)?;
                }
            }
            Some(local) => match compare(&local.updated_at, &incoming.updated_at) {
                Ordering::RemoteNewer => {
                    report.decisions_updated += 1;
                    if !dry_run {
                        store.update_decision(&incoming)?;
                    }
                }
                Ordering::Same => {}
                Ordering::LocalNewer => {
                    if local.decision != remote.decision {
                        report.conflicts.push(Conflict {
                            kind: "decision".into(),
                            id: local.id.clone(),
                            title: local.title.clone(),
                            detail: "both sides changed; the local copy is newer and was kept"
                                .into(),
                        });
                    }
                }
            },
        }
        if supersedes.is_some() || superseded_by.is_some() {
            incoming.supersedes = supersedes;
            incoming.superseded_by = superseded_by;
            deferred_decisions.push(incoming);
        }
    }

    if !dry_run {
        for mut decision in deferred_decisions {
            decision.supersedes =
                decision.supersedes.filter(|id| store.get_decision(id).ok().flatten().is_some());
            decision.superseded_by =
                decision.superseded_by.filter(|id| store.get_decision(id).ok().flatten().is_some());
            store.update_decision(&decision)?;
        }
    }

    // Checkpoints are immutable, so presence is the whole question.
    for remote in &bundle.checkpoints {
        let Some(local_project) = project_map.get(&remote.project_id) else {
            continue;
        };
        if store.get_checkpoint(&remote.id)?.is_some() {
            continue;
        }
        report.checkpoints_added += 1;
        if !dry_run {
            let mut incoming = remote.clone();
            incoming.project_id = local_project.clone();
            store.create_checkpoint(&incoming)?;
        }
    }

    Ok(report)
}

/// Which side is newer.
enum Ordering {
    RemoteNewer,
    LocalNewer,
    Same,
}

fn compare(local: &DateTime<Utc>, remote: &DateTime<Utc>) -> Ordering {
    match remote.cmp(local) {
        std::cmp::Ordering::Greater => Ordering::RemoteNewer,
        std::cmp::Ordering::Less => Ordering::LocalNewer,
        std::cmp::Ordering::Equal => Ordering::Same,
    }
}

fn differs(local: &Memory, remote: &Memory) -> bool {
    local.content != remote.content || local.title != remote.title || local.status != remote.status
}

/// Match a bundled project against what is already here.
fn match_project<'a>(local: &'a [Project], remote: &Project) -> Option<&'a Project> {
    local
        .iter()
        .find(|p| p.id == remote.id)
        .or_else(|| {
            remote.git_remote.as_ref().filter(|r| !r.trim().is_empty()).and_then(|remote_url| {
                local.iter().find(|p| {
                    p.git_remote.as_deref().is_some_and(|local_url| {
                        normalise_remote(local_url) == normalise_remote(remote_url)
                    })
                })
            })
        })
        .or_else(|| local.iter().find(|p| p.slug == remote.slug))
}

/// `git@github.com:acme/FerroGrid.git` and
/// `https://github.com/acme/FerroGrid` name the same repository.
fn normalise_remote(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let without_scheme = trimmed.split_once("://").map(|(_, rest)| rest).unwrap_or(trimmed);
    let without_user =
        without_scheme.split_once('@').map(|(_, rest)| rest).unwrap_or(without_scheme);
    without_user.replace(':', "/").to_lowercase()
}

/// Derive a slug that no local project is using.
fn free_slug(local: &[Project], desired: &str) -> String {
    if !local.iter().any(|p| p.slug == desired) {
        return desired.to_string();
    }
    (2..1000)
        .map(|n| format!("{desired}-{n}"))
        .find(|candidate| !local.iter().any(|p| &p.slug == candidate))
        .unwrap_or_else(crate::util::ids::new_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::core::memory::{MemoryService, NewMemory};
    use crate::core::model::Category;
    use crate::core::project::{AttachRequest, ProjectService};
    use crate::storage::repository::MemoryFilter;

    struct Machine {
        _dir: tempfile::TempDir,
        app: App,
        project: Project,
    }

    fn machine(name: &str, git_remote: Option<&str>) -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let app =
            App::open_or_create(Paths::with_root(dir.path().join("home"))).unwrap().with_cwd(&repo);
        let (mut project, _) = ProjectService::new(&app)
            .attach(AttachRequest {
                dir: repo,
                name: Some(name.to_string()),
                description: None,
                bindings: vec![],
            })
            .unwrap();
        if let Some(remote) = git_remote {
            project.git_remote = Some(remote.to_string());
            app.store().update_project(&project).unwrap();
        }
        Machine { _dir: dir, app, project }
    }

    fn add(machine: &Machine, content: &str) -> Memory {
        MemoryService::new(&machine.app)
            .add(NewMemory {
                project: Some(machine.project.clone()),
                ..NewMemory::new(Category::Architecture, content)
            })
            .unwrap()
    }

    fn all_memories(app: &App) -> Vec<Memory> {
        app.store()
            .list_memories(&MemoryFilter { statuses: Status::ALL.to_vec(), ..Default::default() })
            .unwrap()
    }

    #[test]
    fn bundle_roundtrips_through_json() {
        let laptop = machine("FerroGrid", None);
        add(&laptop, "Scheduler transport is NATS");
        let bundle = build(&laptop.app, &BundleOptions::everything()).unwrap();
        let parsed = Bundle::from_json(&bundle.to_json().unwrap()).unwrap();
        assert_eq!(parsed.memories.len(), 1);
        assert_eq!(parsed.contextd_bundle, BUNDLE_VERSION);
        assert!(!parsed.is_empty());
    }

    #[test]
    fn a_newer_bundle_format_is_refused() {
        let text = r#"{"contextd_bundle":99,"generated_at":"2026-01-01T00:00:00Z",
                       "source":{"host":null,"version":"9"},"projects":[],"memories":[],
                       "decisions":[],"checkpoints":[]}"#;
        let err = Bundle::from_json(text).unwrap_err().to_string();
        assert!(err.contains("newer than this build"), "{err}");
        assert!(Bundle::from_json("{}").is_err());
    }

    #[test]
    fn merging_is_idempotent() {
        let laptop = machine("FerroGrid", Some("git@github.com:acme/FerroGrid.git"));
        add(&laptop, "Scheduler transport is NATS");
        add(&laptop, "Workers renew GPU leases every 30s");
        let bundle = build(&laptop.app, &BundleOptions::everything()).unwrap();

        let desktop = machine("FerroGrid", Some("https://github.com/acme/FerroGrid"));
        let first = merge(&desktop.app, &bundle, false).unwrap();
        assert_eq!(first.memories_added, 2);
        // Matched by git remote despite different URL forms and project ids.
        assert!(first.projects_created.is_empty(), "{:?}", first.projects_created);
        assert_eq!(desktop.app.store().list_projects(true).unwrap().len(), 1);

        let second = merge(&desktop.app, &bundle, false).unwrap();
        assert_eq!(second.memories_added, 0);
        assert_eq!(second.memories_unchanged, 2);
        assert_eq!(all_memories(&desktop.app).len(), 2);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let laptop = machine("FerroGrid", None);
        add(&laptop, "Scheduler transport is NATS");
        let bundle = build(&laptop.app, &BundleOptions::everything()).unwrap();

        let desktop = machine("Other", None);
        let report = merge(&desktop.app, &bundle, true).unwrap();
        assert_eq!(report.memories_added, 1);
        assert!(report.dry_run);
        assert!(all_memories(&desktop.app).is_empty());
    }

    #[test]
    fn history_travels_and_stays_history() {
        let laptop = machine("FerroGrid", Some("git@github.com:acme/FerroGrid.git"));
        let redis = add(&laptop, "Task queue uses Redis");
        MemoryService::new(&laptop.app)
            .add(NewMemory {
                project: Some(laptop.project.clone()),
                supersedes: Some(redis.id.clone()),
                ..NewMemory::new(Category::Architecture, "Task queue transport is NATS")
            })
            .unwrap();

        let bundle = build(&laptop.app, &BundleOptions::everything()).unwrap();
        let desktop = machine("FerroGrid", Some("git@github.com:acme/FerroGrid.git"));
        merge(&desktop.app, &bundle, false).unwrap();

        let merged = desktop.app.store().get_memory(&redis.id).unwrap().unwrap();
        assert_eq!(merged.status, Status::Superseded);
        assert!(merged.superseded_by.is_some(), "the supersede link must survive the trip");

        // Retrieval on the receiving machine sees one current answer.
        let current = desktop.app.store().list_memories(&MemoryFilter::default()).unwrap();
        assert_eq!(current.len(), 1);
        assert!(current[0].content.contains("NATS"));
    }

    #[test]
    fn the_newer_side_wins_and_real_divergence_is_reported() {
        let laptop = machine("FerroGrid", Some("git@github.com:acme/FerroGrid.git"));
        let memory = add(&laptop, "Heartbeat interval is 10s");

        let desktop = machine("FerroGrid", Some("git@github.com:acme/FerroGrid.git"));
        merge(&desktop.app, &build(&laptop.app, &BundleOptions::everything()).unwrap(), false)
            .unwrap();

        // The remote edits the memory afterwards.
        let mut edited = laptop.app.store().get_memory(&memory.id).unwrap().unwrap();
        edited.content = "Heartbeat interval is 5s".into();
        edited.updated_at = time::now() + chrono::Duration::seconds(30);
        laptop.app.store().update_memory(&edited).unwrap();

        let report =
            merge(&desktop.app, &build(&laptop.app, &BundleOptions::everything()).unwrap(), false)
                .unwrap();
        assert_eq!(report.memories_updated, 1);
        assert!(desktop
            .app
            .store()
            .get_memory(&memory.id)
            .unwrap()
            .unwrap()
            .content
            .contains("5s"));

        // Now the local side is the newer one and diverges: keep local, report.
        let mut local = desktop.app.store().get_memory(&memory.id).unwrap().unwrap();
        local.content = "Heartbeat interval is 2s (measured here)".into();
        local.updated_at = time::now() + chrono::Duration::seconds(120);
        desktop.app.store().update_memory(&local).unwrap();

        let report =
            merge(&desktop.app, &build(&laptop.app, &BundleOptions::everything()).unwrap(), false)
                .unwrap();
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(report.conflicts[0].kind, "memory");
        assert!(desktop
            .app
            .store()
            .get_memory(&memory.id)
            .unwrap()
            .unwrap()
            .content
            .contains("2s"));
    }

    #[test]
    fn an_unknown_project_arrives_without_a_local_path() {
        let laptop = machine("FerroGrid", None);
        add(&laptop, "Scheduler transport is NATS");
        let bundle = build(&laptop.app, &BundleOptions::everything()).unwrap();

        let desktop = machine("Unrelated", None);
        let report = merge(&desktop.app, &bundle, false).unwrap();
        assert_eq!(report.projects_created, vec!["FerroGrid".to_string()]);

        let created = desktop.app.lookup_project("FerroGrid").unwrap();
        assert!(created.root_path.is_none(), "a remote path must not be claimed locally");
        assert_eq!(all_memories(&desktop.app).len(), 1);
    }

    #[test]
    fn since_filters_to_recent_changes() {
        let laptop = machine("FerroGrid", None);
        add(&laptop, "Old memory");
        let cutoff = time::now() + chrono::Duration::seconds(1);
        let bundle = build(
            &laptop.app,
            &BundleOptions { since: Some(cutoff), ..BundleOptions::everything() },
        )
        .unwrap();
        assert!(bundle.memories.is_empty());
    }

    #[test]
    fn remote_url_forms_normalise_to_the_same_repository() {
        assert_eq!(
            normalise_remote("git@github.com:acme/FerroGrid.git"),
            normalise_remote("https://github.com/acme/FerroGrid")
        );
        assert_ne!(
            normalise_remote("git@github.com:acme/one"),
            normalise_remote("git@github.com:acme/two")
        );
    }

    #[test]
    fn colliding_slugs_get_a_free_one() {
        let laptop = machine("FerroGrid", Some("git@github.com:acme/one.git"));
        add(&laptop, "memory from machine one");
        let bundle = build(&laptop.app, &BundleOptions::everything()).unwrap();

        // Same name, different repository: two distinct projects.
        let desktop = machine("FerroGrid", Some("git@github.com:acme/two.git"));
        let report = merge(&desktop.app, &bundle, false).unwrap();
        assert!(report.projects_created.is_empty(), "slug match keeps them together");

        // A project whose slug is taken but which is genuinely new arrives
        // under a free slug.
        let mut third = bundle.clone();
        third.projects[0].id = crate::util::ids::new_id();
        third.projects[0].git_remote = Some("git@github.com:acme/three.git".into());
        third.projects[0].slug = "ferrogrid".into();
        let _ = merge(&desktop.app, &third, false);
        let slugs: Vec<String> =
            desktop.app.store().list_projects(true).unwrap().into_iter().map(|p| p.slug).collect();
        assert_eq!(slugs.len(), slugs.iter().collect::<std::collections::HashSet<_>>().len());
    }
}
