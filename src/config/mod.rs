//! Configuration and on-disk layout.
//!
//! Everything ContextD owns lives under a single root directory (`~/.contextd`
//! by default). The root is resolved at runtime — never hardcoded — so tests,
//! portable installs and Windows all work through the same code path.

mod paths;

pub use paths::Paths;

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::{Error, Result};

/// The full configuration file (`<root>/config.toml`).
///
/// Every section has a `Default`, and unknown/missing keys fall back to it, so
/// upgrading ContextD never invalidates an existing config file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub embeddings: EmbeddingConfig,
    pub vector: VectorConfig,
    pub search: SearchConfig,
    pub context: ContextConfig,
    pub refresh: RefreshConfig,
    pub sync: SyncConfig,
    /// Machines whose memory can be pulled or pushed over SSH.
    #[serde(default, rename = "remote")]
    pub remotes: Vec<RemoteConfig>,
}

/// General behaviour.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    /// Agent used when `contextd export` is called without a target.
    pub default_agent: String,
    /// Colour output: `auto`, `always` or `never`.
    pub color: String,
}

impl Default for General {
    fn default() -> Self {
        Self { default_agent: "claude".into(), color: "auto".into() }
    }
}

/// Which embedding backend to use. Providers are pluggable; nothing in the
/// storage or MCP layers knows which one is active.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingConfig {
    /// `local`, `openai` (any OpenAI-compatible endpoint) or `none`.
    pub provider: String,
    /// Model identifier passed to remote providers.
    pub model: String,
    /// Vector width. Only meaningful for the local provider; remote providers
    /// report their own dimension.
    pub dimensions: usize,
    /// Base URL for OpenAI-compatible endpoints (Ollama, vLLM, LM Studio, …).
    pub api_base: String,
    /// Name of the environment variable holding the API key. The key itself is
    /// deliberately never written to the config file.
    pub api_key_env: String,
    /// Request timeout in seconds.
    pub timeout_secs: u64,
    /// Number of texts sent per request when embedding in bulk.
    pub batch_size: usize,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: "local".into(),
            model: "text-embedding-3-small".into(),
            dimensions: 384,
            api_base: "https://api.openai.com/v1".into(),
            api_key_env: "OPENAI_API_KEY".into(),
            timeout_secs: 30,
            batch_size: 32,
        }
    }
}

/// Where vectors are indexed for similarity search.
///
/// SQLite is the default and needs nothing installed: a brute-force cosine
/// scan over the stored vectors, which is sub-millisecond for a personal
/// store. Qdrant is for people who already run one, or whose memory has grown
/// past what a scan should handle.
///
/// Either way SQLite remains the authoritative copy of every vector, so the
/// external index can always be rebuilt and `contextd bundle` keeps working.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct VectorConfig {
    /// `sqlite` (default) or `qdrant`.
    pub backend: String,
    /// Base URL of the Qdrant REST API.
    pub url: String,
    /// Collection name; created on first use if missing.
    pub collection: String,
    /// Environment variable holding the Qdrant API key, when one is needed.
    pub api_key_env: String,
    pub timeout_secs: u64,
    /// Points sent per upsert request.
    pub batch_size: usize,
}

impl Default for VectorConfig {
    fn default() -> Self {
        Self {
            backend: "sqlite".into(),
            url: "http://localhost:6333".into(),
            collection: "contextd".into(),
            api_key_env: "QDRANT_API_KEY".into(),
            timeout_secs: 15,
            batch_size: 64,
        }
    }
}

impl VectorConfig {
    /// Whether an external service is in play.
    pub fn is_external(&self) -> bool {
        !matches!(self.backend.trim().to_lowercase().as_str(), "sqlite" | "" | "none")
    }
}

/// Hybrid retrieval weights. Exposed in the config file so ranking can be
/// tuned without a rebuild.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    pub fts_weight: f64,
    pub semantic_weight: f64,
    pub priority_weight: f64,
    pub recency_weight: f64,
    pub project_weight: f64,
    /// Recency score halves every this many days.
    pub recency_half_life_days: f64,
    /// Multiplier applied to superseded/deprecated memories so current truth
    /// outranks historical truth even when the history is more verbose.
    pub superseded_penalty: f64,
    pub archived_penalty: f64,
    /// Candidates pulled from each retrieval arm before fusion.
    pub candidate_limit: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            fts_weight: 1.0,
            semantic_weight: 1.0,
            priority_weight: 0.35,
            recency_weight: 0.25,
            project_weight: 0.5,
            recency_half_life_days: 90.0,
            superseded_penalty: 0.35,
            archived_penalty: 0.15,
            candidate_limit: 100,
        }
    }
}

