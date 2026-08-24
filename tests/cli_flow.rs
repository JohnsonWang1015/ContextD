//! End-to-end CLI tests: the MVP acceptance flow and the behaviours around it.

mod common;

use common::Sandbox;

#[test]
fn mvp_acceptance_flow() {
    let sandbox = Sandbox::new();

    // init
    let init = sandbox.run(&["init"]);
    assert!(init.contains("ContextD is ready"));
    assert!(sandbox.home().join("contextd.db").is_file());
    assert!(sandbox.home().join("config.toml").is_file());

    // attach
    let attach = sandbox.run(&["attach", "--name", "FerroGrid"]);
    assert!(attach.contains("FerroGrid"));

    // add
    sandbox.run(&[
        "add",
        "--category",
        "architecture",
        "GPU scheduler uses NATS for task transport",
    ]);

    // checkpoint
    sandbox.run(&[
        "checkpoint",
        "worker heartbeat completed",
        "--goal",
        "Implement distributed GPU scheduling",
        "--done",
        "Coordinator",
        "--next",
        "Lease-based GPU allocation",
        "--problem",
        "Worker reconnect",
    ]);

    // search
    let search = sandbox.run(&["search", "scheduler"]);
    assert!(search.contains("NATS"), "search output: {search}");

    // recall, phrased as a question in another language
    let recall = sandbox.run(&["recall", "scheduler 使用哪一套 message transport？"]);
    assert!(recall.contains("NATS"), "recall output: {recall}");

    // export to two agents
    sandbox.run(&["export", "claude"]);
    sandbox.run(&["export", "codex"]);
    let claude = sandbox.read("CLAUDE.md");
    assert!(claude.contains("NATS"));
    assert!(claude.contains("Implement distributed GPU scheduling"));
    assert!(sandbox.read("AGENTS.md").contains("NATS"));

    // status
    let status = sandbox.run(&["status"]);
    assert!(status.contains("FerroGrid"));
    assert!(status.contains("worker heartbeat completed"));
    assert!(status.contains("Semantic index"));

    // resume reads like a handover
    let resume = sandbox.run(&["resume"]);
    assert!(resume.starts_with("Resume FerroGrid."));
    assert!(resume.contains("Lease-based GPU allocation"));
    assert!(resume.contains("Worker reconnect"));
}

#[test]
fn current_truth_outranks_history() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();

    let redis = sandbox.run_json(&["add", "-c", "architecture", "Task queue uses Redis"])["memory"]
        ["id"]
        .as_str()
        .unwrap()
        .to_string();
    let postgres = sandbox.run_json(&[
        "add",
        "-c",
        "architecture",
        "Task queue uses PostgreSQL LISTEN/NOTIFY",
        "--supersedes",
        &redis,
    ])["memory"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    sandbox.run_json(&[
        "add",
        "-c",
        "architecture",
        "Task queue transport migrated to NATS",
        "--supersedes",
        &postgres,
    ]);

    // Default retrieval returns only what is currently true…
    let hits = sandbox.run_json(&["search", "task queue"]);
    let titles: Vec<String> =
        hits.as_array().unwrap().iter().map(|h| h["title"].as_str().unwrap().into()).collect();
    assert!(titles.iter().any(|t| t.contains("NATS")), "{titles:?}");
    assert!(!titles.iter().any(|t| t.contains("Redis")), "history leaked into results: {titles:?}");

    // …and history is available when asked for, marked as such.
    let with_history = sandbox.run_json(&["search", "task queue", "--history"]);
    let statuses: Vec<String> = with_history
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["status"].as_str().unwrap().into())
        .collect();
    assert!(statuses.iter().any(|s| s == "superseded"));
    assert_eq!(with_history[0]["status"], "active", "current truth must rank first");
}

