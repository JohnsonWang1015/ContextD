//! `remote` (add/list/remove/pull/push) and `bundle` (export/import).
//!
//! `bundle` is the machine-facing half: `remote pull` runs it over SSH on the
//! other side. It is a normal command rather than a hidden one so the same
//! exchange can be done by hand — `contextd bundle export > memory.json`,
//! carried on a USB stick, `contextd bundle import --file memory.json`.

use std::io::Read;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::app::App;
use crate::cli::{output, GlobalArgs};
use crate::config::RemoteConfig;
use crate::error::{Error, Result};
use crate::search::IndexService;
use crate::storage::repository::ProjectScope;
use crate::sync::bundle::{self, Bundle, BundleOptions, MergeReport};
use crate::sync::remote::{transport_for, RemoteSync, TransferOptions, TransferReport};
use crate::ui;

/// Machines ContextD can exchange memory with.
#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// Remember a machine to sync with.
    Add(AddRemoteArgs),
    /// List configured machines.
    List,
    /// Forget a machine.
    Remove(RemoveRemoteArgs),
    /// Fetch a machine's memory and merge it into this one.
    Pull(TransferArgs),
    /// Send this machine's memory to another and merge it there.
    Push(TransferArgs),
}

#[derive(Debug, Args)]
pub struct AddRemoteArgs {
    /// Short name, e.g. `lab`.
    pub name: String,

    /// SSH destination: `user@host`, or a Host alias from ~/.ssh/config.
    pub host: String,

    /// How to invoke ContextD there (default: `contextd`).
    #[arg(long, default_value = "contextd")]
    pub command: String,

    /// CONTEXTD_HOME on that machine, if it is not the default.
    ///
    /// Named `--remote-home` rather than `--home`, which is the global flag
    /// selecting *this* machine's store.
    #[arg(long = "remote-home", value_name = "PATH")]
    pub remote_home: Option<String>,

    /// Extra ssh arguments, e.g. --ssh-option=-p --ssh-option=2222.
    #[arg(long = "ssh-option", value_name = "ARG")]
    pub ssh_options: Vec<String>,
}

#[derive(Debug, Args)]
pub struct RemoveRemoteArgs {
    /// Name of the remote to forget.
    pub name: String,
}

#[derive(Debug, Args)]
pub struct TransferArgs {
    /// Which configured remote to talk to.
    pub remote: String,

    /// Limit to one project (default: everything).
    #[arg(long, value_name = "NAME")]
    pub only_project: Option<String>,

    /// Only records changed at or after this RFC3339 timestamp.
    #[arg(long, value_name = "TIMESTAMP")]
    pub since: Option<String>,

    /// Report what would change without writing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Leave checkpoints behind; exchange memories and decisions only.
    #[arg(long)]
    pub no_checkpoints: bool,

    /// Skip the embedding pass that normally follows a pull.
    #[arg(long)]
    pub skip_embeddings: bool,
}

/// Move memory in and out as JSON.
#[derive(Debug, Subcommand)]
pub enum BundleCommand {
    /// Write a JSON bundle of this machine's memory to stdout or a file.
    Export(BundleExportArgs),
    /// Merge a JSON bundle into this machine.
    Import(BundleImportArgs),
}

#[derive(Debug, Args)]
pub struct BundleExportArgs {
    /// Limit to one project.
    #[arg(long, value_name = "NAME")]
    pub project: Option<String>,

    /// Only records changed at or after this RFC3339 timestamp.
    #[arg(long, value_name = "TIMESTAMP")]
    pub since: Option<String>,

    /// Leave checkpoints out.
    #[arg(long)]
    pub no_checkpoints: bool,

