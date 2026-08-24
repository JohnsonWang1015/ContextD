//! Command-line interface.
//!
//! The CLI is a thin shell over the services in [`crate::core`]: it parses
//! arguments, resolves the project, calls one service and renders the result.
//! Anything with logic in it belongs a layer down, where the MCP server can
//! reach it too.

pub mod commands;
pub mod output;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::app::App;
use crate::config::Paths;
use crate::core::model::{Category, Status};
use crate::error::Result;

/// Developer context and semantic memory for AI coding agents.
#[derive(Debug, Parser)]
#[command(
    name = "contextd",
    version,
    about = "Developer context & semantic memory manager for AI coding agents",
    long_about = "ContextD stores what you would otherwise re-explain to Claude Code, Codex or \
                  Cursor every session: project context, architecture decisions, conventions, \
                  current tasks and where you left off — and hands back only the parts that \
                  matter, through the CLI or an MCP server.",
    propagate_version = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

/// Options accepted by every subcommand.
#[derive(Debug, Args, Clone, Default)]
pub struct GlobalArgs {
    /// ContextD home directory (default: $CONTEXTD_HOME or ~/.contextd).
    #[arg(long, global = true, value_name = "DIR", env = "CONTEXTD_HOME")]
    pub home: Option<PathBuf>,

    /// Act on this project instead of the one attached to the current directory.
    #[arg(short, long, global = true, value_name = "NAME")]
    pub project: Option<String>,

    /// Emit JSON instead of formatted text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Disable colour.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Increase log verbosity (repeatable).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

impl GlobalArgs {
    /// Resolve the ContextD home for this invocation.
    pub fn paths(&self) -> Result<Paths> {
        match &self.home {
            Some(dir) => Ok(Paths::with_root(dir.clone())),
            None => Paths::resolve(),
        }
    }

    /// Tracing filter implied by `-v` flags, unless `RUST_LOG` says otherwise.
    pub fn log_filter(&self) -> String {
        std::env::var("RUST_LOG").unwrap_or_else(|_| {
            match self.verbose {
                0 => "warn",
                1 => "info",
                2 => "debug",
                _ => "trace",
            }
            .to_string()
        })
    }
}

/// Top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the ContextD home and database.
    Init(commands::project::InitArgs),

    /// Attach the current directory (or --path) as a project.
    Attach(commands::project::AttachArgs),

    /// Stop tracking a project.
    Detach(commands::project::DetachArgs),

    /// List projects.
    List(commands::project::ListArgs),

    /// Show the state of the current project.
    Status(commands::project::StatusArgs),

    /// Record a memory.
    Add(commands::memory::AddArgs),

    /// Change a memory.
    Edit(commands::memory::EditArgs),

    /// Delete a memory.
    Delete(commands::memory::DeleteArgs),

    /// Show one memory in full.
    Show(commands::memory::ShowArgs),

    /// List stored memories.
    #[command(visible_alias = "ls")]
    Memories(commands::memory::ListMemoriesArgs),

    /// Mark one memory as replaced by another.
    Supersede(commands::memory::SupersedeArgs),

    /// Keyword search across memories, decisions and checkpoints.
    Search(commands::search::SearchArgs),

    /// Ask a question; answers come from hybrid semantic retrieval.
    Recall(commands::search::RecallArgs),

    /// Record where you are in the work.
    Checkpoint(commands::checkpoint::CheckpointArgs),

    /// Print context for picking the work back up.
    Resume(commands::checkpoint::ResumeArgs),

    /// Working sessions: who worked on this project, when, and what came of it.
    #[command(subcommand)]
    Session(commands::session::SessionCommand),

    /// Architecture decision records.
    #[command(subcommand)]
    Decision(commands::decision::DecisionCommand),

    /// Tidy memory: merge duplicates, mark history, rebuild indexes.
    Refresh(commands::maintenance::RefreshArgs),

    /// Write the Markdown mirror and bound agent files.
    Sync(commands::maintenance::SyncArgs),

    /// Exchange memory with another machine over SSH.
    #[command(subcommand)]
    Remote(commands::remote::RemoteCommand),

    /// Move memory in and out as a JSON bundle.
    #[command(subcommand)]
    Bundle(commands::remote::BundleCommand),

    /// Import context from an agent's files.
    Import(commands::agent::ImportArgs),

    /// Export context to an agent's files.
    Export(commands::agent::ExportArgs),

    /// Model Context Protocol server.
    #[command(subcommand)]
    Mcp(commands::mcp::McpCommand),

    /// Summarise what this machine's store holds.
    Inventory(commands::remote::InventoryArgs),

    /// Show configuration and paths.
    Config(commands::config::ConfigArgs),
}

