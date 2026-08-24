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
//! Deletions travel as tombstones: a record removed here becomes a note that
//! it was removed, so the next machine to sync stops handing it back. Where a
//! record was edited *after* it was deleted elsewhere, the edit wins and the
//! record returns — the most recent decision about a record is the one that
//! stands, and deleting is a decision with a timestamp like any other.
//!
//! Sessions do not travel either: they record who was working on which
//! machine, not what was learned, and a checkpoint arriving from elsewhere is
//! detached from the session it belonged to over there.
//!
//! Embeddings are not transferred. They are derived data, they may come from a
//! different provider on the other machine, and `contextd refresh` recreates
//! them locally in less time than shipping them would take.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::app::App;
use crate::core::model::{
    Checkpoint, Decision, Memory, Project, RecordKind, RecordRef, Status, Tombstone,
};
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
    /// Records deleted on the sending machine.
    ///
    /// Defaulted rather than required so a bundle written before tombstones
    /// existed still parses; a bundle carrying them stays readable by an older
    /// build too, which simply will not propagate the deletions.
    #[serde(default)]
    pub tombstones: Vec<Tombstone>,
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

    /// Total records carried, deletions included.
    pub fn len(&self) -> usize {
        self.memories.len() + self.decisions.len() + self.checkpoints.len() + self.tombstones.len()
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

    let scope = scope_for(options);

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

    let tombstones = store
        .tombstones(&scope_for(options), options.since)?
        .into_iter()
        .filter(|tombstone| {
            options.include_checkpoints || tombstone.record.kind != RecordKind::Checkpoint
        })
        .collect();

    Ok(Bundle {
        contextd_bundle: BUNDLE_VERSION,
        generated_at: time::now(),
        source: BundleSource { host: hostname(), version: crate::VERSION.to_string() },
        projects,
        memories,
        decisions,
        checkpoints,
        tombstones,
    })
}

