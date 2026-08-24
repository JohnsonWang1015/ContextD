//! Resolution of the ContextD root and everything inside it.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Environment variable that overrides the root directory. Used by tests and
/// by users who keep their memory in a synced folder.
pub const HOME_ENV: &str = "CONTEXTD_HOME";

/// On-disk layout.
///
/// ```text
/// <root>/config.toml
/// <root>/contextd.db
/// <root>/projects/<Project>/{overview,architecture,decisions,tasks}.md
/// <root>/projects/<Project>/checkpoints/
/// <root>/global/{coding,git,preferences}.md
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// Resolve from the environment: `$CONTEXTD_HOME`, else `~/.contextd`.
    pub fn resolve() -> Result<Self> {
        if let Some(dir) = std::env::var_os(HOME_ENV) {
            let dir = PathBuf::from(dir);
            if dir.as_os_str().is_empty() {
                return Err(Error::Config(format!("{HOME_ENV} is set but empty")));
            }
            return Ok(Self::with_root(dir));
        }
        let home =
            directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()).ok_or_else(|| {
                Error::Config(format!(
                    "cannot determine your home directory; set {HOME_ENV} to choose a location"
                ))
            })?;
        Ok(Self::with_root(home.join(".contextd")))
    }

    /// Use an explicit root (`--home`, tests).
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn database(&self) -> PathBuf {
        self.root.join("contextd.db")
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    pub fn global_dir(&self) -> PathBuf {
        self.root.join("global")
    }

    /// Markdown mirror directory for one project.
    pub fn project_dir(&self, project_name: &str) -> PathBuf {
        self.projects_dir().join(sanitize_component(project_name))
    }

    pub fn project_checkpoints_dir(&self, project_name: &str) -> PathBuf {
        self.project_dir(project_name).join("checkpoints")
    }

    /// True when `contextd init` has been run.
    pub fn is_initialised(&self) -> bool {
        self.database().exists()
    }

    /// Fail with a helpful message when not initialised.
    pub fn require_initialised(&self) -> Result<()> {
        if self.is_initialised() {
            Ok(())
        } else {
            Err(Error::NotInitialised)
        }
    }

    /// Create the directory skeleton.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [self.root.clone(), self.projects_dir(), self.global_dir()] {
            std::fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
        }
        Ok(())
    }
}

/// Make a project name safe as a single path component on every platform.
///
/// Windows additionally forbids a set of reserved device names, and trailing
/// dots/spaces, so those are handled too.
pub fn sanitize_component(name: &str) -> String {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let mut cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    while cleaned.ends_with('.') || cleaned.ends_with(' ') {
        cleaned.pop();
    }
    if cleaned.is_empty() {
        cleaned.push_str("unnamed");
    }
    let stem = cleaned.split('.').next().unwrap_or(&cleaned).to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        cleaned.insert(0, '_');
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_rooted() {
        let p = Paths::with_root("/tmp/cd");
        assert_eq!(p.database(), Path::new("/tmp/cd/contextd.db"));
        assert_eq!(p.config_file(), Path::new("/tmp/cd/config.toml"));
        assert_eq!(p.project_dir("FerroGrid"), Path::new("/tmp/cd/projects/FerroGrid"));
    }

    #[test]
    fn sanitize_blocks_traversal_and_reserved_names() {
        assert_eq!(sanitize_component("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_component("CON"), "_CON");
        assert_eq!(sanitize_component("nul.txt"), "_nul.txt");
        assert_eq!(sanitize_component("trailing. "), "trailing");
        assert_eq!(sanitize_component(""), "unnamed");
    }

    #[test]
    fn ensure_dirs_creates_skeleton() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::with_root(dir.path().join("root"));
        p.ensure_dirs().unwrap();
        assert!(p.projects_dir().is_dir());
        assert!(p.global_dir().is_dir());
        assert!(!p.is_initialised());
    }
}
