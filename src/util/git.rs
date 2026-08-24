//! Git metadata via the `git` CLI.
//!
//! Shelling out keeps ContextD free of a libgit2/openssl build dependency and
//! behaves identically on Linux, macOS and Windows. Every call is best-effort:
//! a missing `git`, or a directory that is not a repository, yields `None`
//! rather than an error, because git data is always supplementary here.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Snapshot of a repository at checkpoint time.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GitSnapshot {
    pub root: Option<PathBuf>,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub remote: Option<String>,
    pub dirty_files: Vec<String>,
}

impl GitSnapshot {
    /// Collect what git can tell us about `dir`.
    pub fn capture(dir: &Path) -> Self {
        let root = repo_root(dir);
        if root.is_none() {
            return Self::default();
        }
        Self {
            // `symbolic-ref` reports the branch even in a repository with no
            // commits yet, where `rev-parse HEAD` fails; `rev-parse` is the
            // fallback that yields "HEAD" for a detached checkout.
            branch: run(dir, &["symbolic-ref", "--short", "HEAD"])
                .or_else(|| run(dir, &["rev-parse", "--abbrev-ref", "HEAD"])),
            commit: run(dir, &["rev-parse", "HEAD"]),
            remote: run(dir, &["config", "--get", "remote.origin.url"]),
            dirty_files: run(dir, &["status", "--porcelain"])
                .map(|out| {
                    out.lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(|l| l.get(3..).unwrap_or(l).trim().to_string())
                        .collect()
                })
                .unwrap_or_default(),
            root,
        }
    }

    /// True when git produced nothing useful.
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }
}

/// Top level of the repository containing `dir`, if any.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    run(dir, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// Short commit hash, convenient for display.
pub fn short_commit(commit: &str) -> &str {
    let end = commit.char_indices().nth(7).map_or(commit.len(), |(i, _)| i);
    &commit[..end]
}

fn run(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").current_dir(dir).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_repo_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let snap = GitSnapshot::capture(dir.path());
        assert!(snap.is_empty());
        assert!(snap.branch.is_none());
    }

    #[test]
    fn fresh_repo_reports_branch_before_first_commit() {
        let dir = tempfile::tempdir().unwrap();
        let init = Command::new("git").args(["init", "-q"]).current_dir(dir.path()).status();
        if !matches!(init, Ok(status) if status.success()) {
            return; // git unavailable
        }
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let snap = GitSnapshot::capture(dir.path());
        assert!(!snap.is_empty());
        assert!(snap.branch.is_some(), "branch should be known on an unborn HEAD");
        assert!(snap.commit.is_none(), "no commit exists yet");
        assert!(snap.dirty_files.iter().any(|f| f.contains("a.txt")));
    }

    #[test]
    fn short_commit_handles_short_input() {
        assert_eq!(short_commit("abc"), "abc");
        assert_eq!(short_commit("abcdef1234567"), "abcdef1");
    }
}
