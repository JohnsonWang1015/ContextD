#![allow(dead_code)] // each integration test binary uses a subset of these helpers

//! Shared helpers for integration tests.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use tempfile::TempDir;

/// An isolated ContextD installation plus a repository to attach.
pub struct Sandbox {
    pub dir: TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("repo")).expect("repo dir");
        Self { dir }
    }

    pub fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    pub fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    /// A `contextd` command wired to this sandbox and running inside the repo.
    pub fn cmd(&self) -> Command {
        let mut command = Command::cargo_bin("contextd").expect("binary built");
        command
            .env("CONTEXTD_HOME", self.home())
            .env("NO_COLOR", "1")
            // Keep tests independent of whatever the developer has configured.
            .env_remove("OPENAI_API_KEY")
            .env_remove("RUST_LOG")
            .current_dir(self.repo());
        command
    }

    /// Run a command and return stdout, asserting success.
    pub fn run(&self, args: &[&str]) -> String {
        let output = self.cmd().args(args).output().expect("spawn contextd");
        assert!(
            output.status.success(),
            "`contextd {}` failed: {}\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Run a command expected to fail, returning stderr.
    pub fn run_failing(&self, args: &[&str]) -> String {
        let output = self.cmd().args(args).output().expect("spawn contextd");
        assert!(!output.status.success(), "`contextd {}` unexpectedly succeeded", args.join(" "));
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// Run a command and parse its `--json` output.
    pub fn run_json(&self, args: &[&str]) -> serde_json::Value {
        let mut full = args.to_vec();
        full.push("--json");
        let stdout = self.run(&full);
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{stdout}"))
    }

    /// `init` + `attach`, the starting point for most tests.
    pub fn bootstrap(&self) -> &Self {
        self.run(&["init"]);
        self.run(&["attach", "--name", "FerroGrid"]);
        self
    }

    pub fn read(&self, relative: impl AsRef<Path>) -> String {
        let path = self.repo().join(relative);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }
}