/// Context budget. "Store everything, inject only what matters."
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    pub max_context_tokens: usize,
    pub max_memories: usize,
    /// Tokens reserved for the checkpoint/resume header before memories are
    /// packed, so the current goal is never crowded out by history.
    pub reserve_checkpoint_tokens: usize,
    /// Include superseded decisions in exported context (as history).
    pub include_superseded: bool,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_context_tokens: 6000,
            max_memories: 40,
            reserve_checkpoint_tokens: 800,
            include_superseded: true,
        }
    }
}

/// `contextd refresh` thresholds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RefreshConfig {
    /// At or above this similarity two memories are treated as duplicates.
    pub duplicate_threshold: f64,
    /// At or above this similarity they are merely related (reported, not merged).
    pub similar_threshold: f64,
    /// Summarisation provider: `none` or `openai`.
    pub summarizer: String,
    pub summarizer_model: String,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            duplicate_threshold: 0.9,
            similar_threshold: 0.65,
            summarizer: "none".into(),
            summarizer_model: "gpt-4o-mini".into(),
        }
    }
}

/// Markdown mirror settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    /// Write the human-readable Markdown mirror on every mutation.
    pub auto_export_markdown: bool,
    /// Never overwrite agent files (CLAUDE.md, AGENTS.md, …) that changed
    /// outside ContextD without an explicit `--force`.
    pub protect_agent_files: bool,
    /// How long deletion records are kept before `contextd refresh` forgets
    /// them.
    ///
    /// A tombstone is what stops a deleted memory coming back on the next
    /// sync, so it must outlive the longest gap between syncs: a machine that
    /// has not synced since before a tombstone was forgotten will hand the
    /// record back. A year is generous for a personal setup; lower it only if
    /// every machine syncs often.
    pub tombstone_retention_days: i64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            auto_export_markdown: false,
            protect_agent_files: true,
            tombstone_retention_days: 365,
        }
    }
}

/// One machine ContextD can exchange memory with.
///
/// Remotes live in the config file rather than the database: they describe
/// *this* machine's view of the network, not memory, and a developer should be
/// able to add one with an editor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteConfig {
    /// Short name used on the command line.
    pub name: String,
    /// SSH destination, e.g. `dev@lab-box` or a `~/.ssh/config` host alias.
    pub host: String,
    /// How to invoke ContextD over there.
    pub command: String,
    /// `CONTEXTD_HOME` on the remote machine, when it is not the default.
    pub home: Option<String>,
    /// Extra arguments passed to `ssh` (`-p 2222`, `-i key`, `-J jump`, …).
    pub ssh_options: Vec<String>,
    /// Run the remote command through a login shell.
    ///
    /// `ssh host command` uses a non-interactive, non-login shell, which on a
    /// stock Ubuntu never reaches the part of `~/.bashrc` that adds
    /// `~/.local/bin` or `~/.cargo/bin` to `PATH`. A login shell reads
    /// `~/.profile` and finds them.
    pub login_shell: bool,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            command: "contextd".into(),
            home: None,
            ssh_options: Vec::new(),
            login_shell: false,
        }
    }
}

impl RemoteConfig {
    pub fn new(name: impl Into<String>, host: impl Into<String>) -> Self {
        Self { name: name.into(), host: host.into(), ..Default::default() }
    }

    /// Reject the shapes that would produce a confusing failure much later.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::invalid("remote.name", "must not be empty"));
        }
        if self.host.trim().is_empty() {
            return Err(Error::invalid("remote.host", "must not be empty"));
        }
        if self.command.trim().is_empty() {
            return Err(Error::invalid("remote.command", "must not be empty"));
        }
        Ok(())
    }
}

impl Config {
    /// Find a configured remote by name.
    pub fn remote(&self, name: &str) -> Option<&RemoteConfig> {
        self.remotes.iter().find(|r| r.name.eq_ignore_ascii_case(name.trim()))
    }

    /// Add or replace a remote by name.
    pub fn upsert_remote(&mut self, remote: RemoteConfig) -> Result<()> {
        remote.validate()?;
        match self.remotes.iter_mut().find(|r| r.name.eq_ignore_ascii_case(&remote.name)) {
            Some(existing) => *existing = remote,
            None => self.remotes.push(remote),
        }
        Ok(())
    }

    /// Remove a remote, reporting whether it existed.
    pub fn remove_remote(&mut self, name: &str) -> bool {
        let before = self.remotes.len();
        self.remotes.retain(|r| !r.name.eq_ignore_ascii_case(name.trim()));
        self.remotes.len() != before
    }

