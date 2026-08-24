//! Rendering a [`ContextBundle`] as Markdown.
//!
//! Agent adapters wrap these sections in whatever preamble their tool expects,
//! so the substance of the context is written once. Everything here is plain
//! CommonMark — readable in a terminal, in an editor and to a model.

use crate::core::context::{ContextBundle, SelectedMemory};
use crate::core::model::{Category, Checkpoint};

/// Which sections to render.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub include_ids: bool,
    pub include_superseded: bool,
    pub include_scores: bool,
    /// Heading level for top-level sections (2 = `##`).
    pub heading_level: usize,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            include_ids: false,
            include_superseded: true,
            include_scores: false,
            heading_level: 2,
        }
    }
}

/// Full context document.
pub fn markdown(bundle: &ContextBundle, options: &RenderOptions) -> String {
    let h = "#".repeat(options.heading_level.clamp(1, 5));
    let mut out = String::new();

    if let Some(project) = &bundle.project {
        out.push_str(&format!("{h} Project: {}\n\n", project.name));
        if let Some(description) = &project.description {
            out.push_str(description.trim());
            out.push_str("\n\n");
        }
        if let Some(root) = &project.root_path {
            out.push_str(&format!("- Repository: `{}`\n", root.display()));
        }
        if let Some(branch) = &project.default_branch {
            out.push_str(&format!("- Branch: `{branch}`\n"));
        }
        out.push('\n');
    }

    if !bundle.decisions.is_empty() {
        out.push_str(&format!("{h} Current architecture\n\n"));
        for decision in &bundle.decisions {
            out.push_str(&format!("- **{}** — {}", decision.title, one_line(&decision.decision)));
            if options.include_ids {
                out.push_str(&format!(" `{}`", crate::util::ids::short(&decision.id)));
            }
            out.push('\n');
            if let Some(consequences) =
                decision.consequences.as_ref().filter(|c| !c.trim().is_empty())
            {
                out.push_str(&format!("  - Consequences: {}\n", one_line(consequences)));
            }
        }
        out.push('\n');
    }

    let grouped = group_by_category(&bundle.memories);
    for (category, memories) in grouped {
        out.push_str(&format!("{h} {}\n\n", section_title(category)));
        for memory in memories {
            out.push_str(&render_memory(memory, options));
        }
        out.push('\n');
    }

    if let Some(checkpoint) = &bundle.checkpoint {
        out.push_str(&render_checkpoint(checkpoint, &h));
    }

    if options.include_superseded && !bundle.superseded.is_empty() {
        out.push_str(&format!("{h} Superseded (do not reintroduce)\n\n"));
        for note in &bundle.superseded {
            match &note.replaced_by {
                Some(replacement) => out.push_str(&format!(
                    "- ~~{}~~ → now: {}\n",
                    one_line(&note.title),
                    one_line(replacement)
                )),
                None => {
                    out.push_str(&format!("- ~~{}~~ (no longer current)\n", one_line(&note.title)))
                }
            }
        }
        out.push('\n');
    }

    out.trim_end().to_string() + "\n"
}

/// The `contextd resume` view: what an agent needs to pick the work back up.
pub fn resume(bundle: &ContextBundle) -> String {
    let mut out = String::new();
    let name = bundle.project.as_ref().map(|p| p.name.as_str()).unwrap_or("work");
    out.push_str(&format!("Resume {name}.\n\n"));

    match &bundle.checkpoint {
        Some(checkpoint) => {
            if let Some(goal) = &checkpoint.current_goal {
                out.push_str(&format!("Current goal:\n{goal}\n\n"));
            }
            if let Some(state) = &checkpoint.current_state {
                out.push_str(&format!("Current state:\n{state}\n\n"));
            }
            out.push_str(&format!("Last checkpoint:\n{}", checkpoint.summary));
            out.push_str(&format!(
                " ({})\n\n",
                crate::util::time::humanize_since(&checkpoint.created_at)
            ));
            push_list(&mut out, "Completed", &checkpoint.completed);
            push_list(&mut out, "Next", &checkpoint.next_steps);
            push_list(&mut out, "Open issues", &checkpoint.open_problems);
            push_list(&mut out, "Related files", &checkpoint.related_files);

            if let Some(branch) = &checkpoint.git_branch {
                out.push_str(&format!("Branch: {branch}"));
                if let Some(commit) = &checkpoint.git_commit {
                    out.push_str(&format!(" @ {}", crate::util::git::short_commit(commit)));
                }
                out.push('\n');
                if !checkpoint.dirty_files.is_empty() {
                    out.push_str(&format!(
                        "Uncommitted: {} file(s)\n",
                        checkpoint.dirty_files.len()
                    ));
                }
                out.push('\n');
            }
        }
        None => {
            out.push_str("No checkpoint yet — run `contextd checkpoint \"...\"` to record one.\n\n")
        }
    }

    if !bundle.decisions.is_empty() {
        out.push_str("Current architecture:\n");
        for decision in &bundle.decisions {
            out.push_str(&format!("- {} = {}\n", decision.title, one_line(&decision.decision)));
        }
        out.push('\n');
    }

    if !bundle.memories.is_empty() {
        out.push_str("Relevant memory:\n");
        for memory in &bundle.memories {
            out.push_str(&format!("- {}\n", one_line(&memory.hit.title)));
        }
        out.push('\n');
    }

    if !bundle.superseded.is_empty() {
        out.push_str("Superseded:\n");
        for note in &bundle.superseded {
            out.push_str(&format!("- {}\n", one_line(&note.title)));
        }
        out.push('\n');
    }

    out.trim_end().to_string() + "\n"
}

