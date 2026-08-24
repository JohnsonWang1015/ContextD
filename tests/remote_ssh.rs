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
