//! Any other agent: plain Markdown, no tool-specific framing.
//!
//! This is also the format `contextd export generic` prints to stdout, which
//! makes ContextD usable from an agent that has no adapter yet — pipe it in.

use std::path::{Path, PathBuf};

use crate::agents::{AgentAdapter, AgentFile};
use crate::core::context::{render, ContextBundle};

/// Adapter emitting plain Markdown.
pub struct GenericAdapter;

/// Default file name for a generic export.
pub const EXPORT_FILE: &str = "CONTEXT.md";

impl AgentAdapter for GenericAdapter {
    fn id(&self) -> &'static str {
        "generic"
    }

    fn display_name(&self) -> &'static str {
        "Generic Markdown"
    }

    fn detect(&self, repo: &Path, _include_global: bool) -> Vec<AgentFile> {
        let path = repo.join(EXPORT_FILE);
        vec![AgentFile { exists: path.is_file(), path, global: false }]
    }

    fn export_path(&self, repo: &Path) -> PathBuf {
        repo.join(EXPORT_FILE)
    }

    fn render(&self, bundle: &ContextBundle) -> String {
        let title = bundle
            .project
            .as_ref()
            .map(|p| format!("# {} — context\n\n", p.name))
            .unwrap_or_else(|| "# Context\n\n".to_string());
        format!("{title}{}", render::markdown(bundle, &crate::agents::render_options()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::context::BudgetReport;

    #[test]
    fn renders_plain_markdown() {
        let bundle = ContextBundle {
            project: None,
            query: None,
            checkpoint: None,
            decisions: vec![],
            superseded: vec![],
            memories: vec![],
            budget: BudgetReport::default(),
        };
        let text = GenericAdapter.render(&bundle);
        assert!(text.starts_with("# Context"));
        assert!(!text.contains("---"), "no front matter in the generic form");
    }

    #[test]
    fn export_path_is_context_md() {
        assert_eq!(GenericAdapter.export_path(Path::new("/repo")), Path::new("/repo/CONTEXT.md"));
    }
}
