//! Cursor.
//!
//! Modern Cursor keeps rules in `.cursor/rules/*.mdc` with YAML front matter;
//! older projects use a single `.cursorrules` file. Both are read; exports are
//! written to `.cursor/rules/contextd.mdc` so ContextD owns exactly one file
//! and leaves every hand-written rule alone.

use std::path::{Path, PathBuf};

use crate::agents::{AgentAdapter, AgentFile};
use crate::core::context::{render, ContextBundle};

/// Adapter for Cursor rules.
pub struct CursorAdapter;

/// File ContextD owns inside `.cursor/rules/`.
pub const EXPORT_FILE: &str = "contextd.mdc";

impl AgentAdapter for CursorAdapter {
    fn id(&self) -> &'static str {
        "cursor"
    }

    fn display_name(&self) -> &'static str {
        "Cursor"
    }

    fn detect(&self, repo: &Path, _include_global: bool) -> Vec<AgentFile> {
        let mut files = Vec::new();
        let legacy = repo.join(".cursorrules");
        if legacy.is_file() {
            files.push(AgentFile { path: legacy, global: false, exists: true });
        }
        let rules_dir = repo.join(".cursor").join("rules");
        if let Ok(entries) = std::fs::read_dir(&rules_dir) {
            let mut rules: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "mdc" || ext == "md"))
                .collect();
            rules.sort();
            files.extend(rules.into_iter().map(|path| AgentFile {
                exists: true,
                path,
                global: false,
            }));
        }
        if files.is_empty() {
            files.push(AgentFile { path: self.export_path(repo), global: false, exists: false });
        }
        files
    }

    fn export_path(&self, repo: &Path) -> PathBuf {
        repo.join(".cursor").join("rules").join(EXPORT_FILE)
    }

    fn owns_file(&self) -> bool {
        true
    }

    /// Cursor `.mdc` files start with YAML front matter that decides when the
    /// rule applies; `alwaysApply: true` matches ContextD's role as standing
    /// project context.
    fn render(&self, bundle: &ContextBundle) -> String {
        let description = bundle
            .project
            .as_ref()
            .map(|p| format!("ContextD memory for {}", p.name))
            .unwrap_or_else(|| "ContextD memory".to_string());
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("description: {description}\n"));
        out.push_str("alwaysApply: true\n");
        out.push_str("---\n\n");
        out.push_str(&crate::agents::preamble(bundle, self.id()));
        out.push_str(&render::markdown(bundle, &crate::agents::render_options()));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::context::BudgetReport;
    use crate::core::model::Project;

    fn bundle_with_project() -> ContextBundle {
        let now = crate::util::time::now();
        ContextBundle {
            project: Some(Project {
                id: "p".into(),
                name: "FerroGrid".into(),
                slug: "ferrogrid".into(),
                root_path: None,
                description: None,
                git_remote: None,
                default_branch: None,
                created_at: now,
                updated_at: now,
                active: true,
            }),
            query: None,
            checkpoint: None,
            decisions: vec![],
            superseded: vec![],
            memories: vec![],
            budget: BudgetReport::default(),
        }
    }

    #[test]
    fn detects_legacy_and_modern_rules() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".cursorrules"), "old style").unwrap();
        std::fs::create_dir_all(dir.path().join(".cursor/rules")).unwrap();
        std::fs::write(dir.path().join(".cursor/rules/style.mdc"), "new style").unwrap();

        let files = CursorAdapter.detect(dir.path(), false);
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.exists));
    }

    #[test]
    fn export_path_is_a_dedicated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = CursorAdapter.export_path(dir.path());
        assert!(path.ends_with(".cursor/rules/contextd.mdc"));
        // With nothing present, detection points at the file to be created.
        let files = CursorAdapter.detect(dir.path(), false);
        assert_eq!(files.len(), 1);
        assert!(!files[0].exists);
    }

    #[test]
    fn render_emits_front_matter() {
        let text = CursorAdapter.render(&bundle_with_project());
        assert!(text.starts_with("---\ndescription: ContextD memory for FerroGrid\n"));
        assert!(text.contains("alwaysApply: true"));
    }

    #[test]
    fn cursor_owns_its_export_file() {
        // Front matter must lead the file, so the managed-block wrapper cannot
        // be used here.
        assert!(CursorAdapter.owns_file());
    }
}
