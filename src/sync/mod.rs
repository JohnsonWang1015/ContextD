//! Synchronisation between the database, the Markdown mirror and agent files.
//!
//! SQLite is authoritative. Markdown exists so a human can read, edit and
//! commit their memory, and so agents that only understand files can consume
//! it. Anything ContextD writes into a file the developer also owns is
//! confined to a marked block, and a block that changed since ContextD last
//! wrote it is reported as a conflict rather than overwritten.

pub mod agent_sync;
pub mod bundle;
pub mod mirror;
pub mod remote;

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::util::hash::content_hash;

/// What happened to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WriteStatus {
    /// The file did not exist and was created.
    Created,
    /// The managed block was updated.
    Updated,
    /// Already up to date.
    Unchanged,
    /// The file changed outside ContextD; nothing was written.
    Conflict,
    /// Nothing was written because this was a dry run.
    Skipped,
}

impl WriteStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            WriteStatus::Created => "created",
            WriteStatus::Updated => "updated",
            WriteStatus::Unchanged => "unchanged",
            WriteStatus::Conflict => "conflict",
            WriteStatus::Skipped => "skipped",
        }
    }
}

/// Result of writing one file.
#[derive(Debug, Clone, Serialize)]
pub struct FileOutcome {
    pub path: PathBuf,
    pub status: WriteStatus,
    /// Hash of the content ContextD wrote, recorded for later conflict checks.
    pub hash: Option<String>,
    pub detail: Option<String>,
}

impl FileOutcome {
    pub fn new(path: impl Into<PathBuf>, status: WriteStatus) -> Self {
        Self { path: path.into(), status, hash: None, detail: None }
    }

    pub fn with_hash(mut self, hash: impl Into<String>) -> Self {
        self.hash = Some(hash.into());
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// Write a file atomically: content goes to a temporary file in the same
/// directory, then is renamed over the target. A crash mid-write therefore
/// leaves either the old file or the new one, never a truncated CLAUDE.md.
pub fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = path.with_file_name(format!(".{file_name}.contextd-tmp"));
    std::fs::write(&temp, content).map_err(|e| Error::io(&temp, e))?;

    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            Err(Error::io(path, err))
        }
    }
}

/// Read a file, returning an empty string when it does not exist.
pub fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(Error::io(path, e)),
    }
}

/// Decide whether writing `path` would destroy an edit ContextD did not make.
///
/// `last_written` is the hash ContextD recorded the previous time it wrote the
/// managed content. `current` is what is in the file now.
pub fn is_conflict(last_written: Option<&str>, current: Option<&str>) -> bool {
    match (last_written, current) {
        // Never written before, and something is already there: leave it alone
        // until the user says otherwise.
        (None, Some(existing)) => !existing.trim().is_empty(),
        (Some(expected), Some(existing)) => content_hash(existing) != expected,
        // Nothing there now: writing cannot destroy anything.
        (_, None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_creates_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("CLAUDE.md");
        write_atomic(&path, "one").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one");
        write_atomic(&path, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");

        // No temporary files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("contextd-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn read_or_empty_tolerates_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_or_empty(&dir.path().join("nope.md")).unwrap(), "");
    }

    #[test]
    fn conflict_rules() {
        // Untracked file with content: do not touch it.
        assert!(is_conflict(None, Some("hand written")));
        assert!(!is_conflict(None, Some("   ")));
        assert!(!is_conflict(None, None));

        let hash = content_hash("generated");
        assert!(!is_conflict(Some(&hash), Some("generated")));
        assert!(
            !is_conflict(Some(&hash), Some("generated\r\n")),
            "line endings alone are not a conflict"
        );
        assert!(is_conflict(Some(&hash), Some("generated, then edited by hand")));
    }
}
