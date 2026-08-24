//! The human-readable Markdown mirror under `~/.contextd/`.
//!
//! ```text
//! ~/.contextd/
//! ├── projects/FerroGrid/{overview,architecture,decisions,tasks}.md
//! │                     └── checkpoints/2026-08-24T12-00-00Z.md
//! └── global/{coding,git,preferences}.md
//! ```
//!
//! The mirror is a *projection*: SQLite stays authoritative, and these files
//! exist so a developer can read their memory in an editor, diff it, and
//! commit it alongside code. Files ContextD generated are refreshed in place;
//! a file that was edited by hand is reported rather than overwritten, and
//! `adopt` turns those edits back into memories.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::app::App;
use crate::core::memory::{MemoryService, NewMemory};
use crate::core::model::{Category, Checkpoint, Decision, Memory, Project, Source, Status};
use crate::error::Result;
use crate::storage::repository::{MemoryFilter, MemoryOrder, ProjectScope};
use crate::sync::{is_conflict, read_or_empty, write_atomic, FileOutcome, WriteStatus};
use crate::util::hash::content_hash;
use crate::util::time;

/// Sidecar recording the hash of each generated file, so hand edits are
/// detectable without keeping the whole file in the database.
const MANIFEST: &str = ".contextd-manifest.json";

/// Summary of one mirror pass.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MirrorReport {
    pub files: Vec<FileOutcome>,
}

impl MirrorReport {
    pub fn count(&self, status: WriteStatus) -> usize {
        self.files.iter().filter(|f| f.status == status).count()
    }

    pub fn conflicts(&self) -> Vec<&FileOutcome> {
        self.files.iter().filter(|f| f.status == WriteStatus::Conflict).collect()
    }
}

/// Writes and reads the Markdown mirror.
pub struct Mirror<'a> {
    app: &'a App,
}

impl<'a> Mirror<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    /// Refresh the mirror for one project plus the global files.
    pub fn export_project(&self, project: &Project, force: bool) -> Result<MirrorReport> {
        let dir = self.app.paths().project_dir(&project.name);
        let mut manifest = Manifest::load(&dir)?;
        let mut report = MirrorReport::default();

        let store = self.app.store();
        let scope = ProjectScope::Project(project.id.clone());
        let memories = store.list_memories(&MemoryFilter {
            statuses: Status::ALL.to_vec(),
            order: MemoryOrder::PriorityFirst,
            ..MemoryFilter::for_scope(scope)
        })?;
        let decisions = store.list_decisions(&project.id, true)?;
        let checkpoints = store.list_checkpoints(&project.id, 50)?;

        report.files.push(self.write(
            &dir.join("overview.md"),
            &render_overview(project, &memories),
            &mut manifest,
            force,
        )?);
        report.files.push(self.write(
            &dir.join("architecture.md"),
            &render_memories(
                "Architecture",
                &memories,
                &[Category::Architecture, Category::Project],
            ),
            &mut manifest,
            force,
        )?);
        report.files.push(self.write(
            &dir.join("decisions.md"),
            &render_decisions(&decisions),
            &mut manifest,
            force,
        )?);
        report.files.push(self.write(
            &dir.join("tasks.md"),
            &render_memories("Tasks", &memories, &[Category::Task, Category::Feedback]),
            &mut manifest,
            force,
        )?);

        let checkpoint_dir = self.app.paths().project_checkpoints_dir(&project.name);
        for checkpoint in &checkpoints {
            let path = checkpoint_dir.join(checkpoint_file_name(checkpoint));
            report.files.push(self.write(
                &path,
                &render_checkpoint(checkpoint),
                &mut manifest,
                force,
            )?);
        }

        manifest.save(&dir)?;
        report.files.extend(self.export_global(force)?.files);
        Ok(report)
    }

    /// Refresh the global (cross-project) files.
    pub fn export_global(&self, force: bool) -> Result<MirrorReport> {
        let dir = self.app.paths().global_dir();
        let mut manifest = Manifest::load(&dir)?;
        let mut report = MirrorReport::default();

        let memories = self.app.store().list_memories(&MemoryFilter {
            statuses: Status::ALL.to_vec(),
            order: MemoryOrder::PriorityFirst,
            ..MemoryFilter::for_scope(ProjectScope::GlobalOnly)
        })?;

        report.files.push(self.write(
            &dir.join("coding.md"),
            &render_memories("Coding conventions", &memories, &[Category::Convention]),
            &mut manifest,
            force,
        )?);
        report.files.push(self.write(
            &dir.join("preferences.md"),
            &render_memories("Developer preferences", &memories, &[Category::User]),
            &mut manifest,
            force,
        )?);
        // Git habits are a tagged subset rather than a category of their own.
        let git_memories: Vec<Memory> = memories
            .iter()
            .filter(|m| m.tags.iter().any(|t| t == "git" || t == "vcs"))
            .cloned()
            .collect();
        report.files.push(self.write(
            &dir.join("git.md"),
            &render_memories("Git workflow", &git_memories, &[]),
            &mut manifest,
            force,
        )?);

        manifest.save(&dir)?;
        Ok(report)
    }

    /// Turn hand edits in the mirror back into memories.
    ///
    /// This is the only path by which the mirror writes to the database, and
    /// it is explicit: an edited file becomes new memories, leaving the
    /// originals untouched, because ContextD cannot know which of two
    /// divergent versions the developer meant.
    pub fn adopt(&self, project: Option<&Project>) -> Result<Vec<String>> {
        let dir = match project {
            Some(project) => self.app.paths().project_dir(&project.name),
            None => self.app.paths().global_dir(),
        };
        let manifest = Manifest::load(&dir)?;
        let memories = MemoryService::new(self.app);
        let mut adopted = Vec::new();

        for (name, recorded) in manifest.entries() {
            let path = dir.join(name);
            let current = read_or_empty(&path)?;
            if current.trim().is_empty() || content_hash(&current) == *recorded {
                continue;
            }
            for section in crate::agents::markdown::sections(&current) {
                if section.body.trim().is_empty() {
                    continue;
                }
                let memory = memories.add(NewMemory {
                    project: project.cloned(),
                    title: Some(section.heading.clone()).filter(|h| !h.trim().is_empty()),
                    source: Source::Import {
                        agent: "markdown".into(),
                        path: Some(path.to_string_lossy().into_owned()),
                    },
                    ..NewMemory::new(section.category, section.body)
                })?;
                adopted.push(memory.title);
            }
        }
        Ok(adopted)
    }

    /// Write one generated file, respecting hand edits.
    fn write(
        &self,
        path: &Path,
        content: &str,
        manifest: &mut Manifest,
        force: bool,
    ) -> Result<FileOutcome> {
        let key = file_key(path);
        let existing = read_or_empty(path)?;
        let recorded = manifest.get(&key);

        let existing_opt = (!existing.trim().is_empty()).then_some(existing.as_str());
        if !force && is_conflict(recorded, existing_opt) {
            return Ok(FileOutcome::new(path, WriteStatus::Conflict)
                .with_detail("edited outside ContextD; run `contextd sync --adopt` or --force"));
        }

        let status = if existing.trim().is_empty() {
            WriteStatus::Created
        } else if content_hash(&existing) == content_hash(content) {
            WriteStatus::Unchanged
        } else {
            WriteStatus::Updated
        };

        if status != WriteStatus::Unchanged {
            write_atomic(path, content)?;
        }
        let hash = content_hash(content);
        manifest.set(key, hash.clone());
        Ok(FileOutcome::new(path, status).with_hash(hash))
    }
}

