//! `refresh` and `sync`.

use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::{output, GlobalArgs};
use crate::core::refresh::{RefreshOptions, RefreshService};
use crate::error::Result;
use crate::sync::agent_sync::export_bound_agents;
use crate::sync::mirror::Mirror;
use crate::sync::WriteStatus;
use crate::ui;
use crate::util::ids;

/// `contextd refresh`
#[derive(Debug, Args)]
pub struct RefreshArgs {
    /// Report what would change without changing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Re-embed every record, not only changed ones.
    #[arg(long)]
    pub force_embeddings: bool,

    /// Skip embeddings (useful offline with a remote provider).
    #[arg(long)]
    pub skip_embeddings: bool,

    /// Refresh global memories as well as the project's.
    #[arg(long)]
    pub global: bool,

    /// Similarity above which two memories count as duplicates (0.0–1.0).
    #[arg(long, value_name = "N")]
    pub duplicate_threshold: Option<f64>,
}

/// `contextd sync`
#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Overwrite files that were edited outside ContextD.
    #[arg(long)]
    pub force: bool,

    /// Import hand edits from the Markdown mirror as new memories.
    #[arg(long)]
    pub adopt: bool,

    /// Only write the Markdown mirror, not agent files.
    #[arg(long)]
    pub mirror_only: bool,
}

/// Tidy memory and rebuild indexes.
pub async fn refresh(app: &App, global: &GlobalArgs, args: &RefreshArgs) -> Result<()> {
    let project = app.resolve_project(global.project.as_deref())?;
    let mut options = RefreshOptions::from_config(app);
    options.dry_run = args.dry_run;
    options.force_embeddings = args.force_embeddings;
    options.skip_embeddings = args.skip_embeddings;
    if let Some(threshold) = args.duplicate_threshold {
        if !(0.0..=1.0).contains(&threshold) {
            return Err(crate::error::Error::invalid(
                "duplicate-threshold",
                "must be within 0.0..=1.0",
            ));
        }
        options.duplicate_threshold = threshold;
        options.similar_threshold = options.similar_threshold.min(threshold);
    }

    let service = RefreshService::new(app)?;
    let mut report = service.run(project.as_ref(), &options).await?;
    if args.global && project.is_some() {
        let global_report = service.run(None, &options).await?;
        report.scanned += global_report.scanned;
        report.merged.extend(global_report.merged);
        report.similar.extend(global_report.similar);
        report.embedded += global_report.embedded;
        report.notes.extend(global_report.notes);
    }

    output::render(global, &report, || {
        let mut text = format!("{}\n", ui::header("Refresh"));
        text.push_str(&ui::kv(&[
            ("Scanned", report.scanned.to_string()),
            ("Merged", report.merged.len().to_string()),
            ("Similar", report.similar.len().to_string()),
            ("Indexed", report.fts_records.to_string()),
            ("Embedded", report.embedded.to_string()),
        ]));

        if !report.merged.is_empty() {
            text.push_str(&format!("\n\n{}\n", ui::dim("Merged as duplicates")));
            for pair in &report.merged {
                text.push_str(&format!(
                    "  {} {} {}\n",
                    ui::yellow(&format!("{} →", ids::short(&pair.other_id))),
                    ids::short(&pair.kept_id),
                    ui::dim(&format!("{:.0}%  {}", pair.similarity * 100.0, pair.kept_title))
                ));
            }
        }
        if !report.similar.is_empty() {
            text.push_str(&format!("\n{}\n", ui::dim("Related — review by hand")));
            for pair in report.similar.iter().take(10) {
                text.push_str(&format!(
                    "  {} ~ {} {}\n",
                    ids::short(&pair.kept_id),
                    ids::short(&pair.other_id),
                    ui::dim(&format!("{:.0}%  {}", pair.similarity * 100.0, pair.kept_title))
                ));
            }
        }
        for summary in &report.summaries {
            text.push_str(&format!("\n{}\n{summary}\n", ui::dim("Suggested consolidation")));
        }
        for note in &report.notes {
            text.push_str(&format!("\n{}", ui::dim(&format!("  {note}"))));
        }
        text
    })
}

/// Write the Markdown mirror and any bound agent files.
pub async fn sync(app: &App, global: &GlobalArgs, args: &SyncArgs) -> Result<()> {
    let project = app.resolve_project(global.project.as_deref())?;
    let mirror = Mirror::new(app);

    let adopted = if args.adopt { mirror.adopt(project.as_ref())? } else { Vec::new() };

    let mirror_report = match &project {
        Some(project) => mirror.export_project(project, args.force)?,
        None => mirror.export_global(args.force)?,
    };

    let agent_results = match (&project, args.mirror_only) {
        (Some(project), false) => export_bound_agents(app, project, args.force).await?,
        _ => Vec::new(),
    };

    #[derive(Serialize)]
    struct SyncOutput {
        adopted: Vec<String>,
        mirror: crate::sync::mirror::MirrorReport,
        agents: Vec<crate::sync::agent_sync::ExportResult>,
    }
    let out = SyncOutput {
        adopted: adopted.clone(),
        mirror: mirror_report.clone(),
        agents: agent_results.clone(),
    };

    output::render(global, &out, || {
        let mut text = format!("{}\n", ui::header("Sync"));
        let written =
            mirror_report.count(WriteStatus::Created) + mirror_report.count(WriteStatus::Updated);
        text.push_str(&ui::kv(&[
            (
                "Mirror",
                format!(
                    "{written} written, {} unchanged",
                    mirror_report.count(WriteStatus::Unchanged)
                ),
            ),
            (
                "Agents",
                if agent_results.is_empty() {
                    ui::dim("none bound")
                } else {
                    agent_results
                        .iter()
                        .map(|r| format!("{} ({})", r.agent, r.outcome.status.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                },
            ),
            ("Root", app.paths().root().display().to_string()),
        ]));

        if !adopted.is_empty() {
            text.push_str(&format!(
                "\n\n{}\n",
                ui::ok(&format!("Adopted {} hand-edited sections", adopted.len()))
            ));
            for title in adopted.iter().take(10) {
                text.push_str(&format!("  - {title}\n"));
            }
        }

        let conflicts: Vec<String> = mirror_report
            .conflicts()
            .iter()
            .map(|f| f.path.display().to_string())
            .chain(
                agent_results
                    .iter()
                    .filter(|r| r.outcome.status == WriteStatus::Conflict)
                    .map(|r| r.outcome.path.display().to_string()),
            )
            .collect();
        if !conflicts.is_empty() {
            text.push_str(&format!(
                "\n\n{}\n",
                ui::warn("Edited outside ContextD, left untouched:")
            ));
            for path in &conflicts {
                text.push_str(&format!("  {path}\n"));
            }
            text.push_str(&ui::hint(
                "Run `contextd sync --adopt` to keep those edits, or --force to overwrite.",
            ));
        }
        text
    })
}
