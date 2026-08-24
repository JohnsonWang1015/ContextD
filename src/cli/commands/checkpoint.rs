//! `checkpoint` and `resume`.

use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::{output, GlobalArgs};
use crate::core::checkpoint::{CheckpointService, NewCheckpoint};
use crate::core::context::{render, ContextBuilder, ContextRequest};
use crate::core::model::{Checkpoint, RecordRef};
use crate::error::Result;
use crate::search::IndexService;
use crate::ui;
use crate::util::{git, time};

/// `contextd checkpoint`
#[derive(Debug, Args)]
pub struct CheckpointArgs {
    /// What you just finished. Multiple words are joined.
    #[arg(value_name = "SUMMARY")]
    pub summary: Vec<String>,

    /// What you are working towards (carried forward when omitted).
    #[arg(short, long)]
    pub goal: Option<String>,

    /// Where things stand right now.
    #[arg(long)]
    pub state: Option<String>,

    /// Something finished, repeatable.
    #[arg(long = "done", value_name = "ITEM")]
    pub completed: Vec<String>,

    /// Something to do next, repeatable.
    #[arg(long = "next", value_name = "ITEM")]
    pub next_steps: Vec<String>,

    /// An unresolved problem, repeatable.
    #[arg(long = "problem", value_name = "ITEM")]
    pub open_problems: Vec<String>,

    /// A file this work touches, repeatable.
    #[arg(long = "file", value_name = "PATH")]
    pub related_files: Vec<String>,

    /// List checkpoints instead of creating one.
    #[arg(long)]
    pub list: bool,

    /// How many to list.
    #[arg(short = 'n', long, default_value_t = 10)]
    pub limit: usize,
}

/// `contextd resume`
#[derive(Debug, Args)]
pub struct ResumeArgs {
    /// Focus the context on a question.
    #[arg(value_name = "QUERY")]
    pub query: Vec<String>,

    /// Token budget for the generated context.
    #[arg(long, value_name = "N")]
    pub max_tokens: Option<usize>,

    /// Full Markdown context rather than the short handover form.
    #[arg(long)]
    pub full: bool,
}

/// Record or list checkpoints.
pub async fn checkpoint(app: &App, global: &GlobalArgs, args: &CheckpointArgs) -> Result<()> {
    let project = app.require_project(global.project.as_deref())?;
    let service = CheckpointService::new(app);

    if args.list {
        let checkpoints = service.history(&project, args.limit)?;
        return output::render(global, &checkpoints, || checkpoint_table(&checkpoints));
    }

    let summary = args.summary.join(" ");
    if summary.trim().is_empty() {
        return Err(crate::error::Error::invalid(
            "summary",
            "say what you finished, e.g. `contextd checkpoint \"worker heartbeat completed\"`",
        ));
    }

    let checkpoint = service.create(
        &project,
        NewCheckpoint {
            summary,
            current_goal: args.goal.clone(),
            current_state: args.state.clone(),
            completed: args.completed.clone(),
            next_steps: args.next_steps.clone(),
            open_problems: args.open_problems.clone(),
            related_files: args.related_files.clone(),
            skip_git: false,
        },
    )?;

    let _ = IndexService::new(app).embed_record(&RecordRef::checkpoint(&checkpoint.id)).await;

    output::render(global, &checkpoint, || {
        let mut text =
            format!("{}\n", ui::ok(&format!("Checkpoint saved for {}", ui::bold(&project.name))));
        let mut rows = vec![("summary", checkpoint.summary.clone())];
        if let Some(goal) = &checkpoint.current_goal {
            rows.push(("goal", goal.clone()));
        }
        if let Some(branch) = &checkpoint.git_branch {
            let mut value = branch.clone();
            if let Some(commit) = &checkpoint.git_commit {
                value.push_str(&format!(" @ {}", git::short_commit(commit)));
            }
            if !checkpoint.dirty_files.is_empty() {
                value.push_str(&format!(" ({} dirty)", checkpoint.dirty_files.len()));
            }
            rows.push(("git", value));
        }
        if !checkpoint.next_steps.is_empty() {
            rows.push(("next", checkpoint.next_steps.join(", ")));
        }
        text.push_str(&ui::kv(&rows));
        text.push_str(&format!("\n\n{}", ui::hint("Pick it up later with `contextd resume`.")));
        text
    })
}

/// Print context for resuming work.
pub async fn resume(app: &App, global: &GlobalArgs, args: &ResumeArgs) -> Result<()> {
    let project = app.resolve_project(global.project.as_deref())?;
    let mut request = ContextRequest::from_config(app, project.clone());
    if let Some(max_tokens) = args.max_tokens {
        request.max_tokens = max_tokens;
    }
    let query = args.query.join(" ");
    if !query.trim().is_empty() {
        request = request.with_query(query);
    }

    let bundle = ContextBuilder::new(app).build(&request).await?;

    #[derive(Serialize)]
    struct ResumeOutput<'a> {
        text: String,
        budget: &'a crate::core::context::BudgetReport,
        memories: usize,
    }

    let text = if args.full {
        render::markdown(&bundle, &render::RenderOptions::default())
    } else {
        render::resume(&bundle)
    };
    let payload = ResumeOutput {
        text: text.clone(),
        budget: &bundle.budget,
        memories: bundle.memories.len(),
    };

    // A finished session is the closest thing to "what happened last time",
    // so it belongs in the handover the next agent reads.
    let previous_session = match &project {
        Some(project) => {
            let sessions = crate::core::session::SessionService::new(app);
            sessions
                .latest(project)?
                .filter(|session| session.ended_at.is_some())
                .map(|session| sessions.activity(&session))
                .transpose()?
        }
        None => None,
    };

    output::render(global, &payload, || {
        let mut out = text.clone();
        if let Some(activity) = &previous_session {
            out.push_str(&format!(
                "\nLast session: {}{}\n",
                activity.headline(),
                activity
                    .session
                    .summary
                    .as_ref()
                    .map(|summary| format!(" — {summary}"))
                    .unwrap_or_default()
            ));
        }
        if bundle.budget.dropped > 0 {
            out.push_str(&format!(
                "\n{}\n",
                ui::dim(&format!(
                    "({} more memories did not fit in {} tokens — narrow with `contextd resume \"<topic>\"`)",
                    bundle.budget.dropped, bundle.budget.max_tokens
                ))
            ));
        }
        out
    })
}

fn checkpoint_table(checkpoints: &[Checkpoint]) -> String {
    if checkpoints.is_empty() {
        return ui::dim("No checkpoints yet.").to_string();
    }
    let rows: Vec<Vec<String>> = checkpoints
        .iter()
        .map(|checkpoint| {
            vec![
                crate::util::ids::short(&checkpoint.id).to_string(),
                crate::util::text::one_line(&checkpoint.summary, 60),
                checkpoint.git_branch.clone().unwrap_or_else(|| "—".into()),
                ui::dim(&time::humanize_since(&checkpoint.created_at)),
            ]
        })
        .collect();
    ui::table(&["id", "summary", "branch", "when"], &rows)
}