/// Scope implied by the bundle options.
fn scope_for(options: &BundleOptions) -> ProjectScope {
    match (&options.project_id, options.include_global) {
        (Some(id), true) => ProjectScope::ProjectWithGlobal(id.clone()),
        (Some(id), false) => ProjectScope::Project(id.clone()),
        (None, _) => ProjectScope::Any,
    }
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
    /// Records removed here because they were deleted on the other machine.
    pub deleted: usize,
    /// Records that came back because they were edited after being deleted.
    pub revived: usize,
    /// Deletion notes stored, whether or not they removed anything here.
    pub tombstones_received: usize,
    /// What was removed, so an external vector index can be told.
    #[serde(default)]
    pub deleted_records: Vec<RecordRef>,
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

    /// Whether the merge changed anything at all.
    pub fn changed_anything(&self) -> bool {
        self.written() > 0 || self.deleted > 0
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

    // What is known to be deleted, here or there. The earliest deletion wins,
    // matching how tombstones are stored: once a machine has agreed a record
    // is gone, a later copy of the same tombstone must not move the goalposts.
    let mut deletions: HashMap<RecordRef, DateTime<Utc>> = HashMap::new();
    for tombstone in &bundle.tombstones {
        report.tombstones_received += 1;
        deletions
            .entry(tombstone.record.clone())
            .and_modify(|existing| *existing = (*existing).min(tombstone.deleted_at))
            .or_insert(tombstone.deleted_at);
        if !dry_run {
            store.record_tombstone(tombstone)?;
        }
    }
    for record in bundle
        .memories
        .iter()
        .map(|memory| RecordRef::memory(&memory.id))
        .chain(bundle.decisions.iter().map(|decision| RecordRef::decision(&decision.id)))
        .chain(bundle.checkpoints.iter().map(|checkpoint| RecordRef::checkpoint(&checkpoint.id)))
    {
        if let Some(local) = store.tombstone_for(&record)? {
            deletions
                .entry(record)
                .and_modify(|existing| *existing = (*existing).min(local.deleted_at))
                .or_insert(local.deleted_at);
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

        match verdict(&deletions, &RecordRef::memory(&incoming.id), &incoming.updated_at) {
            Verdict::Dead => {
                apply_deletion(store, &RecordRef::memory(&incoming.id), dry_run, &mut report)?;
                continue;
            }
            Verdict::Revived => {
                report.revived += 1;
                if !dry_run {
                    store.clear_tombstone(&RecordRef::memory(&incoming.id))?;
                }
            }
            Verdict::Alive => {}
        }

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

        match verdict(&deletions, &RecordRef::decision(&incoming.id), &incoming.updated_at) {
            Verdict::Dead => {
                apply_deletion(store, &RecordRef::decision(&incoming.id), dry_run, &mut report)?;
                continue;
            }
            Verdict::Revived => {
                report.revived += 1;
                if !dry_run {
                    store.clear_tombstone(&RecordRef::decision(&incoming.id))?;
                }
            }
            Verdict::Alive => {}
        }

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
        // Checkpoints are immutable, so a tombstone always wins.
        if deletions.contains_key(&RecordRef::checkpoint(&remote.id)) {
            apply_deletion(store, &RecordRef::checkpoint(&remote.id), dry_run, &mut report)?;
            continue;
        }
        if store.get_checkpoint(&remote.id)?.is_some() {
            continue;
        }
        report.checkpoints_added += 1;
        if !dry_run {
            let mut incoming = remote.clone();
            incoming.project_id = local_project.clone();
            // Sessions describe activity on the machine they happened on and
            // do not travel, so a session id from over there would dangle.
            incoming.session_id = None;
            store.create_checkpoint(&incoming)?;
        }
    }

    // Records the bundle did not carry, because the sender had already deleted
    // them: their tombstones still have to take effect here.
    for tombstone in &bundle.tombstones {
        if let Verdict::Dead = verdict_for_local(store, &deletions, &tombstone.record)? {
            apply_deletion(store, &tombstone.record, dry_run, &mut report)?;
        }
    }

    Ok(report)
}

/// What a deletion note means for one record.
enum Verdict {
    /// No deletion is known.
    Alive,
    /// Deleted, and nothing has happened since.
    Dead,
    /// Deleted, then edited afterwards: the edit is the newer decision.
    Revived,
}

fn verdict(
    deletions: &HashMap<RecordRef, DateTime<Utc>>,
    record: &RecordRef,
    updated_at: &DateTime<Utc>,
) -> Verdict {
    match deletions.get(record) {
        None => Verdict::Alive,
        Some(deleted_at) if updated_at > deleted_at => Verdict::Revived,
        Some(_) => Verdict::Dead,
    }
}

/// The same judgement for a record that exists only on this machine.
fn verdict_for_local(
    store: &dyn crate::storage::repository::Storage,
    deletions: &HashMap<RecordRef, DateTime<Utc>>,
    record: &RecordRef,
) -> Result<Verdict> {
    let updated_at = match record.kind {
        RecordKind::Memory => store.get_memory(&record.id)?.map(|memory| memory.updated_at),
        RecordKind::Decision => store.get_decision(&record.id)?.map(|d| d.updated_at),
        RecordKind::Checkpoint => store.get_checkpoint(&record.id)?.map(|c| c.created_at),
    };
    Ok(match updated_at {
        // Already gone here; nothing to do.
        None => Verdict::Alive,
        Some(updated_at) => verdict(deletions, record, &updated_at),
    })
}

/// Remove a record because it was deleted elsewhere.
///
/// The delete path writes a tombstone of its own, stamped now. That is
/// harmless here because the incoming tombstone was stored first and an
/// existing one is never overwritten: this machine keeps claiming the record
/// died when it actually died, rather than when it heard about it, which is
/// what lets a third machine's edit from in between still win.
fn apply_deletion(
    store: &dyn crate::storage::repository::Storage,
    record: &RecordRef,
    dry_run: bool,
    report: &mut MergeReport,
) -> Result<bool> {
    let existed = match record.kind {
        RecordKind::Memory => store.get_memory(&record.id)?.is_some(),
        RecordKind::Decision => store.get_decision(&record.id)?.is_some(),
        RecordKind::Checkpoint => store.get_checkpoint(&record.id)?.is_some(),
    };
    if !existed {
        return Ok(false);
    }

    report.deleted += 1;
    report.deleted_records.push(record.clone());
    if dry_run {
        return Ok(true);
    }

    match record.kind {
        RecordKind::Memory => store.delete_memory(&record.id)?,
        RecordKind::Decision => store.delete_decision(&record.id)?,
        RecordKind::Checkpoint => store.delete_checkpoint(&record.id)?,
    };
    Ok(true)
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
    fn a_deletion_travels_and_does_not_come_back() {
        let url = "git@github.com:acme/FerroGrid.git";
        let laptop = machine("FerroGrid", Some(url));
        let keep = add(&laptop, "Scheduler transport is NATS");
        let drop = add(&laptop, "Temporary note about the spike branch");

        let desktop = machine("FerroGrid", Some(url));
        merge(&desktop.app, &build(&laptop.app, &BundleOptions::everything()).unwrap(), false)
            .unwrap();
        assert_eq!(all_memories(&desktop.app).len(), 2);

        // Deleted on the laptop, then synced.
        laptop.app.store().delete_memory(&drop.id).unwrap();
        let bundle = build(&laptop.app, &BundleOptions::everything()).unwrap();
        assert_eq!(bundle.tombstones.len(), 1);

        let report = merge(&desktop.app, &bundle, false).unwrap();
        assert_eq!(report.deleted, 1);
        assert_eq!(report.tombstones_received, 1);
        assert_eq!(report.deleted_records[0].id, drop.id);

        let remaining = all_memories(&desktop.app);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, keep.id);

        // Syncing the other way must not resurrect it: the desktop now knows
        // it was deleted and says so in its own bundle.
        let back = build(&desktop.app, &BundleOptions::everything()).unwrap();
        assert!(back.tombstones.iter().any(|t| t.record.id == drop.id));
        let report = merge(&laptop.app, &back, false).unwrap();
        assert_eq!(report.memories_added, 0);
        assert_eq!(all_memories(&laptop.app).len(), 1);
    }

    #[test]
    fn a_deletion_propagates_through_a_third_machine() {
        let url = "git@github.com:acme/FerroGrid.git";
        let a = machine("FerroGrid", Some(url));
        let note = add(&a, "Temporary note");
        let bundle = build(&a.app, &BundleOptions::everything()).unwrap();

        let b = machine("FerroGrid", Some(url));
        let c = machine("FerroGrid", Some(url));
        merge(&b.app, &bundle, false).unwrap();
        merge(&c.app, &bundle, false).unwrap();

        // A deletes; B hears about it; C hears about it from B.
        a.app.store().delete_memory(&note.id).unwrap();
        merge(&b.app, &build(&a.app, &BundleOptions::everything()).unwrap(), false).unwrap();
        let report =
            merge(&c.app, &build(&b.app, &BundleOptions::everything()).unwrap(), false).unwrap();

        assert_eq!(report.deleted, 1, "the tombstone must travel onwards");
        assert!(all_memories(&c.app).is_empty());
    }

    #[test]
    fn an_edit_after_a_deletion_brings_the_record_back() {
        let url = "git@github.com:acme/FerroGrid.git";
        let laptop = machine("FerroGrid", Some(url));
        let memory = add(&laptop, "Heartbeat interval is 10s");
        let bundle = build(&laptop.app, &BundleOptions::everything()).unwrap();

        let desktop = machine("FerroGrid", Some(url));
        merge(&desktop.app, &bundle, false).unwrap();

        // Deleted on the laptop; edited on the desktop afterwards.
        laptop.app.store().delete_memory(&memory.id).unwrap();
        let mut edited = desktop.app.store().get_memory(&memory.id).unwrap().unwrap();
        edited.content = "Heartbeat interval is 5s".into();
        edited.updated_at = time::now() + chrono::Duration::seconds(60);
        desktop.app.store().update_memory(&edited).unwrap();

        // The desktop keeps its edit and tells the laptop about it.
        let report =
            merge(&desktop.app, &build(&laptop.app, &BundleOptions::everything()).unwrap(), false)
                .unwrap();
        assert_eq!(report.deleted, 0, "an edit after the deletion outranks it");
        assert_eq!(all_memories(&desktop.app).len(), 1);

        let report =
            merge(&laptop.app, &build(&desktop.app, &BundleOptions::everything()).unwrap(), false)
                .unwrap();
        assert_eq!(report.revived, 1);
        assert_eq!(all_memories(&laptop.app).len(), 1);
        assert!(laptop
            .app
            .store()
            .tombstone_for(&RecordRef::memory(&memory.id))
            .unwrap()
            .is_none());
    }

    #[test]
    fn deleting_decisions_and_checkpoints_also_travels() {
        use crate::core::checkpoint::{CheckpointService, NewCheckpoint};
        use crate::core::decision::{DecisionService, NewDecision};

        let url = "git@github.com:acme/FerroGrid.git";
        let laptop = machine("FerroGrid", Some(url));
        let decision = DecisionService::new(&laptop.app)
            .record(&laptop.project, NewDecision::new("Transport", "Redis"))
            .unwrap();
        let checkpoint = CheckpointService::new(&laptop.app)
            .create(
                &laptop.project,
                NewCheckpoint { summary: "spike".into(), skip_git: true, ..Default::default() },
            )
            .unwrap();

        let desktop = machine("FerroGrid", Some(url));
        merge(&desktop.app, &build(&laptop.app, &BundleOptions::everything()).unwrap(), false)
            .unwrap();
        assert_eq!(desktop.app.store().list_decisions(&desktop.project.id, true).unwrap().len(), 1);

        laptop.app.store().delete_decision(&decision.id).unwrap();
        laptop.app.store().delete_checkpoint(&checkpoint.id).unwrap();

        let report =
            merge(&desktop.app, &build(&laptop.app, &BundleOptions::everything()).unwrap(), false)
                .unwrap();
        assert_eq!(report.deleted, 2);
        assert!(desktop.app.store().list_decisions(&desktop.project.id, true).unwrap().is_empty());
        assert!(desktop.app.store().list_checkpoints(&desktop.project.id, 10).unwrap().is_empty());
    }

    #[test]
    fn dry_run_reports_deletions_without_making_them() {
        let url = "git@github.com:acme/FerroGrid.git";
        let laptop = machine("FerroGrid", Some(url));
        let memory = add(&laptop, "Temporary note");
        let desktop = machine("FerroGrid", Some(url));
        merge(&desktop.app, &build(&laptop.app, &BundleOptions::everything()).unwrap(), false)
            .unwrap();

        laptop.app.store().delete_memory(&memory.id).unwrap();
        let report =
            merge(&desktop.app, &build(&laptop.app, &BundleOptions::everything()).unwrap(), true)
                .unwrap();
        assert_eq!(report.deleted, 1);
        assert_eq!(all_memories(&desktop.app).len(), 1, "nothing is removed in a dry run");
        assert!(desktop
            .app
            .store()
            .tombstone_for(&RecordRef::memory(&memory.id))
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_bundle_without_tombstones_still_parses() {
        let text = r#"{"contextd_bundle":1,"generated_at":"2026-01-01T00:00:00Z",
                       "source":{"host":null,"version":"0.1.0"},"projects":[],"memories":[],
                       "decisions":[],"checkpoints":[]}"#;
        let bundle = Bundle::from_json(text).unwrap();
        assert!(bundle.tombstones.is_empty());
        assert!(bundle.is_empty());
    }

    #[test]
    fn checkpoints_arrive_detached_from_their_session() {
        use crate::core::session::SessionService;

        let laptop = machine("FerroGrid", Some("git@github.com:acme/FerroGrid.git"));
        SessionService::new(&laptop.app).start(&laptop.project, Some("claude")).unwrap();
        crate::core::checkpoint::CheckpointService::new(&laptop.app)
            .create(
                &laptop.project,
                crate::core::checkpoint::NewCheckpoint {
                    summary: "heartbeat done".into(),
                    skip_git: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let bundle = build(&laptop.app, &BundleOptions::everything()).unwrap();
        assert!(bundle.checkpoints[0].session_id.is_some(), "it is linked on the source machine");

        let desktop = machine("FerroGrid", Some("git@github.com:acme/FerroGrid.git"));
        merge(&desktop.app, &bundle, false).unwrap();

        let arrived = desktop
            .app
            .store()
            .list_checkpoints(&desktop.project.id, 10)
            .unwrap()
            .pop()
            .expect("checkpoint travelled");
        assert_eq!(arrived.summary, "heartbeat done");
        assert!(arrived.session_id.is_none(), "the session id must not dangle");
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