/// Hashes of the files ContextD generated, stored next to them.
#[derive(Debug, Default)]
struct Manifest {
    entries: std::collections::BTreeMap<String, String>,
}

impl Manifest {
    fn load(dir: &Path) -> Result<Self> {
        let text = read_or_empty(&dir.join(MANIFEST))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        // A corrupt manifest means "we do not know what we wrote", which the
        // conflict rules already handle conservatively.
        Ok(Self { entries: serde_json::from_str(&text).unwrap_or_default() })
    }

    fn save(&self, dir: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(&self.entries)?;
        write_atomic(&dir.join(MANIFEST), &format!("{text}\n"))
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    fn set(&mut self, key: String, hash: String) {
        self.entries.insert(key, hash);
    }

    fn entries(&self) -> impl Iterator<Item = (&String, &String)> {
        self.entries.iter()
    }
}

/// Manifest key: path relative to the manifest's directory.
fn file_key(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match path.parent().and_then(|p| p.file_name()) {
        Some(parent) if parent == "checkpoints" => format!("checkpoints/{name}"),
        _ => name,
    }
}

fn checkpoint_file_name(checkpoint: &Checkpoint) -> String {
    // Colons are illegal in Windows file names, so the timestamp is flattened.
    let stamp = time::to_storage(&checkpoint.created_at).replace(':', "-");
    format!("{stamp}-{}.md", crate::util::ids::short(&checkpoint.id))
}

fn render_overview(project: &Project, memories: &[Memory]) -> String {
    let mut out = format!("# {}\n\n", project.name);
    if let Some(description) = &project.description {
        out.push_str(&format!("{}\n\n", description.trim()));
    }
    out.push_str("| Field | Value |\n|---|---|\n");
    out.push_str(&format!("| Slug | `{}` |\n", project.slug));
    if let Some(root) = &project.root_path {
        out.push_str(&format!("| Repository | `{}` |\n", root.display()));
    }
    if let Some(remote) = &project.git_remote {
        out.push_str(&format!("| Remote | `{remote}` |\n"));
    }
    if let Some(branch) = &project.default_branch {
        out.push_str(&format!("| Branch | `{branch}` |\n"));
    }
    out.push_str(&format!("| Memories | {} |\n", memories.len()));
    out.push_str(&format!("| Updated | {} |\n\n", time::to_storage(&project.updated_at)));
    out.push_str(
        "_Generated by ContextD. SQLite is the source of truth; \
                  edit here only if you intend to run `contextd sync --adopt`._\n",
    );
    out
}

fn render_memories(title: &str, memories: &[Memory], categories: &[Category]) -> String {
    let mut out = format!("# {title}\n\n");
    let selected: Vec<&Memory> = memories
        .iter()
        .filter(|m| categories.is_empty() || categories.contains(&m.category))
        .collect();

    if selected.is_empty() {
        out.push_str("_Nothing recorded yet._\n");
        return out;
    }

    for memory in selected.iter().filter(|m| m.status.is_current()) {
        out.push_str(&render_memory(memory));
    }

    let history: Vec<&&Memory> = selected.iter().filter(|m| !m.status.is_current()).collect();
    if !history.is_empty() {
        out.push_str("\n## History\n\n");
        for memory in history {
            out.push_str(&format!(
                "- ~~{}~~ ({}) `{}`\n",
                memory.title.trim(),
                memory.status,
                crate::util::ids::short(&memory.id)
            ));
        }
    }
    out
}

fn render_memory(memory: &Memory) -> String {
    let mut out = format!("## {}\n\n", memory.title.trim());
    out.push_str(&format!("{}\n\n", memory.content.trim()));
    let mut meta = vec![
        format!("category: {}", memory.category),
        format!("priority: {}", memory.priority),
        format!("id: {}", crate::util::ids::short(&memory.id)),
    ];
    if !memory.tags.is_empty() {
        meta.push(format!("tags: {}", memory.tags.join(", ")));
    }
    if !memory.files.is_empty() {
        meta.push(format!("files: {}", memory.files.join(", ")));
    }
    if let Some(commit) = &memory.commit {
        meta.push(format!("commit: {}", crate::util::git::short_commit(commit)));
    }
    out.push_str(&format!("<sub>{}</sub>\n\n", meta.join(" · ")));
    out
}

fn render_decisions(decisions: &[Decision]) -> String {
    let mut out = String::from("# Decisions\n\n");
    if decisions.is_empty() {
        out.push_str("_No decisions recorded yet._\n");
        return out;
    }
    for decision in decisions.iter().filter(|d| d.status.is_current()) {
        out.push_str(&render_decision(decision));
    }
    let history: Vec<&Decision> = decisions.iter().filter(|d| !d.status.is_current()).collect();
    if !history.is_empty() {
        out.push_str("## Superseded\n\n");
        for decision in history {
            out.push_str(&format!(
                "- ~~{}: {}~~ ({})\n",
                decision.title.trim(),
                decision.decision.trim(),
                decision.status
            ));
        }
        out.push('\n');
    }
    out
}

fn render_decision(decision: &Decision) -> String {
    let mut out = format!("## {}\n\n", decision.title.trim());
    out.push_str(&format!("**Decision:** {}\n\n", decision.decision.trim()));
    if let Some(context) = decision.context.as_ref().filter(|c| !c.trim().is_empty()) {
        out.push_str(&format!("**Context:** {}\n\n", context.trim()));
    }
    if let Some(consequences) = decision.consequences.as_ref().filter(|c| !c.trim().is_empty()) {
        out.push_str(&format!("**Consequences:** {}\n\n", consequences.trim()));
    }
    if !decision.alternatives.is_empty() {
        out.push_str("**Alternatives considered:**\n\n");
        for alternative in &decision.alternatives {
            out.push_str(&format!("- {alternative}\n"));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "<sub>status: {} · decided: {} · id: {}</sub>\n\n",
        decision.status,
        time::to_storage(&decision.decided_at),
        crate::util::ids::short(&decision.id)
    ));
    out
}

fn render_checkpoint(checkpoint: &Checkpoint) -> String {
    let mut out = format!("# {}\n\n", checkpoint.summary.trim());
    out.push_str(&format!("_{}_\n\n", time::to_storage(&checkpoint.created_at)));
    if let Some(goal) = &checkpoint.current_goal {
        out.push_str(&format!("**Goal:** {goal}\n\n"));
    }
    if let Some(state) = &checkpoint.current_state {
        out.push_str(&format!("**State:** {state}\n\n"));
    }
    for (label, items) in [
        ("Completed", &checkpoint.completed),
        ("Next steps", &checkpoint.next_steps),
        ("Open problems", &checkpoint.open_problems),
        ("Related files", &checkpoint.related_files),
    ] {
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("**{label}:**\n\n"));
        for item in items {
            out.push_str(&format!("- {item}\n"));
        }
        out.push('\n');
    }
    if let Some(branch) = &checkpoint.git_branch {
        out.push_str(&format!("<sub>branch: {branch}"));
        if let Some(commit) = &checkpoint.git_commit {
            out.push_str(&format!(" · commit: {}", crate::util::git::short_commit(commit)));
        }
        out.push_str("</sub>\n");
    }
    out
}

