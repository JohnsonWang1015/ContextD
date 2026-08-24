//! `contextd mcp serve` and `contextd mcp tools`.

use clap::{Args, Subcommand};

use crate::app::App;
use crate::cli::{output, GlobalArgs};
use crate::error::Result;
use crate::mcp::{protocol, McpServer, ServerOptions};
use crate::ui;

/// MCP subcommands.
#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Serve ContextD over MCP on stdio.
    Serve(ServeArgs),
    /// List the tools this server exposes.
    Tools(ToolsArgs),
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Refuse tools that write to memory.
    #[arg(long)]
    pub read_only: bool,
}

#[derive(Debug, Args)]
pub struct ToolsArgs {
    /// Show the tools available in read-only mode.
    #[arg(long)]
    pub read_only: bool,
}

/// Dispatch an MCP subcommand.
pub async fn run(app: &App, global: &GlobalArgs, command: McpCommand) -> Result<()> {
    match command {
        McpCommand::Serve(args) => serve(app, global, &args).await,
        McpCommand::Tools(args) => tools(global, &args),
    }
}

async fn serve(app: &App, global: &GlobalArgs, args: &ServeArgs) -> Result<()> {
    // The project is resolved once, from where the server was started, and used
    // whenever a tool call does not name one.
    let default_project = match global.project.clone() {
        Some(project) => Some(project),
        None => app.resolve_project(None)?.map(|p| p.slug),
    };

    let server =
        McpServer::new(app.clone(), ServerOptions { default_project, read_only: args.read_only });
    server.serve_stdio().await
}

fn tools(global: &GlobalArgs, args: &ToolsArgs) -> Result<()> {
    let specs = crate::mcp::tools::specs(args.read_only);
    output::render(global, &specs, || {
        let mut text = format!("{}\n", ui::header("MCP tools"));
        for spec in &specs {
            text.push_str(&format!("{}\n", ui::bold(spec.name)));
            for line in textwrap(spec.description, 76) {
                text.push_str(&format!("  {}\n", ui::dim(&line)));
            }
            text.push('\n');
        }
        text.push_str(&ui::hint(&format!(
            "Protocol {} · start with `contextd mcp serve`",
            protocol::PROTOCOL_VERSION
        )));
        text
    })
}

/// Wrap text at `width` on whitespace, collapsing the runs of spaces that
/// come from multi-line string literals.
fn textwrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + word.chars().count() + 1 > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}
