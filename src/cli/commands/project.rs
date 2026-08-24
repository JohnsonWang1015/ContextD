//! `init`, `attach`, `detach`, `list`, `status`.

use std::path::PathBuf;

use clap::Args;
use serde::Serialize;

use crate::agents;
use crate::app::App;
use crate::cli::{output, GlobalArgs};
use crate::config::Config;
use crate::core::project::{AttachRequest, ProjectService, StatusReport};
use crate::error::{Error, Result};
use crate::storage::sqlite::migrations;
use crate::ui;
use crate::util::{git, time};

/// `contextd init`
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Overwrite an existing config.toml with defaults.
    #[arg(long)]
    pub reset_config: bool,
}

/// `contextd attach`
#[derive(Debug, Args)]
pub struct AttachArgs {
    /// Directory to attach (default: current directory).
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,

    /// Project name (default: derived from the git remote or directory name).
    #[arg(long)]
    pub name: Option<String>,

    /// One-line description of the project.
    #[arg(long)]
    pub description: Option<String>,

    /// Import context from detected agent files straight away.
    #[arg(long)]
    pub import: bool,
}

/// `contextd detach`
#[derive(Debug, Args)]
pub struct DetachArgs {
    /// Delete the project and everything stored for it.
    #[arg(long)]
    pub purge: bool,

    /// Skip the confirmation prompt for --purge.
    #[arg(long)]
    pub yes: bool,
}

/// `contextd list`
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Include detached projects.
    #[arg(long, short)]
    pub all: bool,
}

/// `contextd status`
#[derive(Debug, Args)]
pub struct StatusArgs {}

/// Create the home directory, database and default config.
pub fn init(app: &App, global: &GlobalArgs, args: &InitArgs) -> Result<()> {
    let paths = app.paths();
    let config_file = paths.config_file();
    let config_written = if !config_file.exists() || args.reset_config {
        Config::default().save(&config_file)?;
        true
    } else {
        false
    };

    #[derive(Serialize)]
    struct InitOutput {
        root: PathBuf,
        database: PathBuf,
        config: PathBuf,
        schema_version: i64,
        config_written: bool,
    }

    let output = InitOutput {
        root: paths.root().to_path_buf(),
        database: paths.database(),
        config: config_file,
        schema_version: migrations::target_version(),
        config_written,
    };

    output::render(global, &output, || {
        let mut text = format!("{}\n\n", ui::ok("ContextD is ready."));
        text.push_str(&ui::kv(&[
            ("Home", output.root.display().to_string()),
            ("Database", output.database.display().to_string()),
            ("Config", output.config.display().to_string()),
            ("Schema", format!("v{}", output.schema_version)),
        ]));
        text.push_str(&format!(
            "\n\n{}",
            ui::hint("Next: cd into a repository and run `contextd attach`.")
        ));
        text
    })
}