#[test]
fn export_preserves_hand_written_instructions() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();
    std::fs::write(
        sandbox.repo().join("CLAUDE.md"),
        "# House rules\n\nNever force-push to main.\n",
    )
    .unwrap();
    sandbox.run(&["add", "-c", "architecture", "Scheduler transport is NATS"]);

    sandbox.run(&["export", "claude"]);
    let text = sandbox.read("CLAUDE.md");
    assert!(text.contains("Never force-push to main."), "user content was destroyed");
    assert!(text.contains("NATS"));

    // Editing inside the managed block is a conflict, and nothing is written.
    let edited = text.replace("NATS", "Kafka");
    std::fs::write(sandbox.repo().join("CLAUDE.md"), &edited).unwrap();
    let stderr = sandbox.run_failing(&["export", "claude"]);
    assert!(stderr.contains("modified outside ContextD"), "stderr: {stderr}");
    assert_eq!(sandbox.read("CLAUDE.md"), edited);

    // --force overwrites the block but still keeps the user's own section.
    sandbox.run(&["export", "claude", "--force"]);
    let forced = sandbox.read("CLAUDE.md");
    assert!(forced.contains("Never force-push to main."));
    assert!(forced.contains("NATS"));
}

#[test]
fn import_round_trips_without_duplicating() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();
    std::fs::write(
        sandbox.repo().join("AGENTS.md"),
        "# Coding conventions\n\nUse rustfmt and clippy.\n\n# Architecture\n\nWorkers pull leases.\n",
    )
    .unwrap();

    sandbox.run(&["import", "codex"]);
    let after_first = sandbox.run_json(&["memories"]).as_array().unwrap().len();
    assert_eq!(after_first, 2);

    sandbox.run(&["import", "codex"]);
    let after_second = sandbox.run_json(&["memories"]).as_array().unwrap().len();
    assert_eq!(after_second, 2, "re-importing must not duplicate memories");

    // Exported content must not come back in as new memories either.
    sandbox.run(&["export", "codex", "--force"]);
    sandbox.run(&["import", "codex"]);
    assert_eq!(sandbox.run_json(&["memories"]).as_array().unwrap().len(), 2);
}

#[test]
fn refresh_merges_duplicates_and_dry_run_changes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();
    sandbox.run(&["add", "-c", "convention", "Run cargo clippy before pushing"]);
    sandbox.run(&["add", "-c", "convention", "Run cargo clippy before pushing"]);

    let dry = sandbox.run_json(&["refresh", "--dry-run"]);
    assert_eq!(dry["merged"].as_array().unwrap().len(), 1);
    assert_eq!(sandbox.run_json(&["memories"]).as_array().unwrap().len(), 2);

    let report = sandbox.run_json(&["refresh"]);
    assert_eq!(report["merged"].as_array().unwrap().len(), 1);
    assert_eq!(
        sandbox.run_json(&["memories"]).as_array().unwrap().len(),
        1,
        "the duplicate should now be history"
    );
    assert_eq!(sandbox.run_json(&["memories", "--all"]).as_array().unwrap().len(), 2);
}

#[test]
fn sync_writes_the_markdown_mirror() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();
    sandbox.run(&["add", "-c", "architecture", "Coordinator owns GPU leases"]);
    sandbox.run(&["decision", "add", "Use NATS for task transport", "--title", "Task queue"]);
    sandbox.run(&["checkpoint", "leases wired up"]);

    sandbox.run(&["sync"]);
    let project_dir = sandbox.home().join("projects").join("FerroGrid");
    assert!(project_dir.join("overview.md").is_file());
    assert!(std::fs::read_to_string(project_dir.join("architecture.md"))
        .unwrap()
        .contains("GPU leases"));
    assert!(std::fs::read_to_string(project_dir.join("decisions.md")).unwrap().contains("NATS"));
    assert_eq!(std::fs::read_dir(project_dir.join("checkpoints")).unwrap().count(), 1);
    assert!(sandbox.home().join("global").join("coding.md").is_file());
}

#[test]
fn commands_fail_helpfully_before_init_and_outside_a_project() {
    let sandbox = Sandbox::new();
    let stderr = sandbox.run_failing(&["status"]);
    assert!(stderr.contains("contextd init"), "stderr: {stderr}");

    sandbox.run(&["init"]);
    let stderr = sandbox.run_failing(&["add", "-c", "task", "something"]);
    assert!(stderr.contains("attach"), "stderr: {stderr}");

    // Global memories do not need a project.
    sandbox.run(&["add", "--global", "-c", "user", "Prefers small commits"]);
    let memories = sandbox.run_json(&["memories", "--global"]);
    assert_eq!(memories.as_array().unwrap().len(), 1);
}

