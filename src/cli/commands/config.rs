//! `config` — show where things live and what is configured.

use clap::Args;
use serde::Serialize;

use crate::app::App;
use crate::cli::{output, GlobalArgs};
use crate::error::Result;
use crate::storage::sqlite::migrations;
use crate::ui;

/// `contextd config`
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// Print the config file path only.
    #[arg(long)]
    pub path: bool,

    /// Print the config file contents as TOML.
    #[arg(long)]
    pub toml: bool,
}

/// Show configuration.
pub fn run(app: &App, global: &GlobalArgs, args: &ConfigArgs) -> Result<()> {
    let paths = app.paths();
    if args.path {
        println!("{}", paths.config_file().display());
        return Ok(());
    }
    if args.toml {
        let text = toml::to_string_pretty(app.config())
            .map_err(|e| crate::error::Error::Config(e.to_string()))?;
        print!("{text}");
        return Ok(());
    }

    #[derive(Serialize)]
    struct ConfigOutput<'a> {
        root: String,
        database: String,
        config_file: String,
        schema_version: i64,
        config: &'a crate::config::Config,
    }

    let out = ConfigOutput {
        root: paths.root().display().to_string(),
        database: paths.database().display().to_string(),
        config_file: paths.config_file().display().to_string(),
        schema_version: app.store().schema_version()?,
        config: app.config(),
    };

    output::render(global, &out, || {
        let config = app.config();
        let mut text = format!("{}\n", ui::header("ContextD configuration"));
        text.push_str(&ui::kv(&[
            ("Home", out.root.clone()),
            ("Database", out.database.clone()),
            ("Config", out.config_file.clone()),
            (
                "Schema",
                format!("v{} (latest v{})", out.schema_version, migrations::target_version()),
            ),
            ("Embeddings", format!("{} · {}", config.embeddings.provider, config.embeddings.model)),
            (
                "Context budget",
                format!(
                    "{} tokens, {} memories",
                    config.context.max_context_tokens, config.context.max_memories
                ),
            ),
            ("Default agent", config.general.default_agent.clone()),
        ]));
        text.push_str(&format!(
            "\n\n{}",
            ui::hint(
                "Edit the config file to change any of this; `contextd config --toml` prints it."
            )
        ));
        text
    })
}