/// Path a project's mirror lives at.
pub fn project_dir(app: &App, project: &Project) -> PathBuf {
    app.paths().project_dir(&project.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::core::checkpoint::{CheckpointService, NewCheckpoint};
    use crate::core::decision::{DecisionService, NewDecision};
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
                description: Some("Distributed GPU scheduling".into()),
                bindings: vec![],
            })
            .unwrap();
        (dir, app, project)
    }

    #[test]
    fn export_writes_the_expected_layout() {
        let (_dir, app, project) = fixture();
        MemoryService::new(&app)
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Architecture, "Scheduler transport is NATS")
            })
            .unwrap();
        DecisionService::new(&app)
            .record(&project, NewDecision::new("Task queue", "NATS"))
            .unwrap();
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

        let report = Mirror::new(&app).export_project(&project, false).unwrap();
        assert!(report.conflicts().is_empty());

        let dir = app.paths().project_dir("FerroGrid");
        assert!(dir.join("overview.md").is_file());
        assert!(std::fs::read_to_string(dir.join("architecture.md")).unwrap().contains("NATS"));
        assert!(std::fs::read_to_string(dir.join("decisions.md")).unwrap().contains("Task queue"));
        assert!(dir.join("tasks.md").is_file());
        let checkpoints: Vec<_> = std::fs::read_dir(dir.join("checkpoints")).unwrap().collect();
        assert_eq!(checkpoints.len(), 1);
        assert!(app.paths().global_dir().join("coding.md").is_file());
    }

    #[test]
    fn second_export_is_unchanged() {
        let (_dir, app, project) = fixture();
        let mirror = Mirror::new(&app);
        mirror.export_project(&project, false).unwrap();
        let report = mirror.export_project(&project, false).unwrap();
        assert_eq!(report.count(WriteStatus::Updated), 0);
        assert!(report.count(WriteStatus::Unchanged) > 0);
    }

    #[test]
    fn hand_edits_are_reported_not_overwritten() {
        let (_dir, app, project) = fixture();
        let mirror = Mirror::new(&app);
        mirror.export_project(&project, false).unwrap();

        let path = app.paths().project_dir("FerroGrid").join("architecture.md");
        std::fs::write(&path, "# Architecture\n\n## Hand written\n\nWorkers pull leases.\n")
            .unwrap();

        let report = mirror.export_project(&project, false).unwrap();
        assert_eq!(report.conflicts().len(), 1);
        assert!(std::fs::read_to_string(&path).unwrap().contains("Hand written"));

        // --force wins.
        let forced = mirror.export_project(&project, true).unwrap();
        assert!(forced.conflicts().is_empty());
        assert!(!std::fs::read_to_string(&path).unwrap().contains("Hand written"));
    }

    #[test]
    fn adopt_imports_hand_edits_as_memories() {
        let (_dir, app, project) = fixture();
        let mirror = Mirror::new(&app);
        mirror.export_project(&project, false).unwrap();

        let path = app.paths().project_dir("FerroGrid").join("architecture.md");
        std::fs::write(
            &path,
            "# Architecture\n\n## Worker leases\n\nWorkers pull leases from the coordinator.\n",
        )
        .unwrap();

        let adopted = mirror.adopt(Some(&project)).unwrap();
        assert!(adopted.iter().any(|t| t == "Worker leases"));
        let memories = MemoryService::new(&app).for_project(Some(&project), 10).unwrap();
        assert!(memories.iter().any(|m| m.content.contains("pull leases")));
    }

    #[test]
    fn superseded_memories_appear_under_history() {
        let (_dir, app, project) = fixture();
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

        Mirror::new(&app).export_project(&project, false).unwrap();
        let text =
            std::fs::read_to_string(app.paths().project_dir("FerroGrid").join("architecture.md"))
                .unwrap();
        assert!(text.contains("## History"));
        assert!(text.contains("~~Task queue is Redis~~"));
    }

    #[test]
    fn checkpoint_file_names_are_windows_safe() {
        let checkpoint = Checkpoint::new("p", "x");
        let name = checkpoint_file_name(&checkpoint);
        assert!(!name.contains(':'));
        assert!(name.ends_with(".md"));
    }
}
