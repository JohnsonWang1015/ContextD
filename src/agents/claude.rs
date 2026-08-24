//! Claude Code.
//!
//! Reads and writes `CLAUDE.md` at the repository root, `.claude/rules/*.md`,
//! and — only when explicitly asked — the developer-wide `~/.claude/CLAUDE.md`.
//! The global file belongs to the user and is never written implicitly.

use std::path::{Path, PathBuf};

use crate::agents::{markdown, AgentAdapter, AgentFile};
use crate::core::context::{render, ContextBundle};

/// Adapter for Claude Code.
pub struct ClaudeAdapter;

impl ClaudeAdapter {
    /// `~/.claude/CLAUDE.md`, resolved without hardcoding a home path.
    pub fn global_memory_file() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|b| b.home_dir().join(".claude").join("CLAUDE.md"))
    }
}

impl AgentAdapter for ClaudeAdapter {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn detect(&self, repo: &Path, include_global: bool) -> Vec<AgentFile> {
        let mut files = vec![AgentFile {
            exists: repo.join("CLAUDE.md").is_file(),
            path: repo.join("CLAUDE.md"),
            global: false,
        }];

        // Rule files are many and unordered; list whatever is there.
        let rules_dir = repo.join(".claude").join("rules");
        if let Ok(entries) = std::fs::read_dir(&rules_dir) {
            let mut rules: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "md" || ext == "mdc"))
                .collect();
            rules.sort();
            files.extend(rules.into_iter().map(|path| AgentFile {
                exists: true,
                path,
                global: false,
            }));
        }

        if include_global {
            if let Some(global) = Self::global_memory_file() {
                files.push(AgentFile { exists: global.is_file(), path: global, global: true });
            }
        }
        files
    }

    fn export_path(&self, repo: &Path) -> PathBuf {
        repo.join("CLAUDE.md")
    }

    fn render(&self, bundle: &ContextBundle) -> String {
        let mut out = String::new();
        out.push_str("# Project memory\n\n");
        out.push_str(&crate::agents::preamble(bundle, self.id()));
        out.push_str(&render::markdown(bundle, &crate::agents::render_options()));
        out
    }
}

/// Wrap generated content in the managed block for a Claude file.
pub fn splice(existing: &str, generated: &str) -> String {
    markdown::splice_managed_block(existing, generated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::context::BudgetReport;

    fn empty_bundle() -> ContextBundle {
        ContextBundle {
            project: None,
            query: None,
            checkpoint: None,
            decisions: vec![],
            superseded: vec![],
            memories: vec![],
            budget: BudgetReport::default(),
        }
    }

    #[test]
    fn detects_claude_md_and_rules() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude/rules")).unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "x").unwrap();
        std::fs::write(dir.path().join(".claude/rules/style.md"), "y").unwrap();
        std::fs::write(dir.path().join(".claude/rules/ignore.txt"), "z").unwrap();

        let files = ClaudeAdapter.detect(dir.path(), false);
        assert_eq!(files.len(), 2, "only .md/.mdc rules are picked up: {files:?}");
        assert!(files.iter().all(|f| f.exists));
        assert!(files.iter().all(|f| !f.global));
    }

    #[test]
    fn missing_claude_md_is_reported_but_not_invented() {
        let dir = tempfile::tempdir().unwrap();
        let files = ClaudeAdapter.detect(dir.path(), false);
        assert_eq!(files.len(), 1);
        assert!(!files[0].exists);
        assert!(!dir.path().join("CLAUDE.md").exists());
    }

    #[test]
    fn global_file_is_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ClaudeAdapter.detect(dir.path(), false).iter().all(|f| !f.global));
        let with_global = ClaudeAdapter.detect(dir.path(), true);
        assert!(with_global.iter().any(|f| f.global));
    }

    #[test]
    fn export_path_is_repo_root() {
        assert_eq!(ClaudeAdapter.export_path(Path::new("/repo")), Path::new("/repo/CLAUDE.md"));
    }

    #[test]
    fn rendering_is_markdown_with_a_preamble() {
        let text = ClaudeAdapter.render(&empty_bundle());
        assert!(text.starts_with("# Project memory"));
        assert!(text.contains("contextd export claude"));
    }
}
