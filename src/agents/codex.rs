//! Codex and other `AGENTS.md` readers.
//!
//! `AGENTS.md` is a cross-tool convention rather than a Codex-specific format,
//! so this adapter also covers the several agents that adopted it.

use std::path::{Path, PathBuf};

use crate::agents::{AgentAdapter, AgentFile};
use crate::core::context::{render, ContextBundle};

/// Adapter for `AGENTS.md`.
pub struct CodexAdapter;

impl AgentAdapter for CodexAdapter {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex (AGENTS.md)"
    }

    fn detect(&self, repo: &Path, include_global: bool) -> Vec<AgentFile> {
        let mut files = vec![AgentFile {
            exists: repo.join("AGENTS.md").is_file(),
            path: repo.join("AGENTS.md"),
            global: false,
        }];
        // Some repositories keep it under .github/ or docs/.
        for candidate in
            [repo.join(".github").join("AGENTS.md"), repo.join("docs").join("AGENTS.md")]
        {
            if candidate.is_file() {
                files.push(AgentFile { path: candidate, global: false, exists: true });
            }
        }
        if include_global {
            if let Some(home) = directories::BaseDirs::new() {
                let global = home.home_dir().join(".codex").join("AGENTS.md");
                files.push(AgentFile { exists: global.is_file(), path: global, global: true });
            }
        }
        files
    }

    fn export_path(&self, repo: &Path) -> PathBuf {
        repo.join("AGENTS.md")
    }

    fn render(&self, bundle: &ContextBundle) -> String {
        let mut out = String::new();
        out.push_str("# Agent instructions\n\n");
        out.push_str(&crate::agents::preamble(bundle, self.id()));
        out.push_str(&render::markdown(bundle, &crate::agents::render_options()));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_agents_md_in_common_locations() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".github")).unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "x").unwrap();
        std::fs::write(dir.path().join(".github/AGENTS.md"), "y").unwrap();

        let files = CodexAdapter.detect(dir.path(), false);
        assert_eq!(files.iter().filter(|f| f.exists).count(), 2);
        assert_eq!(CodexAdapter.export_path(dir.path()), dir.path().join("AGENTS.md"));
    }
}