    /// Write to a file instead of stdout.
    #[arg(short, long, value_name = "PATH")]
    pub out: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct BundleImportArgs {
    /// Read the bundle from this file.
    #[arg(short, long, value_name = "PATH", conflicts_with = "stdin")]
    pub file: Option<PathBuf>,

    /// Read the bundle from standard input.
    #[arg(long)]
    pub stdin: bool,

    /// Report what would change without writing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip the embedding pass that normally follows an import.
    #[arg(long)]
    pub skip_embeddings: bool,
}

/// Dispatch a `remote` subcommand.
pub async fn run_remote(app: &App, global: &GlobalArgs, command: RemoteCommand) -> Result<()> {
    match command {
        RemoteCommand::Add(args) => add(app, global, &args),
        RemoteCommand::List => list(app, global),
        RemoteCommand::Remove(args) => remove(app, global, &args),
        RemoteCommand::Pull(args) => transfer(app, global, &args, Direction::Pull).await,
        RemoteCommand::Push(args) => transfer(app, global, &args, Direction::Push).await,
    }
}

/// Dispatch a `bundle` subcommand.
pub async fn run_bundle(app: &App, global: &GlobalArgs, command: BundleCommand) -> Result<()> {
    match command {
        BundleCommand::Export(args) => export(app, global, &args),
        BundleCommand::Import(args) => import(app, global, &args).await,
    }
}

fn add(app: &App, global: &GlobalArgs, args: &AddRemoteArgs) -> Result<()> {
    let remote = RemoteConfig {
        name: args.name.trim().to_string(),
        host: args.host.trim().to_string(),
        command: args.command.clone(),
        home: args.remote_home.clone(),
        ssh_options: args.ssh_options.clone(),
    };
    remote.validate()?;

    // Config is the source of truth for remotes, so it is read fresh, amended
    // and written back: the in-memory copy may be older than the file.
    let path = app.paths().config_file();
    let mut config = crate::Config::load(&path)?;
    let replaced = config.remote(&remote.name).is_some();
    config.upsert_remote(remote.clone())?;
    config.save(&path)?;

    output::render(global, &remote, || {
        let verb = if replaced { "Updated" } else { "Added" };
        format!(
            "{}\n{}\n\n{}",
            ui::ok(&format!("{verb} remote {}", ui::bold(&remote.name))),
            ui::kv(&[
                ("host", remote.host.clone()),
                ("command", remote.command.clone()),
                ("home", remote.home.clone().unwrap_or_else(|| "default".into())),
            ]),
            ui::hint(&format!(
                "Try `contextd remote pull {} --dry-run` to check the connection.",
                remote.name
            ))
        )
    })
}

fn list(app: &App, global: &GlobalArgs) -> Result<()> {
    let config = crate::Config::load(&app.paths().config_file())?;
    output::render(global, &config.remotes, || {
        if config.remotes.is_empty() {
            return format!(
                "{}\n{}",
                ui::dim("No remotes configured."),
                ui::hint("Add one with `contextd remote add lab user@host`.")
            );
        }
        let rows: Vec<Vec<String>> = config
            .remotes
            .iter()
            .map(|remote| {
                vec![
                    remote.name.clone(),
                    remote.host.clone(),
                    remote.command.clone(),
                    ui::dim(&remote.home.clone().unwrap_or_else(|| "default home".into())),
                ]
            })
            .collect();
        ui::table(&["name", "host", "command", "home"], &rows)
    })
}

fn remove(app: &App, global: &GlobalArgs, args: &RemoveRemoteArgs) -> Result<()> {
    let path = app.paths().config_file();
    let mut config = crate::Config::load(&path)?;
    if !config.remove_remote(&args.name) {
        return Err(Error::invalid("remote", format!("no remote named `{}`", args.name)));
    }
    config.save(&path)?;

    #[derive(Serialize)]
    struct Removed<'a> {
        removed: &'a str,
    }
    output::render(global, &Removed { removed: &args.name }, || {
        ui::ok(&format!("Removed remote {}.", args.name))
    })
}

enum Direction {
    Pull,
    Push,
}

async fn transfer(
    app: &App,
    global: &GlobalArgs,
    args: &TransferArgs,
    direction: Direction,
) -> Result<()> {
    let transport = transport_for(app, &args.remote)?;

    // For a push the project must be resolved locally; for a pull the name is
    // passed through and resolved on the other machine.
    let project = match (&args.only_project, &direction) {
        (Some(name), Direction::Push) => Some(app.lookup_project(name)?.id),
        (Some(name), Direction::Pull) => Some(name.clone()),
        (None, _) => None,
    };

    let options = TransferOptions {
        project,
        since: parse_since(args.since.as_deref())?,
        dry_run: args.dry_run,
        include_checkpoints: !args.no_checkpoints,
    };

    let sync = RemoteSync::new(app);
    let report = match direction {
        Direction::Pull => sync.pull(&transport, &options)?,
        Direction::Push => sync.push(&transport, &options)?,
    };

    // Vectors are never shipped; what just arrived has to be embedded here.
    let embedded = if matches!(direction, Direction::Pull)
        && !args.dry_run
        && !args.skip_embeddings
        && report.merge.written() > 0
    {
        IndexService::new(app).embed_pending(&ProjectScope::Any, false).await?.embedded
    } else {
        0
    };

    output::render(global, &report, || render_transfer(&report, embedded))
}

fn export(app: &App, global: &GlobalArgs, args: &BundleExportArgs) -> Result<()> {
    let project_id = match &args.project {
        Some(name) => Some(app.lookup_project(name)?.id),
        None => None,
    };
    let bundle = bundle::build(
        app,
        &BundleOptions {
            project_id,
            since: parse_since(args.since.as_deref())?,
            include_checkpoints: !args.no_checkpoints,
            include_global: true,
        },
    )?;

    let json = bundle.to_json()?;
    match &args.out {
        Some(path) => {
            crate::sync::write_atomic(path, &format!("{json}\n"))?;
            // Never print the bundle as well: with --out the caller wants the
            // file, and a summary is what is useful on the terminal.
            output::render(global, &Summary::from(&bundle), || {
                format!(
                    "{}\n{}",
                    ui::ok(&format!("Wrote {}", path.display())),
                    ui::kv(&[
                        ("projects", bundle.projects.len().to_string()),
                        ("memories", bundle.memories.len().to_string()),
                        ("decisions", bundle.decisions.len().to_string()),
                        ("checkpoints", bundle.checkpoints.len().to_string()),
                    ])
                )
            })
        }
        // stdout carries the bundle itself: this is what the far side of an
        // SSH pipe reads, so nothing else may be written here.
        None => {
            println!("{json}");
            Ok(())
        }
    }
}

