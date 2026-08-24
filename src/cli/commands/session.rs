//! `session start|end|list|show`.

use clap::{Args, Subcommand};
use serde::Serialize;

use crate::app::App;
use crate::cli::{output, GlobalArgs};
use crate::core::model::Session;
use crate::core::session::{humanize_duration, SessionActivity, SessionService};
use crate::error::Result;
use crate::ui;
use crate::util::{ids, text, time};

/// Working sessions.
#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    /// Open a session for this project.
    Start(StartArgs),
    /// Close the open session.
    End(EndArgs),
    /// List recent sessions.
    List(ListArgs),
    /// Show what happened during a session.
    Show(ShowArgs),
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// Who is working: claude, codex, cursor, … (default: cli).
    #[arg(long)]
    pub agent: Option<String>,
}

#[derive(Debug, Args)]
pub struct EndArgs {
    /// What the session achieved.
    #[arg(value_name = "SUMMARY")]
    pub summary: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// How many to show.
    #[arg(short = 'n', long, default_value_t = 10)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Session id or prefix (default: the current or most recent session).
    pub id: Option<String>,
}

/// Dispatch a session subcommand.
pub fn run(app: &App, global: &GlobalArgs, command: SessionCommand) -> Result<()> {
    match command {
        SessionCommand::Start(args) => start(app, global, &args),
        SessionCommand::End(args) => end(app, global, &args),
        SessionCommand::List(args) => list(app, global, &args),
        SessionCommand::Show(args) => show(app, global, &args),
    }
}

fn start(app: &App, global: &GlobalArgs, args: &StartArgs) -> Result<()> {
    let project = app.require_project(global.project.as_deref())?;
    let (session, closed) = SessionService::new(app).start(&project, args.agent.as_deref())?;

    #[derive(Serialize)]
    struct StartOutput {
        session: Session,
        closed_previous: Option<Session>,
    }
    let out = StartOutput { session: session.clone(), closed_previous: closed.clone() };

    output::render(global, &out, || {
        let mut text = format!(
            "{}\n{}",
            ui::ok(&format!("Session open on {}", ui::bold(&project.name))),
            ui::kv(&[
                ("id", ids::short(&session.id).to_string()),
                ("agent", session.agent.clone().unwrap_or_else(|| "cli".into())),
                ("started", time::to_storage(&session.started_at)),
            ])
        );
        if let Some(previous) = &closed {
            text.push_str(&format!(
                "\n\n{}",
                ui::warn(&format!(
                    "closed the session left open by {} ({})",
                    previous.agent.clone().unwrap_or_else(|| "cli".into()),
                    time::humanize_since(&previous.started_at)
                ))
            ));
        }
        text.push_str(&format!(
            "\n{}",
            ui::hint("Checkpoints made from now on belong to this session.")
        ));
        text
    })
}

fn end(app: &App, global: &GlobalArgs, args: &EndArgs) -> Result<()> {
    let project = app.require_project(global.project.as_deref())?;
    let service = SessionService::new(app);
    let summary = args.summary.join(" ");
    let Some(session) = service.end(&project, Some(summary.as_str()))? else {
        return output::render(global, &serde_json::json!({"session": null}), || {
            format!(
                "{}\n{}",
                ui::dim("No session is open."),
                ui::hint("Open one with `contextd session start`.")
            )
        });
    };

    let activity = service.activity(&session)?;
    output::render(global, &activity, || {
        format!(
            "{}\n{}",
            ui::ok(&format!("Session closed after {}", humanize_duration(activity.duration()))),
            summarise(&activity)
        )
    })
}

