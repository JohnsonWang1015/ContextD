//! `contextd` — command-line entry point.
//!
//! The binary is deliberately thin: parse arguments, set up logging, hand off
//! to [`contextd::cli::dispatch`], and turn errors into a readable message and
//! an exit code.

use std::process::ExitCode;

use clap::Parser;

use contextd::cli::{dispatch, Cli};
use contextd::ui;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(&cli);
    ui::init_color(&color_preference(&cli), cli.global.no_color);

    // A current-thread runtime starts in well under a millisecond, which keeps
    // `contextd status` feeling instant; the MCP server does not need work
    // stealing either, since it is I/O bound on one stdio stream.
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("{}", ui::error(&format!("cannot start async runtime: {err}")));
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(dispatch(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{}", ui::error(&err.to_string()));
            for (depth, source) in sources(&err).enumerate() {
                eprintln!("{}", ui::dim(&format!("  {}└ {source}", "  ".repeat(depth))));
            }
            ExitCode::FAILURE
        }
    }
}

/// Logs go to stderr so they can never corrupt MCP's stdout stream.
fn init_tracing(cli: &Cli) {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter =
        EnvFilter::try_new(cli.global.log_filter()).unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .try_init();
}

/// Colour preference: the flag wins, then the config file if it can be read,
/// then automatic detection.
fn color_preference(cli: &Cli) -> String {
    if cli.global.no_color {
        return "never".to_string();
    }
    cli.global
        .paths()
        .ok()
        .and_then(|paths| contextd::Config::load(&paths.config_file()).ok())
        .map(|config| config.general.color)
        .unwrap_or_else(|| "auto".to_string())
}

/// Walk the error chain for context lines.
fn sources(error: &dyn std::error::Error) -> impl Iterator<Item = String> + '_ {
    let mut current = error.source();
    std::iter::from_fn(move || {
        let source = current?;
        current = source.source();
        Some(source.to_string())
    })
}