#[test]
fn memory_lifecycle_commands() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();
    let id = sandbox.run_json(&["add", "-c", "task", "Wire up worker reconnect"])["memory"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let short = &id[..8];

    let shown = sandbox.run(&["show", short]);
    assert!(shown.contains("Wire up worker reconnect"));

    sandbox.run(&["edit", short, "--priority", "5", "--tag", "urgent"]);
    let edited = sandbox.run_json(&["show", short]);
    assert_eq!(edited["priority"], 5);
    assert_eq!(edited["tags"][0], "urgent");

    sandbox.run(&["delete", short, "--archive"]);
    assert!(sandbox.run_json(&["memories"]).as_array().unwrap().is_empty());
    assert_eq!(sandbox.run_json(&["memories", "--all"]).as_array().unwrap().len(), 1);

    sandbox.run(&["delete", short]);
    assert!(sandbox.run_json(&["memories", "--all"]).as_array().unwrap().is_empty());
}

#[test]
fn detach_keeps_memories_and_reattach_finds_them() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();
    sandbox.run(&["add", "-c", "architecture", "Coordinator owns GPU leases"]);

    sandbox.run(&["detach"]);
    assert!(sandbox.run_json(&["list"])["projects"].as_array().unwrap().is_empty());

    sandbox.run(&["attach"]);
    let memories = sandbox.run_json(&["memories"]);
    assert_eq!(memories.as_array().unwrap().len(), 1, "memories survive a detach");
}

#[test]
fn memory_moves_between_machines_as_a_bundle() {
    // Two independent ContextD installations, the same repository.
    let laptop = Sandbox::new();
    laptop.run(&["init"]);
    laptop.run(&["attach", "--name", "FerroGrid"]);
    laptop.run(&["add", "-c", "architecture", "Scheduler transport is NATS"]);
    laptop.run(&["decision", "add", "Use NATS JetStream", "--title", "Task transport"]);
    laptop.run(&["checkpoint", "heartbeat done", "--next", "GPU lease allocation"]);

    let bundle_path = laptop.dir.path().join("memory.json");
    laptop.run(&["bundle", "export", "--out", bundle_path.to_str().unwrap()]);
    assert!(bundle_path.is_file());

    let desktop = Sandbox::new();
    desktop.run(&["init"]);
    desktop.run(&["attach", "--name", "FerroGrid"]);

    let report = desktop.run_json(&["bundle", "import", "--file", bundle_path.to_str().unwrap()]);
    assert_eq!(report["memories_added"], 1);
    assert_eq!(report["decisions_added"], 1);
    assert_eq!(report["checkpoints_added"], 1);

    // The receiving machine can now answer questions about the work.
    let recall = desktop.run(&["recall", "which transport does the scheduler use?"]);
    assert!(recall.contains("NATS"), "recall on the second machine: {recall}");
    let resume = desktop.run(&["resume"]);
    assert!(resume.contains("GPU lease allocation"), "resume: {resume}");

    // Re-importing changes nothing.
    let again = desktop.run_json(&["bundle", "import", "--file", bundle_path.to_str().unwrap()]);
    assert_eq!(again["memories_added"], 0);
    assert_eq!(again["memories_unchanged"], 1);
    assert_eq!(desktop.run_json(&["memories"]).as_array().unwrap().len(), 1);
}

