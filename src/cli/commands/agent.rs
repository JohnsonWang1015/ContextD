//! `import` and `export`.

use std::path::PathBuf;

use clap::Args;

use crate::agents;
use crate::app::App;
use crate::cli::{output, GlobalArgs};
use crate::error::Result;
use crate::sync::agent_sync::{AgentSync, ExportOptions};
use crate::sync::WriteStatus;
use crate::ui;

/// `contextd import <agent>`
#[derive(Debug, Args)]
pub struct ImportArgs {
    /// claude, codex, cursor or generic.
    #[arg(value_name = "AGENT")]
    pub agent: String,

    /// Also read the agent's global configuration (e.g. ~/.claude/CLAUDE.md).
    #[arg(long)]
    pub include_global: bool,

    /// Show what would be imported without storing anything.
    #[arg(long)]
    pub dry_run: bool,
}

/// `contextd export <agent>`
#[derive(Debug, Args)]
pub struct ExportArgs {
    /// claude, codex, cursor or generic.
    #[arg(value_name = "AGENT")]
    pub agent: String,

    /// Write somewhere other than the agent's default file.
    #[arg(short, long, value_name = "PATH")]
    pub out: Option<PathBuf>,

    /// Print to stdout instead of writing a file.
    #[arg(long)]
    pub stdout: bool,

    /// Overwrite even if the ContextD block was edited by hand.
    #[arg(long)]
    pub force: bool,

    /// Render without writing.
    #[arg(long)]
    pub dry_run: bool,

    /// Narrow the exported context to a topic.
    #[arg(long, value_name = "TEXT")]
    pub query: Option<String>,
}

/// Import an agent's files into memory.
pub fn import(app: &App, global: &GlobalArgs, args: &ImportArgs) -> Result<()> {
    agents::get(&args.agent)?; // fail fast on an unknown agent
    let project = app.resolve_project(global.project.as_deref())?;
    let result = AgentSync::new(app).import(
        project.as_ref(),
        &args.agent,
        args.include_global,
        args.dry_run,
    )?;

    output::render(global, &result, || {
        if result.files.is_empty() {
            return format!(
                "{}\n{}",
                ui::warn(&format!("No {} files found here.", args.agent)),
                ui::hint("Nothing was imported.")
            );
        }
        let verb = if args.dry_run { "Would import" } else { "Imported" };
        let mut text = format!(
            "{}\n",
            ui::ok(&format!("{verb} {} memories from {}", result.imported.len(), args.agent))
        );
        for file in &result.files {
            text.push_str(&format!("{}\n", ui::dim(&format!("  {}", file.display()))));
        }
        for title in result.imported.iter().take(10) {
            text.push_str(&format!("  - {title}\n"));
        }
        if result.imported.len() > 10 {
            text.push_str(&ui::dim(&format!("  … and {} more\n", result.imported.len() - 10)));
        }
        if result.skipped_duplicates > 0 {
            text.push_str(&ui::dim(&format!(
                "  {} already in memory, skipped\n",
                result.skipped_duplicates
            )));
        }
        text.trim_end().to_string()
    })
}

/// Export context to an agent's files.
pub async fn export(app: &App, global: &GlobalArgs, args: &ExportArgs) -> Result<()> {
    let project = app.resolve_project(global.project.as_deref())?;
    let options = ExportOptions {
        agent: args.agent.clone(),
        path: args.out.clone(),
        force: args.force,
        dry_run: args.dry_run || args.stdout,
        query: args.query.clone(),
    };
    let result = AgentSync::new(app).export(project.as_ref(), &options).await?;

    if args.stdout {
        print!("{}", result.content);
        return Ok(());
    }

    // A conflict is a failure the caller should notice: report it through the
    // error path so the exit code is non-zero, while still emitting the JSON
    // document a scripted caller asked for.
    if result.outcome.status == WriteStatus::Conflict {
        if global.json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        return Err(crate::error::Error::SyncConflict { path: result.outcome.path.clone() });
    }

    output::render(global, &result, || match result.outcome.status {
        WriteStatus::Conflict => unreachable!("handled above"),
        WriteStatus::Skipped => result.content.clone(),
        status => {
            let mut text = format!(
                "{}\n",
                ui::ok(&format!(
                    "{} {} ({status})",
                    if status == WriteStatus::Unchanged { "Already current:" } else { "Wrote" },
                    result.outcome.path.display(),
                    status = status.as_str()
                ))
            );
            text.push_str(&ui::kv(&[
                ("agent", result.agent.clone()),
                ("memories", result.memories_included.to_string()),
                ("tokens", result.tokens.to_string()),
            ]));
            if result.dropped > 0 {
                text.push_str(&format!(
                    "\n{}",
                    ui::dim(&format!(
                        "  {} memories did not fit the context budget",
                        result.dropped
                    ))
                ));
            }
            text
        }
    })
}
