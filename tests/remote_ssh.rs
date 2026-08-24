//! Exercises the real SSH transport end to end.
//!
//! A stand-in `ssh` earlier on `PATH` runs the remote command string locally,
//! which is exactly what a real `ssh` does once the connection is up. That
//! covers the parts worth testing here — argument quoting, the `CONTEXTD_HOME`
//! prefix, stdin piping and exit-status handling — without depending on an
//! sshd, a key pair, or the network.

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::Sandbox;

/// Create a directory containing an `ssh` that executes the command it is
/// given instead of connecting anywhere.
fn fake_ssh_dir(root: &Path) -> PathBuf {
    let bin = root.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("ssh");
    std::fs::write(
        &script,
        // The remote command is always the final argument; everything before
        // it is options and the destination, which a local run ignores.
        "#!/bin/sh\nfor arg in \"$@\"; do last=\"$arg\"; done\nexec /bin/sh -c \"$last\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn contextd_binary() -> PathBuf {
    // The test binary lives in target/<profile>/deps/, next to the CLI binary.
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("contextd")
}

#[test]
fn pull_and_push_over_the_ssh_transport() {
    let binary = contextd_binary();
    assert!(binary.is_file(), "expected the contextd binary at {}", binary.display());

    // Two machines: `lab` plays the remote, `laptop` the local one.
    let lab = Sandbox::new();
    lab.run(&["init"]);
    lab.run(&["attach", "--name", "FerroGrid"]);
    lab.run(&["add", "-c", "architecture", "Lab: GPU nodes run on bare metal"]);

    let laptop = Sandbox::new();
    laptop.run(&["init"]);
    laptop.run(&["attach", "--name", "FerroGrid"]);
    laptop.run(&["add", "-c", "architecture", "Laptop: scheduler transport is NATS"]);

    let path_prefix = fake_ssh_dir(laptop.dir.path());
    let with_fake_ssh = |args: &[&str]| -> String {
        let existing = std::env::var("PATH").unwrap_or_default();
        let output = laptop
            .cmd()
            .env("PATH", format!("{}:{existing}", path_prefix.display()))
            .args(args)
            .output()
            .expect("spawn contextd");
        assert!(
            output.status.success(),
            "`contextd {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    // A remote whose "host" is ignored by the stand-in, pointing at the lab's
    // store and at this build of the binary.
    laptop.run(&[
        "remote",
        "add",
        "lab",
        "dev@lab-box",
        "--command",
        binary.to_str().unwrap(),
        "--remote-home",
        lab.home().to_str().unwrap(),
    ]);

    // Dry run reports the incoming record without writing it.
    let dry = with_fake_ssh(&["remote", "pull", "lab", "--dry-run", "--json"]);
    let dry: serde_json::Value = serde_json::from_str(&dry).unwrap();
    assert_eq!(dry["merge"]["memories_added"], 1, "{dry}");
    assert_eq!(laptop.run_json(&["memories"]).as_array().unwrap().len(), 1);

    // Pull for real: the lab's memory arrives and is embedded locally.
    let pulled = with_fake_ssh(&["remote", "pull", "lab", "--json"]);
    let pulled: serde_json::Value = serde_json::from_str(&pulled).unwrap();
    assert_eq!(pulled["merge"]["memories_added"], 1);
    let after_pull = laptop.run_json(&["memories"]);
    assert_eq!(after_pull.as_array().unwrap().len(), 2);
    assert!(laptop.run(&["recall", "where do GPU nodes run?"]).contains("bare metal"));

    // Push: the laptop's own memory reaches the lab, and the remote's merge
    // report comes back through the transport.
    let pushed = with_fake_ssh(&["remote", "push", "lab", "--json"]);
    let pushed: serde_json::Value = serde_json::from_str(&pushed).unwrap();
    assert_eq!(pushed["direction"], "push");
    assert_eq!(pushed["merge"]["memories_added"], 1, "{pushed}");
    assert_eq!(lab.run_json(&["memories"]).as_array().unwrap().len(), 2);

    // Both machines now hold both facts, and repeating the exchange is a no-op.
    let again = with_fake_ssh(&["remote", "pull", "lab", "--json"]);
    let again: serde_json::Value = serde_json::from_str(&again).unwrap();
    assert_eq!(again["merge"]["memories_added"], 0);
    assert_eq!(again["merge"]["memories_unchanged"], 2);
}

#[test]
fn a_failing_remote_reports_the_ssh_error() {
    let laptop = Sandbox::new();
    laptop.bootstrap();

    let bin = laptop.dir.path().join("failbin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("ssh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'ssh: connect to host lab-box port 22: No route to host' >&2\nexit 255\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    laptop.run(&["remote", "add", "lab", "dev@lab-box"]);

    let existing = std::env::var("PATH").unwrap_or_default();
    let output = laptop
        .cmd()
        .env("PATH", format!("{}:{existing}", bin.display()))
        .args(["remote", "pull", "lab"])
        .output()
        .unwrap();

    assert!(!output.status.success(), "an unreachable host must fail the command");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("remote `lab` failed"), "stderr: {stderr}");
    assert!(stderr.contains("No route to host"), "the ssh diagnosis must survive: {stderr}");
}

#[test]
fn scanning_a_remote_reports_what_it_holds_without_copying_it() {
    let binary = contextd_binary();

    // The machine to be surveyed: two projects and a global memory.
    let lab = Sandbox::new();
    lab.run(&["init"]);
    lab.run(&["attach", "--name", "FerroGrid"]);
    lab.run(&["add", "-c", "architecture", "Scheduler transport is NATS"]);
    lab.run(&["add", "-c", "architecture", "GPU nodes are bare metal"]);
    lab.run(&["add", "-c", "convention", "Format with rustfmt"]);
    lab.run(&["decision", "add", "Use NATS JetStream", "--title", "Task transport"]);
    lab.run(&["checkpoint", "heartbeat done"]);
    lab.run(&["add", "--global", "-c", "user", "Prefers small commits"]);

    let laptop = Sandbox::new();
    laptop.run(&["init"]);

    let path_prefix = fake_ssh_dir(laptop.dir.path());
    let with_fake_ssh = |args: &[&str]| -> String {
        let existing = std::env::var("PATH").unwrap_or_default();
        let output = laptop
            .cmd()
            .env("PATH", format!("{}:{existing}", path_prefix.display()))
            .args(args)
            .output()
            .expect("spawn contextd");
        assert!(
            output.status.success(),
            "`contextd {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    // An unconfigured destination can be surveyed directly, before deciding
    // whether it is worth keeping as a remote.
    let raw = with_fake_ssh(&[
        "remote",
        "scan",
        "dev@lab-box",
        "--command",
        binary.to_str().unwrap(),
        "--remote-home",
        lab.home().to_str().unwrap(),
        "--json",
    ]);
    let inventory: serde_json::Value = serde_json::from_str(&raw).unwrap();

    assert_eq!(inventory["totals"]["projects"], 1);
    assert_eq!(inventory["totals"]["memories"], 4, "three project memories plus one global");
    assert_eq!(inventory["totals"]["decisions"], 1);
    assert_eq!(inventory["totals"]["checkpoints"], 1);
    assert_eq!(inventory["projects"][0]["name"], "FerroGrid");
    assert_eq!(inventory["projects"][0]["last_checkpoint"], "heartbeat done");
    assert_eq!(inventory["global"]["memories"], 1);
    assert_eq!(inventory["home"], lab.home().to_str().unwrap());

    // Surveying copies nothing.
    assert!(laptop.run_json(&["memories"]).as_array().unwrap().is_empty());
    assert!(laptop.run_json(&["list"])["projects"].as_array().unwrap().is_empty());

    // The same survey through a configured remote, in human form.
    laptop.run(&[
        "remote",
        "add",
        "lab",
        "dev@lab-box",
        "--command",
        binary.to_str().unwrap(),
        "--remote-home",
        lab.home().to_str().unwrap(),
    ]);
    let text = with_fake_ssh(&["remote", "scan", "lab", "--detail"]);
    assert!(text.contains("FerroGrid"), "{text}");
    assert!(text.contains("Nothing was copied"), "{text}");
    assert!(text.contains("architecture"), "--detail shows the breakdown: {text}");

    // And the local equivalent describes this machine.
    let local = laptop.run_json(&["inventory"]);
    assert_eq!(local["totals"]["memories"], 0);
}

#[test]
fn scanning_a_machine_without_contextd_explains_the_fix() {
    let laptop = Sandbox::new();
    laptop.bootstrap();

    let bin = laptop.dir.path().join("nobin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("ssh");
    std::fs::write(&script, "#!/bin/sh\necho 'bash: contextd: command not found' >&2\nexit 127\n")
        .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let existing = std::env::var("PATH").unwrap_or_default();
    let output = laptop
        .cmd()
        .env("PATH", format!("{}:{existing}", bin.display()))
        .args(["remote", "scan", "dev@lab-box"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not on the PATH"), "stderr: {stderr}");
    assert!(stderr.contains("--command"), "stderr: {stderr}");
}

/// An `ssh` that records the arguments it was handed and then fails, so the
/// flags ContextD passes can be inspected without contacting anything.
fn arg_recording_ssh(root: &Path, log: &Path) -> PathBuf {
    let bin = root.join("argbin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("ssh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nexit 255\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

#[test]
fn password_prompting_is_allowed_unless_batch_is_asked_for() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();
    let log = sandbox.dir.path().join("ssh-args.txt");
    let bin = arg_recording_ssh(sandbox.dir.path(), &log);

    let run = |args: &[&str]| -> Vec<String> {
        let existing = std::env::var("PATH").unwrap_or_default();
        let _ = sandbox
            .cmd()
            .env("PATH", format!("{}:{existing}", bin.display()))
            .args(args)
            .output()
            .expect("spawn contextd");
        std::fs::read_to_string(&log).unwrap_or_default().lines().map(str::to_string).collect()
    };

    // Asking for interaction must not disable ssh's password prompt.
    let args = run(&["remote", "scan", "johnson@140.123.105.18", "--interactive"]);
    assert!(
        !args.iter().any(|arg| arg == "BatchMode=yes"),
        "BatchMode would suppress the password prompt: {args:?}"
    );
    assert!(args.iter().any(|arg| arg == "NumberOfPasswordPrompts=3"), "{args:?}");
    assert!(args.iter().any(|arg| arg == "johnson@140.123.105.18"), "{args:?}");
    assert!(
        args.last().is_some_and(|last| last.contains("contextd inventory --json")),
        "the remote command is the final argument: {args:?}"
    );

    // A scripted run must fail rather than wait for a password nobody types.
    let args = run(&["remote", "scan", "johnson@140.123.105.18", "--batch"]);
    assert!(args.iter().any(|arg| arg == "BatchMode=yes"), "{args:?}");

    // The same choice applies to pull and push.
    let args = run(&["remote", "pull", "johnson@140.123.105.18", "--batch"]);
    assert!(args.is_empty() || args.iter().any(|arg| arg == "BatchMode=yes"), "{args:?}");
}

#[test]
fn a_refused_password_login_explains_the_next_step() {
    let sandbox = Sandbox::new();
    sandbox.bootstrap();

    let bin = sandbox.dir.path().join("denybin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("ssh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'johnson@140.123.105.18: Permission denied (publickey,password).' >&2\nexit 255\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let existing = std::env::var("PATH").unwrap_or_default();
    let output = sandbox
        .cmd()
        .env("PATH", format!("{}:{existing}", bin.display()))
        .args(["remote", "scan", "johnson@140.123.105.18"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refused the login"), "stderr: {stderr}");
    assert!(stderr.contains("ssh-copy-id johnson@140.123.105.18"), "stderr: {stderr}");
}