#[test]
fn bundle_import_dry_run_and_stdin() {
    let laptop = Sandbox::new();
    laptop.bootstrap();
    laptop.run(&["add", "-c", "knowledge", "Build with cargo build --release"]);
    let bundle = laptop.run(&["bundle", "export"]);

    let desktop = Sandbox::new();
    desktop.run(&["init"]);

    // stdin is the path `contextd remote push` uses over SSH.
    let mut child = desktop
        .cmd()
        .args(["bundle", "import", "--stdin", "--dry-run", "--json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child.stdin.as_mut().unwrap().write_all(bundle.as_bytes()).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("merge report on stdout");
    assert_eq!(report["memories_added"], 1);
    assert_eq!(report["dry_run"], true);

    // Dry run wrote nothing.
    assert!(desktop.run_json(&["memories", "--all"]).as_array().unwrap().is_empty());
}

#[test]
fn remotes_are_configured_and_validated() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();

    sandbox.run(&["remote", "add", "lab", "dev@lab-box", "--ssh-option=-p", "--ssh-option=2222"]);
    let remotes = sandbox.run_json(&["remote", "list"]);
    assert_eq!(remotes[0]["name"], "lab");
    assert_eq!(remotes[0]["host"], "dev@lab-box");

    // The config file is the source of truth and stays human-editable.
    let config = std::fs::read_to_string(sandbox.home().join("config.toml")).unwrap();
    assert!(config.contains("[[remote]]"), "config.toml: {config}");
    assert!(config.contains("dev@lab-box"));

    // Unknown remote fails with a helpful message rather than an ssh error.
    let stderr = sandbox.run_failing(&["remote", "pull", "desktop"]);
    assert!(stderr.contains("no remote named `desktop`"), "stderr: {stderr}");
    assert!(stderr.contains("configured: lab"), "stderr: {stderr}");

    sandbox.run(&["remote", "remove", "lab"]);
    assert!(sandbox.run_json(&["remote", "list"]).as_array().unwrap().is_empty());
}

#[test]
fn sessions_group_the_work_they_produced() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();

    // Work done with no session open belongs to no session.
    sandbox.run(&["checkpoint", "solo work"]);

    let started = sandbox.run(&["session", "start", "--agent", "claude"]);
    assert!(started.contains("Session open"));

    sandbox.run(&["add", "-c", "architecture", "Scheduler transport is NATS"]);
    sandbox.run(&["decision", "add", "Use NATS JetStream", "--title", "Task transport"]);
    sandbox.run(&["checkpoint", "worker heartbeat completed"]);

    // Status shows the open session.
    let status = sandbox.run_json(&["status"]);
    assert_eq!(status["session"]["session"]["agent"], "claude");
    assert!(status["session"]["session"]["ended_at"].is_null());
    assert!(sandbox.run(&["status"]).contains("Session"));

    let show = sandbox.run_json(&["session", "show"]);
    assert_eq!(show["checkpoints"].as_array().unwrap().len(), 1, "only the in-session checkpoint");
    assert_eq!(show["memories"].as_array().unwrap().len(), 1);
    assert_eq!(show["decisions"].as_array().unwrap().len(), 1);

    let ended = sandbox.run(&["session", "end", "heartbeat is done"]);
    assert!(ended.contains("Session closed"));
    assert!(ended.contains("worker heartbeat completed"));

    // Closed sessions become the "what happened last time" line.
    let listed = sandbox.run_json(&["session", "list"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["session"]["summary"], "heartbeat is done");

    let resume = sandbox.run(&["resume"]);
    assert!(resume.contains("Last session: claude"), "resume: {resume}");
    assert!(resume.contains("heartbeat is done"), "resume: {resume}");

    // Ending twice is harmless, and a new session starts clean.
    assert!(sandbox.run(&["session", "end"]).contains("No session is open"));
    sandbox.run(&["session", "start", "--agent", "codex"]);
    assert!(sandbox.run_json(&["session", "show"])["checkpoints"].as_array().unwrap().is_empty());
}

#[test]
fn starting_a_session_closes_one_left_open() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();
    sandbox.run(&["session", "start", "--agent", "claude"]);
    let restarted = sandbox.run(&["session", "start", "--agent", "codex"]);
    assert!(restarted.contains("closed the session left open"), "{restarted}");

    let sessions = sandbox.run_json(&["session", "list"]);
    assert_eq!(sessions.as_array().unwrap().len(), 2);
    let open: Vec<&serde_json::Value> = sessions
        .as_array()
        .unwrap()
        .iter()
        .filter(|activity| activity["session"]["ended_at"].is_null())
        .collect();
    assert_eq!(open.len(), 1, "only one session may be open at a time");
    assert_eq!(open[0]["session"]["agent"], "codex");
}
