//! Embedding providers.
//!
//! ContextD never names a provider outside this module: everything else takes
//! an [`EmbeddingProvider`] trait object. Adding Ollama or a local ONNX model
//! later means adding one file here, not touching search, storage or MCP.

pub mod local;
pub mod openai;
pub mod provider;

pub use provider::{BoxFuture, EmbeddingProvider};

use std::sync::Arc;

use crate::config::EmbeddingConfig;
use crate::error::{Error, Result};

/// Build the configured provider.
///
/// `Ok(None)` means embeddings are switched off (`provider = "none"`), which
/// is a supported mode: ContextD then relies on full-text search alone.
pub fn build(config: &EmbeddingConfig) -> Result<Option<Arc<dyn EmbeddingProvider>>> {
    match config.provider.trim().to_lowercase().as_str() {
        "none" | "off" | "disabled" => Ok(None),
        "local" | "hashing" => {
            Ok(Some(Arc::new(local::LocalEmbedder::new(config.dimensions)) as Arc<_>))
        }
        "openai" | "openai-compatible" | "ollama" => {
            Ok(Some(Arc::new(openai::OpenAiEmbedder::from_config(config)?) as Arc<_>))
        }
        other => Err(Error::EmbeddingProvider(
            other.to_string(),
            "unknown provider (expected: local, openai, none)".into(),
        )),
    }
}

/// Cosine similarity, the metric used everywhere vectors are compared.
///
/// Returns 0.0 for mismatched or empty vectors rather than failing: a stale
/// vector from a provider with a different width should be ignored, not crash
/// a search.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f64;
    let mut norm_a = 0.0_f64;
    let mut norm_b = 0.0_f64;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += (*x as f64) * (*y as f64);
        norm_a += (*x as f64) * (*x as f64);
        norm_b += (*y as f64) * (*y as f64);
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a.sqrt() * norm_b.sqrt())).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basics() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-9);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-9);
        assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-9);
    }

    #[test]
    fn cosine_is_defensive() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn build_selects_providers() {
        let mut config = EmbeddingConfig::default();
        assert_eq!(build(&config).unwrap().unwrap().id(), "local");

        config.provider = "none".into();
        assert!(build(&config).unwrap().is_none());

        config.provider = "nonsense".into();
        assert!(build(&config).is_err());
    }
}
