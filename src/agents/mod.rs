//! Agent adapters.
//!
//! Every coding agent keeps its context in its own files: Claude Code reads
//! `CLAUDE.md` and `.claude/rules/`, Codex reads `AGENTS.md`, Cursor reads
//! `.cursor/rules/`. [`AgentAdapter`] is the one abstraction they share, so
//! ContextD is not built around any single tool and adding another agent means
//! adding a file here.
//!
//! Writes never clobber a developer's own instructions: generated content
//! lives inside a marked block (see [`markdown::splice_managed_block`]), and a
//! block that was edited by hand is reported as a conflict rather than
//! overwritten.

pub mod claude;
pub mod codex;
pub mod cursor;
pub mod generic;
pub mod markdown;

use std::path::{Path, PathBuf};

use crate::core::context::render::RenderOptions;
use crate::core::context::ContextBundle;
use crate::core::model::Category;
use crate::error::{Error, Result};

/// A file an agent reads its context from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFile {
    pub path: PathBuf,
    /// A global file (e.g. `~/.claude/CLAUDE.md`) rather than a repository one.
    pub global: bool,
    pub exists: bool,
}

/// One importable chunk of an agent file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedItem {
    pub title: String,
    pub content: String,
    pub category: Category,
    pub source: PathBuf,
    /// Global files carry developer-wide preferences, not project facts.
    pub global: bool,
}

/// Adapter for one agent.
pub trait AgentAdapter: Send + Sync {
    /// Stable identifier used on the command line and in bindings.
    fn id(&self) -> &'static str;

    /// Human-facing name.
    fn display_name(&self) -> &'static str;

    /// Files this agent would read in `repo` (plus global ones when asked).
    fn detect(&self, repo: &Path, include_global: bool) -> Vec<AgentFile>;

    /// Where `contextd export <agent>` writes by default.
    fn export_path(&self, repo: &Path) -> PathBuf;

    /// Whether ContextD owns the whole exported file.
    ///
    /// Files shared with the developer (`CLAUDE.md`, `AGENTS.md`) get a marked
    /// block spliced into them. Files ContextD writes for itself are replaced
    /// wholesale — necessary for formats such as Cursor's `.mdc`, where YAML
    /// front matter has to be the first thing in the file and a comment block
    /// wrapped around it would break the rule.
    fn owns_file(&self) -> bool {
        false
    }

    /// Read agent files into candidate memories.
    fn import(&self, files: &[AgentFile]) -> Result<Vec<ImportedItem>> {
        let mut items = Vec::new();
        for file in files.iter().filter(|f| f.exists) {
            let text = std::fs::read_to_string(&file.path).map_err(|e| Error::io(&file.path, e))?;
            for section in markdown::sections(&text) {
                let title = if section.heading.trim().is_empty() {
                    default_title(&file.path)
                } else {
                    section.heading.clone()
                };
                items.push(ImportedItem {
                    title,
                    content: section.body,
                    category: section.category,
                    source: file.path.clone(),
                    global: file.global,
                });
            }
        }
        Ok(items)
    }

    /// Render context in the form this agent expects.
    fn render(&self, bundle: &ContextBundle) -> String;
}

/// Adapters known to this build.
pub fn all() -> Vec<Box<dyn AgentAdapter>> {
    vec![
        Box::new(claude::ClaudeAdapter),
        Box::new(codex::CodexAdapter),
        Box::new(cursor::CursorAdapter),
        Box::new(generic::GenericAdapter),
    ]
}

/// Look up an adapter by id.
pub fn get(id: &str) -> Result<Box<dyn AgentAdapter>> {
    let id = id.trim().to_lowercase();
    all().into_iter().find(|a| a.id() == id).ok_or_else(|| Error::UnknownAgent(id))
}

/// Every file any known agent might use in `repo`.
pub fn detect_all(repo: &Path, include_global: bool) -> Vec<(String, AgentFile)> {
    all()
        .iter()
        .flat_map(|adapter| {
            adapter
                .detect(repo, include_global)
                .into_iter()
                .map(|file| (adapter.id().to_string(), file))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Standard options for rendering into an agent file.
pub(crate) fn render_options() -> RenderOptions {
    RenderOptions {
        include_ids: false,
        include_superseded: true,
        heading_level: 2,
        include_scores: false,
    }
}

/// Title used for a file whose content has no heading.
fn default_title(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Imported".into())
}

/// Standard preamble telling a reader (human or model) where this came from.
pub(crate) fn preamble(bundle: &ContextBundle, agent: &str) -> String {
    let project = bundle.project.as_ref().map(|p| p.name.as_str()).unwrap_or("this workspace");
    format!(
        "_Generated by ContextD for {agent}. Edit memories with `contextd edit`, \
         then re-run `contextd export {agent}`; changes made inside this block are \
         overwritten. Project: {project}._\n\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lookup() {
        assert_eq!(get("claude").unwrap().id(), "claude");
        assert_eq!(get("  CODEX ").unwrap().id(), "codex");
        assert!(matches!(get("emacs"), Err(Error::UnknownAgent(_))));
        assert_eq!(all().len(), 4);
    }

    #[test]
    fn detect_all_covers_every_adapter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# x\nbody").unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# y\nbody").unwrap();

        let found = detect_all(dir.path(), false);
        let existing: Vec<&str> =
            found.iter().filter(|(_, f)| f.exists).map(|(agent, _)| agent.as_str()).collect();
        assert!(existing.contains(&"claude"));
        assert!(existing.contains(&"codex"));
    }

    #[test]
    fn import_reads_sections_from_existing_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("CLAUDE.md");
        std::fs::write(&path, "# Conventions\nUse rustfmt.\n").unwrap();

        let adapter = get("claude").unwrap();
        let items = adapter
            .import(&[
                AgentFile { path: path.clone(), global: false, exists: true },
                AgentFile { path: dir.path().join("missing.md"), global: false, exists: false },
            ])
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].category, Category::Convention);
        assert_eq!(items[0].title, "Conventions");
    }
}