/// Parse a comma-separated list of categories.
pub fn parse_categories(values: &[String]) -> Result<Vec<Category>> {
    let mut out = Vec::new();
    for value in values {
        for part in value.split(',').filter(|p| !p.trim().is_empty()) {
            out.push(part.parse::<Category>()?);
        }
    }
    Ok(out)
}

/// Parse a comma-separated list of statuses.
pub fn parse_statuses(values: &[String]) -> Result<Vec<Status>> {
    let mut out = Vec::new();
    for value in values {
        for part in value.split(',').filter(|p| !p.trim().is_empty()) {
            out.push(part.parse::<Status>()?);
        }
    }
    Ok(out)
}

/// Run the parsed command.
pub async fn dispatch(cli: Cli) -> Result<()> {
    let paths = cli.global.paths()?;

    // `init` is the one command that may create the home directory.
    let app = match &cli.command {
        Command::Init(_) => App::open_or_create(paths)?,
        _ => App::open(paths)?,
    };
    let app = match std::env::current_dir() {
        Ok(cwd) => app.with_cwd(cwd),
        Err(_) => app,
    };

    let global = &cli.global;
    match cli.command {
        Command::Init(args) => commands::project::init(&app, global, &args),
        Command::Attach(args) => commands::project::attach(&app, global, &args),
        Command::Detach(args) => commands::project::detach(&app, global, &args),
        Command::List(args) => commands::project::list(&app, global, &args),
        Command::Status(args) => commands::project::status(&app, global, &args).await,
        Command::Add(args) => commands::memory::add(&app, global, &args).await,
        Command::Edit(args) => commands::memory::edit(&app, global, &args).await,
        Command::Delete(args) => commands::memory::delete(&app, global, &args).await,
        Command::Show(args) => commands::memory::show(&app, global, &args),
        Command::Memories(args) => commands::memory::list(&app, global, &args),
        Command::Supersede(args) => commands::memory::supersede(&app, global, &args),
        Command::Search(args) => commands::search::search(&app, global, &args).await,
        Command::Recall(args) => commands::search::recall(&app, global, &args).await,
        Command::Checkpoint(args) => commands::checkpoint::checkpoint(&app, global, &args).await,
        Command::Resume(args) => commands::checkpoint::resume(&app, global, &args).await,
        Command::Session(command) => commands::session::run(&app, global, command),
        Command::Decision(command) => commands::decision::run(&app, global, command),
        Command::Refresh(args) => commands::maintenance::refresh(&app, global, &args).await,
        Command::Sync(args) => commands::maintenance::sync(&app, global, &args).await,
        Command::Remote(command) => commands::remote::run_remote(&app, global, command).await,
        Command::Bundle(command) => commands::remote::run_bundle(&app, global, command).await,
        Command::Import(args) => commands::agent::import(&app, global, &args),
        Command::Export(args) => commands::agent::export(&app, global, &args).await,
        Command::Mcp(command) => commands::mcp::run(&app, global, command).await,
        Command::Inventory(args) => commands::remote::inventory(&app, global, &args),
        Command::Config(args) => commands::config::run(&app, global, &args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_a_typical_invocation() {
        let cli = Cli::try_parse_from([
            "contextd",
            "add",
            "--category",
            "architecture",
            "GPU scheduler uses NATS for task transport",
        ])
        .unwrap();
        match cli.command {
            Command::Add(args) => {
                assert_eq!(args.category, Category::Architecture);
                assert_eq!(args.content.join(" "), "GPU scheduler uses NATS for task transport");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn global_flags_are_accepted_after_the_subcommand() {
        let cli =
            Cli::try_parse_from(["contextd", "search", "scheduler", "--project", "ferrogrid"])
                .unwrap();
        assert_eq!(cli.global.project.as_deref(), Some("ferrogrid"));
    }

    #[test]
    fn category_and_status_lists_parse() {
        let categories =
            parse_categories(&["architecture,decision".to_string(), "task".to_string()]).unwrap();
        assert_eq!(categories.len(), 3);
        assert!(parse_categories(&["nonsense".to_string()]).is_err());

        let statuses = parse_statuses(&["active,archived".to_string()]).unwrap();
        assert_eq!(statuses, vec![Status::Active, Status::Archived]);
    }

    #[test]
    fn log_filter_follows_verbosity() {
        let quiet = GlobalArgs::default();
        // RUST_LOG, when set in the environment, always wins.
        if std::env::var("RUST_LOG").is_err() {
            assert_eq!(quiet.log_filter(), "warn");
            let loud = GlobalArgs { verbose: 2, ..Default::default() };
            assert_eq!(loud.log_filter(), "debug");
        }
    }
}