    /// Load from `path`, or return defaults when the file does not exist.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::io(path, e)),
        }
    }

    /// Persist to `path`, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("cannot serialise config: {e}")))?;
        std::fs::write(path, text).map_err(|e| Error::io(path, e))
    }

    /// Validate ranges that would otherwise produce nonsensical rankings.
    pub fn validate(&self) -> Result<()> {
        if self.context.max_context_tokens == 0 {
            return Err(Error::invalid("context.max_context_tokens", "must be greater than 0"));
        }
        if self.embeddings.dimensions == 0 {
            return Err(Error::invalid("embeddings.dimensions", "must be greater than 0"));
        }
        if !(0.0..=1.0).contains(&self.refresh.duplicate_threshold) {
            return Err(Error::invalid("refresh.duplicate_threshold", "must be within 0.0..=1.0"));
        }
        if self.refresh.similar_threshold > self.refresh.duplicate_threshold {
            return Err(Error::invalid(
                "refresh.similar_threshold",
                "must not exceed refresh.duplicate_threshold",
            ));
        }
        if self.vector.is_external() && self.vector.url.trim().is_empty() {
            return Err(Error::invalid("vector.url", "must not be empty for an external backend"));
        }
        if self.vector.is_external() && self.vector.collection.trim().is_empty() {
            return Err(Error::invalid("vector.collection", "must not be empty"));
        }
        if self.sync.tombstone_retention_days < 1 {
            return Err(Error::invalid(
                "sync.tombstone_retention_days",
                "must be at least 1; deletions would otherwise be forgotten immediately \
                 and deleted memories would return on the next sync",
            ));
        }
        for remote in &self.remotes {
            remote.validate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config::default();
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(&dir.path().join("nope.toml")).unwrap(), Config::default());
    }

    #[test]
    fn partial_config_keeps_other_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[context]\nmax_context_tokens = 100\n").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.context.max_context_tokens, 100);
        assert_eq!(cfg.embeddings.provider, "local");
    }

    #[test]
    fn tombstone_retention_must_be_positive() {
        let mut config = Config::default();
        assert_eq!(config.sync.tombstone_retention_days, 365);
        config.sync.tombstone_retention_days = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn vector_backend_defaults_to_sqlite() {
        let config = VectorConfig::default();
        assert!(!config.is_external());
        assert!(VectorConfig { backend: "qdrant".into(), ..VectorConfig::default() }.is_external());
    }

    #[test]
    fn external_vector_backend_needs_a_url_and_collection() {
        let mut config = Config {
            vector: VectorConfig {
                backend: "qdrant".into(),
                url: "  ".into(),
                ..Default::default()
            },
            ..Config::default()
        };
        assert!(config.validate().is_err());
        config.vector.url = "http://localhost:6333".into();
        assert!(config.validate().is_ok());
        config.vector.collection = String::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn remotes_roundtrip_and_are_addressable_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        config
            .upsert_remote(RemoteConfig {
                ssh_options: vec!["-p".into(), "2222".into()],
                ..RemoteConfig::new("lab", "dev@lab-box")
            })
            .unwrap();
        config.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, config);
        assert_eq!(loaded.remote("LAB").unwrap().host, "dev@lab-box");
        assert!(loaded.remote("nope").is_none());
    }

    #[test]
    fn upsert_replaces_and_remove_reports() {
        let mut config = Config::default();
        config.upsert_remote(RemoteConfig::new("lab", "old-host")).unwrap();
        config.upsert_remote(RemoteConfig::new("lab", "new-host")).unwrap();
        assert_eq!(config.remotes.len(), 1);
        assert_eq!(config.remote("lab").unwrap().host, "new-host");

        assert!(config.remove_remote("lab"));
        assert!(!config.remove_remote("lab"));
    }

    #[test]
    fn invalid_remotes_are_rejected() {
        let mut config = Config::default();
        assert!(config.upsert_remote(RemoteConfig::new("", "host")).is_err());
        assert!(config.upsert_remote(RemoteConfig::new("lab", "  ")).is_err());
        config.remotes.push(RemoteConfig::new("broken", ""));
        assert!(config.validate().is_err());
    }

    #[test]
    fn validation_rejects_inverted_thresholds() {
        let mut cfg = Config::default();
        cfg.refresh.similar_threshold = 0.99;
        cfg.refresh.duplicate_threshold = 0.5;
        assert!(cfg.validate().is_err());
    }
}