fn list(app: &App, global: &GlobalArgs, args: &ListArgs) -> Result<()> {
    let project = app.require_project(global.project.as_deref())?;
    let service = SessionService::new(app);
    let sessions = service.history(&project, args.limit)?;

    // Each row reports what the session produced, which is the only reason to
    // look at a list of sessions at all.
    let activities: Vec<SessionActivity> =
        sessions.iter().map(|session| service.activity(session)).collect::<Result<_>>()?;

    output::render(global, &activities, || {
        if activities.is_empty() {
            return format!(
                "{}\n{}",
                ui::dim("No sessions recorded."),
                ui::hint("`contextd mcp serve` opens one automatically; or run `contextd session start`.")
            );
        }
        let rows: Vec<Vec<String>> = activities
            .iter()
            .map(|activity| {
                vec![
                    ids::short(&activity.session.id).to_string(),
                    activity.session.agent.clone().unwrap_or_else(|| "cli".into()),
                    if activity.is_open() {
                        ui::green("open")
                    } else {
                        humanize_duration(activity.duration())
                    },
                    activity.checkpoints.len().to_string(),
                    activity.memories.len().to_string(),
                    activity.decisions.len().to_string(),
                    activity
                        .session
                        .summary
                        .clone()
                        .map(|s| text::one_line(&s, 40))
                        .unwrap_or_default(),
                    ui::dim(&time::humanize_since(&activity.session.started_at)),
                ]
            })
            .collect();
        ui::table(&["id", "agent", "ran", "ckpt", "mem", "adr", "summary", "started"], &rows)
    })
}

fn show(app: &App, global: &GlobalArgs, args: &ShowArgs) -> Result<()> {
    let project = app.require_project(global.project.as_deref())?;
    let service = SessionService::new(app);

    let session = match &args.id {
        Some(ident) => Some(service.resolve(&project, ident)?),
        None => match service.current(&project)? {
            Some(open) => Some(open),
            None => service.latest(&project)?,
        },
    };

    let Some(session) = session else {
        return output::render(global, &serde_json::json!({"session": null}), || {
            ui::dim("No sessions recorded for this project.").to_string()
        });
    };

    let activity = service.activity(&session)?;
    output::render(global, &activity, || {
        format!(
            "{}\n{}\n\n{}",
            ui::header(&format!("Session {}", ids::short(&session.id))),
            ui::kv(&[
                ("agent", session.agent.clone().unwrap_or_else(|| "cli".into())),
                ("window", crate::core::session::window(&session)),
                ("ran", humanize_duration(activity.duration())),
                ("summary", session.summary.clone().unwrap_or_else(|| ui::dim("—").to_string())),
            ]),
            summarise(&activity)
        )
    })
}

/// The "what came of it" part, shared by `end` and `show`.
fn summarise(activity: &SessionActivity) -> String {
    if activity.is_empty() {
        return ui::dim("Nothing was recorded during this session.").to_string();
    }

    let mut text = String::new();
    if !activity.checkpoints.is_empty() {
        text.push_str(&format!("{}\n", ui::dim("Checkpoints")));
        for checkpoint in &activity.checkpoints {
            text.push_str(&format!(
                "  {} {}\n",
                ui::dim(ids::short(&checkpoint.id)),
                text::one_line(&checkpoint.summary, 66)
            ));
        }
    }
    if !activity.decisions.is_empty() {
        text.push_str(&format!("\n{}\n", ui::dim("Decisions")));
        for decision in &activity.decisions {
            text.push_str(&format!(
                "  {} {} — {}\n",
                ui::dim(ids::short(&decision.id)),
                decision.title,
                text::one_line(&decision.decision, 50)
            ));
        }
    }
    if !activity.memories.is_empty() {
        text.push_str(&format!("\n{}\n", ui::dim("Memories")));
        for memory in activity.memories.iter().take(15) {
            text.push_str(&format!(
                "  {} [{}] {}\n",
                ui::dim(ids::short(&memory.id)),
                memory.category,
                text::one_line(&memory.title, 60)
            ));
        }
        if activity.memories.len() > 15 {
            text.push_str(&ui::dim(&format!("  … and {} more\n", activity.memories.len() - 15)));
        }
    }
    text.trim_end().to_string()
}
