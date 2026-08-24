//! `search` and `recall`.

use clap::Args;

use crate::app::App;
use crate::cli::{output, parse_categories, GlobalArgs};
use crate::core::model::RecordKind;
use crate::error::Result;
use crate::search::{SearchMode, SearchRequest, SearchService};
use crate::storage::repository::ProjectScope;
use crate::ui;
use crate::util::text;

/// `contextd search`
#[derive(Debug, Args)]
pub struct SearchArgs {
    /// Words to search for.
    #[arg(required = true, value_name = "QUERY")]
    pub query: Vec<String>,

    /// Restrict to categories, repeatable or comma-separated.
    #[arg(short, long = "category", value_name = "CATEGORY")]
    pub categories: Vec<String>,

    /// Restrict to record kinds: memory, decision, checkpoint.
    #[arg(short, long = "kind", value_name = "KIND")]
    pub kinds: Vec<String>,

    /// Search every project, not just the current one.
    #[arg(long)]
    pub all_projects: bool,

    /// Include superseded and archived records.
    #[arg(long)]
    pub history: bool,

    /// Keyword matching only, no vectors.
    #[arg(long)]
    pub exact: bool,

    /// Maximum results.
    #[arg(short = 'n', long, default_value_t = 10)]
    pub limit: usize,

    /// Show the score breakdown for each hit.
    #[arg(long)]
    pub explain: bool,
}

/// `contextd recall`
#[derive(Debug, Args)]
pub struct RecallArgs {
    /// The question to answer from memory.
    #[arg(required = true, value_name = "QUESTION")]
    pub question: Vec<String>,

    /// Search every project.
    #[arg(long)]
    pub all_projects: bool,

    /// Include superseded records, marked as history.
    #[arg(long)]
    pub history: bool,

    /// Maximum results.
    #[arg(short = 'n', long, default_value_t = 5)]
    pub limit: usize,

    /// Print full memory content rather than an excerpt.
    #[arg(long)]
    pub full: bool,

    /// Show the score breakdown for each hit.
    #[arg(long)]
    pub explain: bool,
}

/// Keyword-first search.
pub async fn search(app: &App, global: &GlobalArgs, args: &SearchArgs) -> Result<()> {
    let scope = scope_for(app, global, args.all_projects)?;
    let kinds: Vec<RecordKind> = args
        .kinds
        .iter()
        .flat_map(|k| k.split(','))
        .filter(|k| !k.trim().is_empty())
        .map(str::parse)
        .collect::<Result<Vec<_>>>()?;

    let request = SearchRequest {
        categories: parse_categories(&args.categories)?,
        kinds,
        limit: args.limit,
        include_history: args.history,
        mode: if args.exact { SearchMode::Fulltext } else { SearchMode::Hybrid },
        ..SearchRequest::new(args.query.join(" ")).in_scope(scope)
    };
    let hits = SearchService::new(app).search(&request).await?;

    output::render(global, &hits, || {
        let mut text = output::hits_table(&hits, true);
        if args.explain {
            text.push_str("\n\n");
            text.push_str(&explain(&hits));
        }
        text
    })
}

/// Question-first retrieval: semantic plus keyword, answers first.
pub async fn recall(app: &App, global: &GlobalArgs, args: &RecallArgs) -> Result<()> {
    let scope = scope_for(app, global, args.all_projects)?;
    let question = args.question.join(" ");
    let request = SearchRequest {
        limit: args.limit,
        include_history: args.history,
        mode: SearchMode::Hybrid,
        ..SearchRequest::new(question.clone()).in_scope(scope)
    };
    let hits = SearchService::new(app).search(&request).await?;

    output::render(global, &hits, || {
        if hits.is_empty() {
            return format!(
                "{}\n{}",
                ui::dim("Nothing in memory matches that."),
                ui::hint("Try `contextd search` for keywords, or `contextd refresh` if the index is stale.")
            );
        }

        let mut text = format!("{}\n\n", ui::dim(&format!("? {question}")));
        for (index, hit) in hits.iter().enumerate() {
            let marker = if hit.is_current() { ui::green("●") } else { ui::yellow("○") };
            text.push_str(&format!("{marker} {}\n", ui::bold(&hit.title)));
            // Many memories are a single sentence, in which case the title is
            // the content; printing it twice is just noise.
            let body = if args.full {
                hit.content.trim().to_string()
            } else {
                text::one_line(&hit.content, 220)
            };
            if body.trim() != hit.title.trim() {
                for line in body.lines() {
                    text.push_str(&format!("  {line}\n"));
                }
            }
            let mut meta = vec![hit.kind.to_string(), crate::util::ids::short(&hit.id).to_string()];
            if let Some(project) = &hit.project_name {
                meta.push(project.clone());
            }
            if !hit.is_current() {
                meta.push(format!("{} — no longer current", hit.status));
            }
            text.push_str(&format!("  {}\n", ui::dim(&meta.join(" · "))));
            if args.explain {
                text.push_str(&format!("  {}\n", ui::dim(&breakdown_line(hit))));
            }
            if index + 1 < hits.len() {
                text.push('\n');
            }
        }
        text
    })
}

/// Current project plus global memories, unless the user asked for everything.
fn scope_for(app: &App, global: &GlobalArgs, all_projects: bool) -> Result<ProjectScope> {
    if all_projects {
        return Ok(ProjectScope::Any);
    }
    Ok(match app.resolve_project(global.project.as_deref())? {
        Some(project) => ProjectScope::ProjectWithGlobal(project.id),
        None => ProjectScope::Any,
    })
}

fn explain(hits: &[crate::search::SearchHit]) -> String {
    let rows: Vec<Vec<String>> = hits
        .iter()
        .map(|hit| {
            vec![
                crate::util::ids::short(&hit.id).to_string(),
                format!("{:.3}", hit.breakdown.total),
                format!("{:.3}", hit.breakdown.fts),
                format!("{:.3}", hit.breakdown.semantic),
                format!("{:.3}", hit.breakdown.priority),
                format!("{:.3}", hit.breakdown.recency),
                format!("{:.3}", hit.breakdown.project),
                format!("{:.2}", hit.breakdown.status_multiplier),
            ]
        })
        .collect();
    ui::table(
        &["id", "total", "fts", "semantic", "priority", "recency", "project", "status×"],
        &rows,
    )
}

fn breakdown_line(hit: &crate::search::SearchHit) -> String {
    format!(
        "score {:.3} = fts {:.3} + semantic {:.3} + priority {:.3} + recency {:.3} + project {:.3}, ×{:.2}",
        hit.breakdown.total,
        hit.breakdown.fts,
        hit.breakdown.semantic,
        hit.breakdown.priority,
        hit.breakdown.recency,
        hit.breakdown.project,
        hit.breakdown.status_multiplier
    )
}