fn render_checkpoint(checkpoint: &Checkpoint, heading: &str) -> String {
    let mut out = format!("{heading} Current state\n\n");
    if let Some(goal) = &checkpoint.current_goal {
        out.push_str(&format!("**Goal:** {goal}\n\n"));
    }
    out.push_str(&format!(
        "Last checkpoint: {} ({})\n\n",
        checkpoint.summary,
        crate::util::time::humanize_since(&checkpoint.created_at)
    ));
    push_bullets(&mut out, "Completed", &checkpoint.completed);
    push_bullets(&mut out, "Next", &checkpoint.next_steps);
    push_bullets(&mut out, "Open problems", &checkpoint.open_problems);
    out
}

fn render_memory(memory: &SelectedMemory, options: &RenderOptions) -> String {
    let mut line = format!("- **{}**", memory.hit.title.trim());
    if options.include_ids {
        line.push_str(&format!(" `{}`", crate::util::ids::short(&memory.hit.id)));
    }
    if options.include_scores {
        line.push_str(&format!(" _(score {:.2}, {})_", memory.hit.score, memory.reason));
    }
    line.push('\n');

    let body = memory.hit.content.trim();
    // Only repeat the body when it says more than the title does.
    if !body.is_empty() && body != memory.hit.title.trim() {
        for body_line in body.lines() {
            line.push_str(&format!("  {}\n", body_line.trim_end()));
        }
    }
    line
}

fn group_by_category(memories: &[SelectedMemory]) -> Vec<(Category, Vec<&SelectedMemory>)> {
    // A fixed order keeps exported files stable across runs, which matters
    // when they are committed to git.
    const ORDER: [Category; 9] = [
        Category::User,
        Category::Convention,
        Category::Project,
        Category::Architecture,
        Category::Decision,
        Category::Knowledge,
        Category::Task,
        Category::Feedback,
        Category::Reference,
    ];
    let mut grouped = Vec::new();
    for category in ORDER {
        let items: Vec<&SelectedMemory> =
            memories.iter().filter(|m| m.hit.category == Some(category)).collect();
        if !items.is_empty() {
            grouped.push((category, items));
        }
    }
    grouped
}

fn section_title(category: Category) -> &'static str {
    match category {
        Category::User => "Developer preferences",
        Category::Project => "Project context",
        Category::Architecture => "Architecture",
        Category::Decision => "Decisions",
        Category::Task => "Current tasks",
        Category::Feedback => "Feedback to honour",
        Category::Convention => "Coding conventions",
        Category::Knowledge => "Technical knowledge",
        Category::Reference => "References",
    }
}

fn push_list(out: &mut String, label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("{label}:\n"));
    for item in items {
        out.push_str(&format!("- {item}\n"));
    }
    out.push('\n');
}

fn push_bullets(out: &mut String, label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("**{label}:**\n\n"));
    for item in items {
        out.push_str(&format!("- {item}\n"));
    }
    out.push('\n');
}