/// Attach a directory as a project.
pub fn attach(app: &App, global: &GlobalArgs, args: &AttachArgs) -> Result<()> {
    let dir = args.path.clone().unwrap_or_else(|| app.cwd().to_path_buf());
    if !dir.is_dir() {
        return Err(Error::invalid("path", format!("{} is not a directory", dir.display())));
    }

    let service = ProjectService::new(app);
    let detection = service.detect(&dir, &[]);
    // Bind every agent file that already exists, so `contextd sync` knows
    // where this project's context is expected to live.
    let discovered = agents::detect_all(&detection.root, false);
    let bindings: Vec<(String, PathBuf)> = discovered
        .iter()
        .filter(|(_, file)| file.exists)
        .map(|(agent, file)| (agent.clone(), file.path.clone()))
        .collect();

    let (project, created) = service.attach(AttachRequest {
        dir: detection.root.clone(),
        name: args.name.clone(),
        description: args.description.clone(),
        bindings: bindings.clone(),
    })?;

    let mut imported = Vec::new();
    if args.import {
        let sync = crate::sync::agent_sync::AgentSync::new(app);
        let mut agents_seen: Vec<String> = bindings.iter().map(|(a, _)| a.clone()).collect();
        agents_seen.sort();
        agents_seen.dedup();
        for agent in agents_seen {
            let result = sync.import(Some(&project), &agent, false, false)?;
            imported.extend(result.imported);
        }
    }

    #[derive(Serialize)]
    struct AttachOutput {
        project: crate::core::model::Project,
        created: bool,
        git: git::GitSnapshot,
        bound_files: Vec<PathBuf>,
        imported: Vec<String>,
    }

    let output = AttachOutput {
        git: detection.git.clone(),
        bound_files: bindings.iter().map(|(_, path)| path.clone()).collect(),
        imported,
        project: project.clone(),
        created,
    };

    output::render(global, &output, || {
        let verb = if output.created { "Attached" } else { "Updated" };
        let mut text = format!("{}\n\n", ui::ok(&format!("{verb} {}", ui::bold(&project.name))));
        let mut rows = vec![
            ("Project", project.name.clone()),
            ("Slug", project.slug.clone()),
            ("Path", detection.root.display().to_string()),
        ];
        if let Some(branch) = &output.git.branch {
            rows.push(("Branch", branch.clone()));
        }
        if let Some(remote) = &output.git.remote {
            rows.push(("Remote", remote.clone()));
        }
        rows.push((
            "Agent files",
            if output.bound_files.is_empty() {
                "none detected".to_string()
            } else {
                output
                    .bound_files
                    .iter()
                    .map(|p| {
                        p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        ));
        text.push_str(&ui::kv(&rows));
        if !output.imported.is_empty() {
            text.push_str(&format!(
                "\n\n{}",
                ui::ok(&format!("Imported {} memories", output.imported.len()))
            ));
        } else if !output.bound_files.is_empty() {
            text.push_str(&format!(
                "\n\n{}",
                ui::hint(
                    "Run `contextd attach --import` to pull existing agent files into memory."
                )
            ));
        }
        text
    })
}

/// Detach (or purge) a project.
pub fn detach(app: &App, global: &GlobalArgs, args: &DetachArgs) -> Result<()> {
    let project = app.require_project(global.project.as_deref())?;
    if args.purge
        && !args.yes
        && !confirm(&format!("Delete project {} and all of its memories? [y/N] ", project.name))?
    {
        println!("{}", ui::warn("Cancelled."));
        return Ok(());
    }

    ProjectService::new(app).detach(&project, args.purge)?;

    #[derive(Serialize)]
    struct DetachOutput {
        project: String,
        purged: bool,
    }
    let output = DetachOutput { project: project.name.clone(), purged: args.purge };
    output::render(global, &output, || {
        if args.purge {
            ui::ok(&format!("Purged {} and everything stored for it.", project.name))
        } else {
            format!(
                "{}\n{}",
                ui::ok(&format!("Detached {}.", project.name)),
                ui::hint(
                    "Memories are kept; run `contextd attach` in the same directory to resume."
                )
            )
        }
    })
}

/// List projects.
pub fn list(app: &App, global: &GlobalArgs, args: &ListArgs) -> Result<()> {
    let projects = app.store().list_projects(args.all)?;
    let current = app.store().find_project_by_path(app.cwd())?;

    #[derive(Serialize)]
    struct ListOutput {
        projects: Vec<crate::core::model::Project>,
        current: Option<String>,
    }
    let output =
        ListOutput { projects: projects.clone(), current: current.as_ref().map(|p| p.id.clone()) };

    output::render(global, &output, || {
        if projects.is_empty() {
            return format!(
                "{}\n{}",
                ui::dim("No projects yet."),
                ui::hint("Run `contextd attach` inside a repository.")
            );
        }
        let rows: Vec<Vec<String>> = projects
            .iter()
            .map(|project| {
                let stats = app.store().project_stats(&project.id).unwrap_or_default();
                let marker = if current.as_ref().is_some_and(|c| c.id == project.id) {
                    ui::green("●")
                } else {
                    " ".to_string()
                };
                vec![
                    marker,
                    project.name.clone(),
                    project.slug.clone(),
                    stats.active_memories.to_string(),
                    stats.decisions.to_string(),
                    stats.checkpoints.to_string(),
                    if project.active { String::new() } else { ui::dim("detached") },
                    ui::dim(
                        &project
                            .root_path
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                    ),
                ]
            })
            .collect();
        ui::table(&["", "name", "slug", "mem", "adr", "ckpt", "", "path"], &rows)
    })
}

/// Show project status.
pub async fn status(app: &App, global: &GlobalArgs, _args: &StatusArgs) -> Result<()> {
    let project = app.resolve_project(global.project.as_deref())?;
    let report = ProjectService::new(app).status(project.as_ref()).await?;

    #[derive(Serialize)]
    struct StatusOutput {
        project: Option<crate::core::model::Project>,
        stats: crate::storage::repository::ProjectStats,
        branch: Option<String>,
        commit: Option<String>,
        dirty_files: usize,
        embedded: (usize, usize),
        embedding_provider: String,
        vector: crate::search::vector::VectorHealth,
        latest_checkpoint: Option<crate::core::model::Checkpoint>,
        session: Option<crate::core::session::SessionActivity>,
        agents: Vec<String>,
    }

    let output = StatusOutput {
        project: report.project.clone(),
        stats: report.stats.clone(),
        branch: report.git.branch.clone(),
        commit: report.git.commit.clone(),
        dirty_files: report.git.dirty_files.len(),
        embedded: report.embedded,
        embedding_provider: report.embedding_provider.clone(),
        vector: report.vector.clone(),
        latest_checkpoint: report.latest_checkpoint.clone(),
        session: report.session.clone(),
        agents: report.bindings.iter().map(|b| b.agent.clone()).collect(),
    };

    output::render(global, &output, || render_status(&report))
}

fn render_status(report: &StatusReport) -> String {
    let mut text = format!("{}\n", ui::header("ContextD"));

    let Some(project) = &report.project else {
        text.push_str(&ui::dim("No project attached to this directory.\n"));
        text.push_str(&ui::hint("Run `contextd attach` here, or `contextd list` to see projects."));
        return text;
    };

    let mut rows = vec![("Project", project.name.clone())];
    if let Some(branch) = &report.git.branch {
        let mut value = branch.clone();
        if let Some(commit) = &report.git.commit {
            value.push_str(&format!(" @ {}", git::short_commit(commit)));
        }
        if !report.git.dirty_files.is_empty() {
            value.push_str(&ui::yellow(&format!(" ({} dirty)", report.git.dirty_files.len())));
        }
        rows.push(("Branch", value));
    }
    if let Some(activity) = &report.session {
        // An open session is live state and belongs at the top; a finished one
        // is history and reads better next to the checkpoint below.
        if activity.is_open() {
            rows.push((
                "Session",
                format!(
                    "{} {}",
                    ui::green(&activity.headline()),
                    ui::dim(&format!(
                        "{}, {}",
                        ui::plural(activity.checkpoints.len(), "checkpoint", "checkpoints"),
                        ui::plural(activity.memories.len(), "memory", "memories")
                    ))
                ),
            ));
        }
    }
    rows.push(("Memories", report.stats.active_memories.to_string()));
    if report.stats.superseded_memories > 0 {
        rows.push(("History", format!("{} superseded", report.stats.superseded_memories)));
    }
    rows.push(("Decisions", report.stats.decisions.to_string()));
    rows.push(("Checkpoints", report.stats.checkpoints.to_string()));
    text.push_str(&ui::kv(&rows));
    text.push_str("\n\n");

    if let Some(activity) = report.session.as_ref().filter(|activity| !activity.is_open()) {
        text.push_str(&format!(
            "{}\n{}{}\n\n",
            ui::dim("Last session"),
            activity.headline(),
            activity
                .session
                .summary
                .as_ref()
                .map(|summary| format!(" — {summary}"))
                .unwrap_or_default()
        ));
    }

    match &report.latest_checkpoint {
        Some(checkpoint) => {
            text.push_str(&format!(
                "{}\n{} {}\n",
                ui::dim("Last checkpoint"),
                checkpoint.summary,
                ui::dim(&format!("({})", time::humanize_since(&checkpoint.created_at)))
            ));
            if let Some(goal) = &checkpoint.current_goal {
                text.push_str(&format!("\n{}\n{goal}\n", ui::dim("Current goal")));
            }
            if !checkpoint.next_steps.is_empty() {
                text.push_str(&format!("\n{}\n", ui::dim("Next")));
                for step in &checkpoint.next_steps {
                    text.push_str(&format!("- {step}\n"));
                }
            }
        }
        None => {
            text.push_str(&format!("{}\n", ui::dim("No checkpoint yet")));
        }
    }

    let (embedded, total) = report.embedded;
    let index_state = if total == 0 {
        ui::dim("nothing to index")
    } else if embedded == total {
        format!("{} {}", ui::check(true), ui::dim(&format!("{embedded}/{total}")))
    } else {
        format!(
            "{} {}",
            ui::yellow("!"),
            ui::dim(&format!("{embedded}/{total} — run `contextd refresh`"))
        )
    };

    let agents: Vec<String> = {
        let mut agents: Vec<String> = report.bindings.iter().map(|b| b.agent.clone()).collect();
        agents.sort();
        agents.dedup();
        agents
    };

    // An external store is worth a line of its own: when Qdrant is down,
    // recall silently degrades to keyword search, and that must be visible.
    let vector_state = if !report.vector.is_external_backend() {
        ui::dim("sqlite (built in)")
    } else if report.vector.reachable {
        format!(
            "{} {}",
            ui::check(true),
            ui::dim(&format!(
                "{}{}",
                report.vector.backend,
                report.vector.points.map(|n| format!(" · {n} points")).unwrap_or_default()
            ))
        )
    } else {
        format!(
            "{} {}",
            ui::red("✗"),
            ui::dim(&format!(
                "{} unreachable — recall is keyword-only{}",
                report.vector.backend,
                report.vector.detail.as_ref().map(|d| format!(" ({d})")).unwrap_or_default()
            ))
        )
    };

    text.push_str(&format!(
        "\n{}\n",
        ui::kv(&[
            ("Semantic index", format!("{index_state}  {}", ui::dim(&report.embedding_provider))),
            ("Vector store", vector_state),
            ("Agents", if agents.is_empty() { ui::dim("none bound") } else { agents.join(", ") }),
            ("MCP", format!("{} {}", ui::check(true), ui::dim("contextd mcp serve"))),
        ])
    ));
    text
}

/// Ask a yes/no question on the terminal.
fn confirm(prompt: &str) -> Result<bool> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}
