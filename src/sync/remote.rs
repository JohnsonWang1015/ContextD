//! Exchanging memory with other machines over SSH.
//!
//! ContextD does not copy the database file between machines: two laptops that
//! both recorded something since the last exchange must both keep their work,
//! and a file copy can only pick a winner. Instead each side speaks its own
//! `contextd bundle` command over an SSH connection, and the records are
//! merged by id (see [`crate::sync::bundle`]).
//!
//! ```text
//! contextd remote pull lab
//!   ssh dev@lab-box 'contextd bundle export --json'   → bundle → merge here
//!
//! contextd remote push lab
//!   bundle here → ssh dev@lab-box 'contextd bundle import --stdin'
//! ```
//!
//! Nothing runs a remote shell string built by concatenation: every argument
//! is quoted, so a project name with a space or a quote in it cannot become a
//! command.

use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};

use serde::Serialize;

use crate::app::App;
use crate::config::RemoteConfig;
use crate::core::inventory::Inventory;
use crate::error::{Error, Result};
use crate::sync::bundle::{self, Bundle, BundleOptions, MergeReport};

/// Runs ContextD somewhere else.
///
/// The trait exists so the pull/push logic can be tested without an SSH
/// daemon, and so another transport (a container, a different protocol) can be
/// added without touching the merge code.
pub trait RemoteTransport: Send + Sync {
    /// Human-facing description, used in reports and errors.
    fn describe(&self) -> String;

    /// The SSH destination, when there is one, so errors can suggest the exact
    /// command that fixes them.
    fn destination(&self) -> Option<&str> {
        None
    }

    /// Run `contextd <args>` on the remote machine and return its stdout.
    fn run(&self, args: &[String], stdin: Option<&str>) -> Result<String>;
}

/// How `ssh` should behave when the connection needs something from a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Interaction {
    /// Prompt when there is a terminal to prompt on, fail fast when there is
    /// not. This is what a person at a shell wants and what cron needs.
    #[default]
    Auto,
    /// Always allow prompting: passwords, host-key confirmation, 2FA.
    Interactive,
    /// Never prompt. A missing key becomes an immediate error rather than a
    /// script that hangs waiting for a password nobody will type.
    Batch,
}

impl Interaction {
    /// Whether ssh may ask the user something.
    fn allows_prompting(self) -> bool {
        match self {
            Interaction::Interactive => true,
            Interaction::Batch => false,
            // stdin being a terminal is the practical test for "somebody is
            // there": a cron job, a pipeline or an MCP server has none.
            Interaction::Auto => std::io::stdin().is_terminal(),
        }
    }
}

/// Transport that shells out to `ssh`.
#[derive(Debug, Clone)]
pub struct SshTransport {
    config: RemoteConfig,
    interaction: Interaction,
}

impl SshTransport {
    pub fn new(config: RemoteConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self { config, interaction: Interaction::default() })
    }

    /// Choose whether ssh may prompt.
    pub fn with_interaction(mut self, interaction: Interaction) -> Self {
        self.interaction = interaction;
        self
    }

    /// Whether this transport will let ssh ask for a password.
    pub fn is_interactive(&self) -> bool {
        self.interaction.allows_prompting()
    }

    /// Arguments passed to `ssh`, before the remote command.
    ///
    /// Split out because this is the part worth testing: whether a password
    /// prompt is possible comes down to one flag being present or absent.
    pub fn ssh_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.interaction.allows_prompting() {
            // Passwords and host-key confirmations are read straight from the
            // terminal by ssh, so they still work while ContextD captures the
            // command's stdout and stderr. Three attempts is ssh's default;
            // stating it keeps a typo from turning into an endless loop.
            args.push("-o".into());
            args.push("NumberOfPasswordPrompts=3".into());
        } else {
            args.push("-o".into());
            args.push("BatchMode=yes".into());
        }
        // Long enough for a sleepy laptop, short enough that an unreachable
        // host is reported rather than waited on.
        args.push("-o".into());
        args.push("ConnectTimeout=15".into());
        args.extend(self.config.ssh_options.iter().cloned());
        args
    }

    /// The command string handed to the remote shell.
    ///
    /// Exposed for tests: quoting is the security-relevant part of this file.
    pub fn remote_command(&self, args: &[String]) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(home) = self.config.home.as_ref().filter(|h| !h.trim().is_empty()) {
            parts.push(format!("CONTEXTD_HOME={}", shell_quote(home)));
        }
        parts.push(shell_quote(&self.config.command));
        parts.extend(args.iter().map(|arg| shell_quote(arg)));
        parts.join(" ")
    }
}