fn one_line(text: &str) -> String {
    crate::util::text::one_line(text, 240)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::context::{BudgetReport, SupersededNote};
    use crate::core::model::{Decision, DecisionStatus, Project, RecordKind, Status};
    use crate::search::SearchHit;
    use crate::util::time;

    fn bundle() -> ContextBundle {
        let now = time::now();
        let project = Project {
            id: "p1".into(),
            name: "FerroGrid".into(),
            slug: "ferrogrid".into(),
            root_path: Some(std::path::PathBuf::from("/repo")),
            description: Some("Distributed GPU scheduling".into()),
            git_remote: None,
            default_branch: Some("main".into()),
            created_at: now,
            updated_at: now,
            active: true,
        };
        let mut checkpoint = Checkpoint::new("p1", "worker heartbeat completed");
        checkpoint.current_goal = Some("Implement distributed GPU scheduling".into());
        checkpoint.completed = vec!["Coordinator".into(), "Worker registration".into()];
        checkpoint.next_steps = vec!["Lease-based GPU allocation".into()];
        checkpoint.open_problems = vec!["Worker reconnect".into()];
        checkpoint.git_branch = Some("main".into());
        checkpoint.git_commit = Some("abc1234567".into());

        ContextBundle {
            project: Some(project),
            query: None,
            checkpoint: Some(checkpoint),
            decisions: vec![Decision {
                id: "d1".into(),
                project_id: "p1".into(),
                title: "Task queue".into(),
                context: None,
                decision: "NATS".into(),
                consequences: Some("Requires a NATS cluster in every environment".into()),
                alternatives: vec![],
                status: DecisionStatus::Accepted,
                supersedes: None,
                superseded_by: None,
                decided_at: now,
                created_at: now,
                updated_at: now,
            }],
            superseded: vec![SupersededNote {
                title: "Task queue is Redis".into(),
                replaced_by: Some("Task queue is NATS".into()),
                kind: RecordKind::Memory,
            }],
            memories: vec![SelectedMemory {
                hit: SearchHit {
                    kind: RecordKind::Memory,
                    id: "m1234567890".into(),
                    project_id: Some("p1".into()),
                    project_name: Some("FerroGrid".into()),
                    title: "Worker heartbeat timeout is handled in WorkerManager".into(),
                    content: "Worker heartbeat timeout is handled in WorkerManager.\n\
                              Related: src/scheduler/worker_manager.rs"
                        .into(),
                    category: Some(Category::Architecture),
                    status: Status::Active,
                    priority: 4,
                    updated_at: now,
                    superseded_by: None,
                    score: 1.25,
                    breakdown: Default::default(),
                },
                reason: "relevant",
                tokens: 30,
            }],
            budget: BudgetReport { max_tokens: 6000, used_tokens: 120, dropped: 2 },
        }
    }

    #[test]
    fn markdown_has_the_expected_sections() {
        let text = markdown(&bundle(), &RenderOptions::default());
        assert!(text.contains("## Project: FerroGrid"));
        assert!(text.contains("## Current architecture"));
        assert!(text.contains("**Task queue** — NATS"));
        assert!(text.contains("## Architecture"));
        assert!(text.contains("WorkerManager"));
        assert!(text.contains("## Current state"));
        assert!(text.contains("## Superseded (do not reintroduce)"));
        assert!(text.contains("→ now: Task queue is NATS"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn options_control_ids_and_history() {
        let options = RenderOptions {
            include_ids: true,
            include_superseded: false,
            include_scores: true,
            ..Default::default()
        };
        let text = markdown(&bundle(), &options);
        assert!(text.contains("`m1234567`"));
        assert!(text.contains("score 1.25"));
        assert!(!text.contains("Superseded"));
    }

    #[test]
    fn heading_level_is_configurable() {
        let options = RenderOptions { heading_level: 3, ..Default::default() };
        assert!(markdown(&bundle(), &options).contains("### Project: FerroGrid"));
    }

    #[test]
    fn resume_reads_like_a_handover() {
        let text = resume(&bundle());
        assert!(text.starts_with("Resume FerroGrid."));
        assert!(text.contains("Current goal:\nImplement distributed GPU scheduling"));
        assert!(text.contains("Completed:\n- Coordinator"));
        assert!(text.contains("Next:\n- Lease-based GPU allocation"));
        assert!(text.contains("Open issues:\n- Worker reconnect"));
        assert!(text.contains("Branch: main @ abc1234"));
    }

    #[test]
    fn resume_without_a_checkpoint_explains_what_to_do() {
        let mut b = bundle();
        b.checkpoint = None;
        assert!(resume(&b).contains("No checkpoint yet"));
    }

    #[test]
    fn empty_bundle_renders_without_panicking() {
        let empty = ContextBundle {
            project: None,
            query: None,
            checkpoint: None,
            decisions: vec![],
            superseded: vec![],
            memories: vec![],
            budget: BudgetReport::default(),
        };
        assert_eq!(markdown(&empty, &RenderOptions::default()), "\n");
        assert!(resume(&empty).contains("Resume work."));
    }
}
