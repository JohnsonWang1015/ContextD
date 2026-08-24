//! `decision add|list|show|supersede|delete`.

use clap::{Args, Subcommand};

use crate::app::App;
use crate::cli::{output, GlobalArgs};
use crate::core::decision::{DecisionService, NewDecision};
use crate::core::model::{Decision, DecisionStatus};
use crate::error::Result;
use crate::ui;
use crate::util::{ids, text, time};

/// Architecture decision records.
#[derive(Debug, Subcommand)]
pub enum DecisionCommand {
    /// Record a decision.
    Add(AddDecisionArgs),
    /// List decisions (current by default).
    List(ListDecisionArgs),
    /// Show one decision in full.
    Show(ShowDecisionArgs),
    /// Mark one decision as replaced by another.
    Supersede(SupersedeDecisionArgs),
    /// Delete a decision.
    Delete(ShowDecisionArgs),
}

#[derive(Debug, Args)]
pub struct AddDecisionArgs {
    /// What was decided. Multiple words are joined.
    #[arg(required = true, value_name = "DECISION")]
    pub decision: Vec<String>,

    /// Short title, e.g. "Task queue transport".
    #[arg(short, long)]
    pub title: Option<String>,

    /// Why the decision was needed.
    #[arg(short, long)]
    pub context: Option<String>,

    /// What follows from it.
    #[arg(long)]
    pub consequences: Option<String>,

    /// An option considered and not taken, repeatable.
    #[arg(long = "alternative", value_name = "TEXT")]
    pub alternatives: Vec<String>,

    /// proposed, accepted, superseded or rejected.
    #[arg(short, long, default_value = "accepted")]
    pub status: DecisionStatus,

    /// Id (or prefix) of the decision this replaces.
    #[arg(long, value_name = "ID")]
    pub supersedes: Option<String>,
}

#[derive(Debug, Args)]
pub struct ListDecisionArgs {
    /// Include superseded and rejected decisions.
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct ShowDecisionArgs {
    /// Decision id or unique prefix.
    pub id: String,
}

#[derive(Debug, Args)]
pub struct SupersedeDecisionArgs {
    /// The decision that no longer holds.
    pub old: String,
    /// The decision that replaces it.
    pub new: String,
}

/// Dispatch a decision subcommand.
pub fn run(app: &App, global: &GlobalArgs, command: DecisionCommand) -> Result<()> {
    match command {
        DecisionCommand::Add(args) => add(app, global, &args),
        DecisionCommand::List(args) => list(app, global, &args),
        DecisionCommand::Show(args) => show(app, global, &args),
        DecisionCommand::Supersede(args) => supersede(app, global, &args),
        DecisionCommand::Delete(args) => delete(app, global, &args),
    }
}

fn add(app: &App, global: &GlobalArgs, args: &AddDecisionArgs) -> Result<()> {
    let project = app.require_project(global.project.as_deref())?;
    let body = args.decision.join(" ");
    let title = args.title.clone().unwrap_or_else(|| {
        text::truncate_chars(body.split(['.', '。']).next().unwrap_or(&body).trim(), 72)
    });

    let decision = DecisionService::new(app).record(
        &project,
        NewDecision {
            title,
            decision: body,
            context: args.context.clone(),
            consequences: args.consequences.clone(),
            alternatives: args.alternatives.clone(),
            status: args.status,
            supersedes: args.supersedes.clone(),
        },
    )?;

    output::render(global, &decision, || {
        let mut text =
            format!("{}\n", ui::ok(&format!("Decision recorded for {}", ui::bold(&project.name))));
        text.push_str(&ui::kv(&[
            ("id", ids::short(&decision.id).to_string()),
            ("title", decision.title.clone()),
            ("decision", decision.decision.clone()),
            ("status", decision.status.to_string()),
        ]));
        if args.supersedes.is_some() {
            text.push_str(&format!(
                "\n{}",
                ui::hint("The previous decision is now marked superseded.")
            ));
        }
        text
    })
}

fn list(app: &App, global: &GlobalArgs, args: &ListDecisionArgs) -> Result<()> {
    let project = app.require_project(global.project.as_deref())?;
    let service = DecisionService::new(app);
    let decisions = if args.all { service.all(&project)? } else { service.current(&project)? };

    output::render(global, &decisions, || {
        if decisions.is_empty() {
            return format!(
                "{}\n{}",
                ui::dim("No decisions recorded."),
                ui::hint("Record one with `contextd decision add \"...\"`.")
            );
        }
        let rows: Vec<Vec<String>> = decisions
            .iter()
            .map(|decision| {
                vec![
                    ids::short(&decision.id).to_string(),
                    text::one_line(&decision.title, 34),
                    text::one_line(&decision.decision, 52),
                    if decision.status.is_current() {
                        ui::green(decision.status.as_str())
                    } else {
                        ui::yellow(decision.status.as_str())
                    },
                    ui::dim(&time::humanize_since(&decision.decided_at)),
                ]
            })
            .collect();
        ui::table(&["id", "title", "decision", "status", "when"], &rows)
    })
}

fn show(app: &App, global: &GlobalArgs, args: &ShowDecisionArgs) -> Result<()> {
    let decision = DecisionService::new(app).get(&args.id)?;
    output::render(global, &decision, || detail(&decision))
}

fn supersede(app: &App, global: &GlobalArgs, args: &SupersedeDecisionArgs) -> Result<()> {
    let (old, new) = DecisionService::new(app).supersede(&args.old, &args.new)?;
    output::render(global, &new, || {
        format!(
            "{}\n{}",
            ui::ok("Decision history recorded."),
            ui::kv(&[
                ("was", format!("{} {}", ui::dim(ids::short(&old.id)), old.decision)),
                ("now", format!("{} {}", ui::dim(ids::short(&new.id)), new.decision)),
            ])
        )
    })
}

fn delete(app: &App, global: &GlobalArgs, args: &ShowDecisionArgs) -> Result<()> {
    let decision = DecisionService::new(app).delete(&args.id)?;
    output::render(global, &decision, || ui::ok(&format!("Deleted “{}”.", decision.title)))
}

fn detail(decision: &Decision) -> String {
    let mut out = format!("{}\n\n{}\n\n", ui::bold(&decision.title), decision.decision.trim());
    if let Some(context) = decision.context.as_ref().filter(|c| !c.trim().is_empty()) {
        out.push_str(&format!("{}\n{}\n\n", ui::dim("Context"), context.trim()));
    }
    if let Some(consequences) = decision.consequences.as_ref().filter(|c| !c.trim().is_empty()) {
        out.push_str(&format!("{}\n{}\n\n", ui::dim("Consequences"), consequences.trim()));
    }
    if !decision.alternatives.is_empty() {
        out.push_str(&format!("{}\n", ui::dim("Alternatives")));
        for alternative in &decision.alternatives {
            out.push_str(&format!("- {alternative}\n"));
        }
        out.push('\n');
    }
    let mut rows = vec![
        ("id", decision.id.clone()),
        ("status", decision.status.to_string()),
        ("decided", time::to_storage(&decision.decided_at)),
    ];
    if let Some(previous) = &decision.supersedes {
        rows.push(("supersedes", ids::short(previous).to_string()));
    }
    if let Some(next) = &decision.superseded_by {
        rows.push(("superseded by", ids::short(next).to_string()));
    }
    out.push_str(&ui::kv(&rows));
    out
}