#[derive(Serialize)]
struct Summary {
    projects: usize,
    memories: usize,
    decisions: usize,
    checkpoints: usize,
}

impl From<&Bundle> for Summary {
    fn from(bundle: &Bundle) -> Self {
        Self {
            projects: bundle.projects.len(),
            memories: bundle.memories.len(),
            decisions: bundle.decisions.len(),
            checkpoints: bundle.checkpoints.len(),
        }
    }
}

async fn import(app: &App, global: &GlobalArgs, args: &BundleImportArgs) -> Result<()> {
    let text = match (&args.file, args.stdin) {
        (Some(path), _) => std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?,
        (None, true) => {
            let mut buffer = String::new();
            std::io::stdin().read_to_string(&mut buffer)?;
            buffer
        }
        (None, false) => {
            return Err(Error::invalid("input", "pass --file <PATH> or --stdin"));
        }
    };

    let bundle = Bundle::from_json(text.trim())?;
    let report = bundle::merge(app, &bundle, args.dry_run)?;

    let embedded = if !args.dry_run && !args.skip_embeddings && report.written() > 0 {
        IndexService::new(app).embed_pending(&ProjectScope::Any, false).await?.embedded
    } else {
        0
    };

    // `remote push` parses this JSON on the other side, so the machine-readable
    // form must be exactly the merge report.
    if global.json {
        println!("{}", serde_json::to_string(&report)?);
        return Ok(());
    }
    println!("{}", render_merge(&report, embedded));
    Ok(())
}

fn render_transfer(report: &TransferReport, embedded: usize) -> String {
    let mut text =
        format!("{}\n", ui::header(&format!("{} {}", capitalise(report.direction), report.remote)));
    text.push_str(&render_merge(&report.merge, embedded));
    text.push_str(&format!(
        "\n{}",
        ui::dim(&format!("  {} records examined", report.records_transferred))
    ));
    text
}

fn render_merge(report: &MergeReport, embedded: usize) -> String {
    let mut text = ui::kv(&[
        (
            "Memories",
            format!(
                "{} new, {} updated, {} unchanged",
                report.memories_added, report.memories_updated, report.memories_unchanged
            ),
        ),
        (
            "Decisions",
            format!("{} new, {} updated", report.decisions_added, report.decisions_updated),
        ),
        ("Checkpoints", format!("{} new", report.checkpoints_added)),
        (
            "Projects",
            if report.projects_created.is_empty() {
                format!("{} matched", report.projects_matched.len())
            } else {
                format!(
                    "{} matched, created {}",
                    report.projects_matched.len(),
                    report.projects_created.join(", ")
                )
            },
        ),
    ]);

    if embedded > 0 {
        text.push_str(&format!("\n{}", ui::kv(&[("Embedded", embedded.to_string())])));
    }

    if !report.conflicts.is_empty() {
        text.push_str(&format!(
            "\n\n{}\n",
            ui::warn(&format!(
                "{} record(s) changed on both sides; the local copy was kept:",
                report.conflicts.len()
            ))
        ));
        for conflict in report.conflicts.iter().take(10) {
            text.push_str(&format!(
                "  {} {} {}\n",
                ui::dim(&conflict.kind),
                crate::util::ids::short(&conflict.id),
                conflict.title
            ));
        }
        text.push_str(&ui::hint(
            "Compare with `contextd show <id>`; keep both by recording the other side as a new memory.",
        ));
    }

    if report.dry_run {
        text.push_str(&format!("\n\n{}", ui::dim("  dry run: nothing was written")));
    }
    text
}

fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Parse `--since`, accepting a date as well as a full timestamp.
fn parse_since(value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(Some(parsed.with_timezone(&Utc)));
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        let midnight = date.and_hms_opt(0, 0, 0).expect("midnight is a valid time");
        return Ok(Some(DateTime::from_naive_utc_and_offset(midnight, Utc)));
    }
    Err(Error::invalid(
        "since",
        format!("`{value}` is not a date (2026-08-24) or an RFC3339 timestamp"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_accepts_dates_and_timestamps() {
        assert!(parse_since(None).unwrap().is_none());
        assert!(parse_since(Some("  ")).unwrap().is_none());

        let date = parse_since(Some("2026-08-24")).unwrap().unwrap();
        assert_eq!(crate::util::time::to_storage(&date), "2026-08-24T00:00:00.000Z");

        let stamp = parse_since(Some("2026-08-24T12:30:00Z")).unwrap().unwrap();
        assert_eq!(stamp.timestamp(), date.timestamp() + 12 * 3600 + 1800);

        assert!(parse_since(Some("last tuesday")).is_err());
    }

    #[test]
    fn capitalise_handles_empty() {
        assert_eq!(capitalise("pull"), "Pull");
        assert_eq!(capitalise(""), "");
    }
}