impl RemoteTransport for SshTransport {
    fn describe(&self) -> String {
        if self.config.name == self.config.host {
            self.config.host.clone()
        } else {
            format!("{} ({})", self.config.name, self.config.host)
        }
    }

    fn destination(&self) -> Option<&str> {
        Some(&self.config.host)
    }

    fn run(&self, args: &[String], stdin: Option<&str>) -> Result<String> {
        let mut command = Command::new("ssh");
        command.args(self.ssh_args());
        command.arg(&self.config.host);
        command.arg(self.remote_command(args));
        command
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|err| {
            Error::Other(anyhow::anyhow!("cannot run ssh for remote `{}`: {err}", self.config.name))
        })?;

        if let Some(input) = stdin {
            let mut handle = child.stdin.take().expect("stdin was piped");
            // The remote may exit early (bad flag, old binary); a broken pipe
            // here is not the interesting error, the exit status is.
            let _ = handle.write_all(input.as_bytes());
            drop(handle);
        }

        let output = child.wait_with_output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Other(anyhow::anyhow!(
                "remote `{}` failed ({}): {}",
                self.config.name,
                output.status,
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Quote one argument for a POSIX remote shell.
fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '=' | ':'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// What to exchange.
#[derive(Debug, Clone, Default)]
pub struct TransferOptions {
    /// Limit to one project (local id for push, name for pull).
    pub project: Option<String>,
    /// Only records changed at or after this instant.
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    /// Report what would change without writing.
    pub dry_run: bool,
    pub include_checkpoints: bool,
}

/// Outcome of a pull or a push.
#[derive(Debug, Clone, Serialize)]
pub struct TransferReport {
    pub remote: String,
    pub direction: &'static str,
    pub records_transferred: usize,
    pub merge: MergeReport,
}

/// Pull and push against one remote.
pub struct RemoteSync<'a> {
    app: &'a App,
}

impl<'a> RemoteSync<'a> {
    pub fn new(app: &'a App) -> Self {
        Self { app }
    }

    /// Survey a machine without taking anything from it.
    ///
    /// The remote reports counts, not content, so finding out what an account
    /// holds costs a few kilobytes instead of its whole memory. It is also the
    /// step that says plainly whether ContextD is installed over there and
    /// where its home turned out to be, which is what makes `pull` predictable.
    pub fn scan(&self, transport: &dyn RemoteTransport) -> Result<Inventory> {
        let stdout = transport
            .run(&["inventory".to_string(), "--json".to_string()], None)
            .map_err(|err| explain_remote_failure(transport, err))?;

        Inventory::from_json(stdout.trim()).map_err(|err| {
            Error::Other(anyhow::anyhow!(
                "{} did not return an inventory ({err}). Older builds do not have \
                 `contextd inventory`; upgrade contextd there, or use \
                 `contextd remote pull <name> --dry-run` instead.",
                transport.describe()
            ))
        })
    }

    /// Fetch the remote's memory and merge it into this machine.
    pub fn pull(
        &self,
        transport: &dyn RemoteTransport,
        options: &TransferOptions,
    ) -> Result<TransferReport> {
        let mut args = vec!["bundle".to_string(), "export".to_string()];
        if let Some(project) = &options.project {
            args.push("--project".to_string());
            args.push(project.clone());
        }
        if let Some(since) = options.since {
            args.push("--since".to_string());
            args.push(crate::util::time::to_storage(&since));
        }
        if !options.include_checkpoints {
            args.push("--no-checkpoints".to_string());
        }

        let stdout =
            transport.run(&args, None).map_err(|err| explain_remote_failure(transport, err))?;
        let bundle = Bundle::from_json(stdout.trim()).map_err(|err| {
            Error::Other(anyhow::anyhow!(
                "{} did not return a bundle ({err}); is contextd installed there?",
                transport.describe()
            ))
        })?;

        let merge = bundle::merge(self.app, &bundle, options.dry_run)?;
        Ok(TransferReport {
            remote: transport.describe(),
            direction: "pull",
            records_transferred: bundle.len(),
            merge,
        })
    }

    /// Send this machine's memory to the remote and merge it there.
    pub fn push(
        &self,
        transport: &dyn RemoteTransport,
        options: &TransferOptions,
    ) -> Result<TransferReport> {
        let bundle = bundle::build(
            self.app,
            &BundleOptions {
                project_id: options.project.clone(),
                since: options.since,
                include_checkpoints: options.include_checkpoints,
                include_global: true,
            },
        )?;

        let mut args = vec![
            "bundle".to_string(),
            "import".to_string(),
            "--stdin".to_string(),
            "--json".to_string(),
        ];
        if options.dry_run {
            args.push("--dry-run".to_string());
        }

        let stdout = transport.run(&args, Some(&bundle.to_json()?))?;
        // The remote answers with its own merge report; parsing it is what
        // makes `push` report real numbers rather than "probably fine".
        let merge: MergeReport = serde_json::from_str(stdout.trim()).map_err(|err| {
            Error::Other(anyhow::anyhow!(
                "{} did not return a merge report ({err}): {}",
                transport.describe(),
                stdout.trim()
            ))
        })?;

        Ok(TransferReport {
            remote: transport.describe(),
            direction: "push",
            records_transferred: bundle.len(),
            merge,
        })
    }
}

/// Turn a transport failure into something the user can act on.
///
/// The two failures worth naming are "no contextd over there" and "cannot get
/// there at all"; everything else is passed through as ssh reported it.
fn explain_remote_failure(transport: &dyn RemoteTransport, err: Error) -> Error {
    let text = err.to_string();
    if text.contains("Permission denied") || text.contains("Too many authentication failures") {
        return Error::Other(anyhow::anyhow!(
            "{} refused the login. If that account uses a password, run this from a terminal \
             so ssh can ask for it (or pass --interactive); to stop being asked every time, \
             `ssh-copy-id {}`.",
            transport.describe(),
            transport.destination().unwrap_or("user@host")
        ));
    }
    if text.contains("Host key verification failed") {
        return Error::Other(anyhow::anyhow!(
            "{} could not be verified. Connect once with `ssh {}` to check and accept its \
             fingerprint, then try again.",
            transport.describe(),
            transport.destination().unwrap_or("user@host")
        ));
    }
    if text.contains("command not found") || text.contains("No such file or directory") {
        return Error::Other(anyhow::anyhow!(
            "contextd is not on the PATH for {}. Install it there, or point at it with \
             `contextd remote add <name> <host> --command /path/to/contextd`.",
            transport.describe()
        ));
    }
    err
}

/// Look up a remote by name and build its transport.
pub fn transport_for(app: &App, name: &str) -> Result<SshTransport> {
    transport_with(app, name, Interaction::default())
}

/// Look up a remote by name, choosing how ssh may interact with the user.
pub fn transport_with(app: &App, name: &str, interaction: Interaction) -> Result<SshTransport> {
    let config = app.config().remote(name).cloned().ok_or_else(|| {
        Error::invalid(
            "remote",
            format!(
                "no remote named `{name}` (configured: {})",
                if app.config().remotes.is_empty() {
                    "none".to_string()
                } else {
                    app.config()
                        .remotes
                        .iter()
                        .map(|r| r.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ),
        )
    })?;
    Ok(SshTransport::new(config)?.with_interaction(interaction))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::core::memory::{MemoryService, NewMemory};
    use crate::core::model::{Category, Project};
    use crate::core::project::{AttachRequest, ProjectService};
    use std::sync::Mutex;

    /// A transport that runs against a second in-process [`App`], which is
    /// what an SSH hop amounts to once the connection is made.
    struct LoopbackTransport {
        peer: App,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl LoopbackTransport {
        fn new(peer: App) -> Self {
            Self { peer, calls: Mutex::new(Vec::new()) }
        }
    }

    impl RemoteTransport for LoopbackTransport {
        fn describe(&self) -> String {
            "loopback".into()
        }

        fn run(&self, args: &[String], stdin: Option<&str>) -> Result<String> {
            self.calls.lock().unwrap().push(args.to_vec());
            match args.first().map(String::as_str) {
                Some("inventory") => crate::core::inventory::collect(&self.peer)?.to_json(),
                Some("bundle") => match args.get(1).map(String::as_str) {
                    Some("export") => {
                        bundle::build(&self.peer, &BundleOptions::everything())?.to_json()
                    }
                    Some("import") => {
                        let bundle = Bundle::from_json(stdin.unwrap_or_default())?;
                        let dry_run = args.iter().any(|a| a == "--dry-run");
                        let report = bundle::merge(&self.peer, &bundle, dry_run)?;
                        Ok(serde_json::to_string(&report)?)
                    }
                    other => Err(Error::invalid("args", format!("unexpected: {other:?}"))),
                },
                other => Err(Error::invalid("args", format!("unexpected: {other:?}"))),
            }
        }
    }

    fn machine(name: &str, remote_url: &str) -> (tempfile::TempDir, App, Project) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let app =
            App::open_or_create(Paths::with_root(dir.path().join("home"))).unwrap().with_cwd(&repo);
        let (mut project, _) = ProjectService::new(&app)
            .attach(AttachRequest {
                dir: repo,
                name: Some(name.to_string()),
                description: None,
                bindings: vec![],
            })
            .unwrap();
        project.git_remote = Some(remote_url.to_string());
        app.store().update_project(&project).unwrap();
        (dir, app, project)
    }

    fn add(app: &App, project: &Project, content: &str) {
        MemoryService::new(app)
            .add(NewMemory {
                project: Some(project.clone()),
                ..NewMemory::new(Category::Architecture, content)
            })
            .unwrap();
    }

    #[test]
    fn pull_brings_remote_memory_home() {
        let url = "git@github.com:acme/FerroGrid.git";
        let (_lab_dir, lab, lab_project) = machine("FerroGrid", url);
        add(&lab, &lab_project, "Lab box measured heartbeat latency at 4ms");

        let (_local_dir, local, _local_project) = machine("FerroGrid", url);
        let transport = LoopbackTransport::new(lab.clone());
        let report = RemoteSync::new(&local).pull(&transport, &TransferOptions::default()).unwrap();

        assert_eq!(report.direction, "pull");
        assert_eq!(report.merge.memories_added, 1);
        let memories = MemoryService::new(&local).for_project(None, 50).unwrap();
        assert!(local
            .store()
            .list_memories(&Default::default())
            .unwrap()
            .iter()
            .any(|m| m.content.contains("4ms")));
        let _ = memories;
    }

    #[test]
    fn push_sends_local_memory_and_reports_the_remote_result() {
        let url = "git@github.com:acme/FerroGrid.git";
        let (_lab_dir, lab, _) = machine("FerroGrid", url);
        let (_local_dir, local, local_project) = machine("FerroGrid", url);
        add(&local, &local_project, "Laptop recorded the lease renewal interval");

        let transport = LoopbackTransport::new(lab.clone());
        let report = RemoteSync::new(&local).push(&transport, &TransferOptions::default()).unwrap();

        assert_eq!(report.direction, "push");
        assert_eq!(report.merge.memories_added, 1);
        assert!(lab
            .store()
            .list_memories(&Default::default())
            .unwrap()
            .iter()
            .any(|m| m.content.contains("lease renewal")));
    }

    #[test]
    fn a_pull_then_push_converges_both_machines() {
        let url = "git@github.com:acme/FerroGrid.git";
        let (_lab_dir, lab, lab_project) = machine("FerroGrid", url);
        let (_local_dir, local, local_project) = machine("FerroGrid", url);
        add(&lab, &lab_project, "Lab: GPU nodes are bare metal");
        add(&local, &local_project, "Laptop: scheduler transport is NATS");

        let transport = LoopbackTransport::new(lab.clone());
        let sync = RemoteSync::new(&local);
        sync.pull(&transport, &TransferOptions::default()).unwrap();
        sync.push(&transport, &TransferOptions::default()).unwrap();

        for app in [&local, &lab] {
            let contents: Vec<String> = app
                .store()
                .list_memories(&Default::default())
                .unwrap()
                .into_iter()
                .map(|m| m.content)
                .collect();
            assert_eq!(contents.len(), 2, "both machines end up with both memories");
            assert!(contents.iter().any(|c| c.contains("bare metal")));
            assert!(contents.iter().any(|c| c.contains("NATS")));
        }

        // Running it again changes nothing.
        let report = sync.pull(&transport, &TransferOptions::default()).unwrap();
        assert_eq!(report.merge.memories_added, 0);
    }

    #[test]
    fn dry_run_reaches_the_remote_but_writes_nothing() {
        let url = "git@github.com:acme/FerroGrid.git";
        let (_lab_dir, lab, lab_project) = machine("FerroGrid", url);
        add(&lab, &lab_project, "Lab memory");
        let (_local_dir, local, _) = machine("FerroGrid", url);

        let transport = LoopbackTransport::new(lab.clone());
        let report = RemoteSync::new(&local)
            .pull(&transport, &TransferOptions { dry_run: true, ..Default::default() })
            .unwrap();
        assert_eq!(report.merge.memories_added, 1);
        assert!(report.merge.dry_run);
        assert!(local.store().list_memories(&Default::default()).unwrap().is_empty());
    }

    #[test]
    fn scan_reports_what_the_other_machine_holds() {
        let url = "git@github.com:acme/FerroGrid.git";
        let (_lab_dir, lab, lab_project) = machine("FerroGrid", url);
        add(&lab, &lab_project, "Lab box measured heartbeat latency at 4ms");
        add(&lab, &lab_project, "GPU nodes are bare metal");

        let (_local_dir, local, _) = machine("FerroGrid", url);
        let transport = LoopbackTransport::new(lab.clone());
        let inventory = RemoteSync::new(&local).scan(&transport).unwrap();

        assert_eq!(inventory.totals.projects, 1);
        assert_eq!(inventory.totals.memories, 2);
        assert_eq!(inventory.projects[0].name, "FerroGrid");
        assert!(inventory.last_activity().is_some());

        // Surveying takes nothing: the local store is untouched.
        assert!(local.store().list_memories(&Default::default()).unwrap().is_empty());
        assert_eq!(transport.calls.lock().unwrap()[0], vec!["inventory", "--json"]);
    }

    #[test]
    fn scanning_a_machine_without_contextd_says_so() {
        struct Missing;
        impl RemoteTransport for Missing {
            fn describe(&self) -> String {
                "lab (dev@lab-box)".into()
            }
            fn run(&self, _args: &[String], _stdin: Option<&str>) -> Result<String> {
                Err(Error::Other(anyhow::anyhow!(
                    "remote `lab` failed (exit status: 127): bash: contextd: command not found"
                )))
            }
        }

        let (_dir, local, _) = machine("FerroGrid", "git@github.com:acme/FerroGrid.git");
        let err = RemoteSync::new(&local).scan(&Missing).unwrap_err().to_string();
        assert!(err.contains("not on the PATH"), "{err}");
        assert!(err.contains("--command"), "the fix must be in the message: {err}");
    }

    #[test]
    fn scanning_an_older_build_explains_the_gap() {
        struct OldBuild;
        impl RemoteTransport for OldBuild {
            fn describe(&self) -> String {
                "lab".into()
            }
            fn run(&self, _args: &[String], _stdin: Option<&str>) -> Result<String> {
                Ok("error: unrecognized subcommand 'inventory'".into())
            }
        }

        let (_dir, local, _) = machine("FerroGrid", "git@github.com:acme/FerroGrid.git");
        let err = RemoteSync::new(&local).scan(&OldBuild).unwrap_err().to_string();
        assert!(err.contains("did not return an inventory"), "{err}");
        assert!(err.contains("--dry-run"), "{err}");
    }

    #[test]
    fn a_remote_that_is_not_contextd_produces_a_clear_error() {
        struct Noise;
        impl RemoteTransport for Noise {
            fn describe(&self) -> String {
                "noise".into()
            }
            fn run(&self, _args: &[String], _stdin: Option<&str>) -> Result<String> {
                Ok("bash: contextd: command not found".into())
            }
        }

        let (_dir, local, _) = machine("FerroGrid", "git@github.com:acme/FerroGrid.git");
        let err = RemoteSync::new(&local)
            .pull(&Noise, &TransferOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not return a bundle"), "{err}");
        assert!(err.contains("is contextd installed"), "{err}");
    }

    #[test]
    fn batch_mode_is_what_decides_whether_a_password_can_be_typed() {
        let transport = SshTransport::new(RemoteConfig::new("lab", "johnson@140.123.105.18"))
            .unwrap()
            .with_interaction(Interaction::Batch);
        let args = transport.ssh_args();
        assert!(args.iter().any(|a| a == "BatchMode=yes"), "{args:?}");
        assert!(!transport.is_interactive());

        let transport = transport.with_interaction(Interaction::Interactive);
        let args = transport.ssh_args();
        assert!(
            !args.iter().any(|a| a == "BatchMode=yes"),
            "batch mode would suppress the password prompt: {args:?}"
        );
        assert!(args.iter().any(|a| a == "NumberOfPasswordPrompts=3"), "{args:?}");
        assert!(transport.is_interactive());
    }

    #[test]
    fn ssh_options_from_configuration_are_kept() {
        let transport = SshTransport::new(RemoteConfig {
            ssh_options: vec!["-p".into(), "2222".into()],
            ..RemoteConfig::new("lab", "johnson@140.123.105.18")
        })
        .unwrap();
        let args = transport.ssh_args();
        assert!(args.windows(2).any(|pair| pair == ["-p", "2222"]), "{args:?}");
        assert!(args.iter().any(|a| a == "ConnectTimeout=15"), "{args:?}");
    }

    #[test]
    fn an_ad_hoc_destination_describes_itself_by_host() {
        let transport = SshTransport::new(RemoteConfig::new(
            "johnson@140.123.105.18",
            "johnson@140.123.105.18",
        ))
        .unwrap();
        assert_eq!(transport.describe(), "johnson@140.123.105.18");
        assert_eq!(transport.destination(), Some("johnson@140.123.105.18"));
    }

    #[test]
    fn a_refused_login_suggests_how_to_get_in() {
        struct Refused;
        impl RemoteTransport for Refused {
            fn describe(&self) -> String {
                "johnson@140.123.105.18".into()
            }
            fn destination(&self) -> Option<&str> {
                Some("johnson@140.123.105.18")
            }
            fn run(&self, _args: &[String], _stdin: Option<&str>) -> Result<String> {
                Err(Error::Other(anyhow::anyhow!(
                    "remote failed (exit status: 255): johnson@140.123.105.18: Permission denied \
                     (publickey,password)."
                )))
            }
        }

        let (_dir, local, _) = machine("FerroGrid", "git@github.com:acme/FerroGrid.git");
        let err = RemoteSync::new(&local).scan(&Refused).unwrap_err().to_string();
        assert!(err.contains("--interactive"), "{err}");
        assert!(err.contains("ssh-copy-id johnson@140.123.105.18"), "{err}");
    }

    #[test]
    fn remote_commands_are_quoted() {
        let transport = SshTransport::new(RemoteConfig {
            home: Some("/srv/state dir/.contextd".into()),
            ..RemoteConfig::new("lab", "dev@lab-box")
        })
        .unwrap();

        let command = transport.remote_command(&[
            "bundle".into(),
            "export".into(),
            "--project".into(),
            "Ferro'; rm -rf /".into(),
        ]);
        assert!(
            command.starts_with("CONTEXTD_HOME='/srv/state dir/.contextd' contextd bundle export")
        );
        // The payload survives only in POSIX-escaped form: the closing quote,
        // an escaped quote, then the rest still inside quotes.
        assert!(command.contains(r"'Ferro'\''; rm -rf /'"), "{command}");
        assert!(
            !command.contains("Ferro'; rm"),
            "the quote was not escaped, so the shell would run it: {command}"
        );
    }

    #[test]
    fn shell_quoting_leaves_plain_arguments_alone() {
        assert_eq!(shell_quote("bundle"), "bundle");
        assert_eq!(shell_quote("--json"), "--json");
        assert_eq!(shell_quote("2026-08-24T00:00:00Z"), "2026-08-24T00:00:00Z");
        assert_eq!(shell_quote("has space"), "'has space'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn unknown_remotes_list_what_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::open_or_create(Paths::with_root(dir.path().join("home"))).unwrap();
        app.config_mut().upsert_remote(RemoteConfig::new("lab", "dev@lab")).unwrap();

        let err = transport_for(&app, "desktop").unwrap_err().to_string();
        assert!(err.contains("no remote named `desktop`"), "{err}");
        assert!(err.contains("configured: lab"), "{err}");
        assert!(transport_for(&app, "LAB").is_ok());
    }
}
