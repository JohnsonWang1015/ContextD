//! Offline embedder based on feature hashing.
//!
//! This is a *lexical* vector, not a neural one: tokens and adjacent token
//! pairs are hashed into a fixed number of buckets with signed, sublinear term
//! frequencies, then L2-normalised. It captures overlap and phrasing, which is
//! enough for hybrid retrieval to work with no model download, no network and
//! no API key — the default a new user gets.
//!
//! It cannot relate words that never co-occur ("queue" ↔ "transport"). Users
//! who want true paraphrase matching point `embeddings.provider` at an
//! OpenAI-compatible endpoint; nothing else in ContextD changes.

use std::collections::HashMap;

use crate::embeddings::provider::{BoxFuture, EmbeddingProvider};
use crate::error::Result;
use crate::util::text;

/// Model identifier recorded with every vector this embedder produces.
/// Bump it if the algorithm changes, so existing vectors are re-computed.
pub const MODEL: &str = "hashing-v1";

/// Feature-hashing embedder.
#[derive(Debug, Clone)]
pub struct LocalEmbedder {
    dimensions: usize,
}

impl LocalEmbedder {
    pub fn new(dimensions: usize) -> Self {
        Self { dimensions: dimensions.max(16) }
    }

    /// Embed one text synchronously — the local path never needs to await.
    pub fn embed_text(&self, text_input: &str) -> Vec<f32> {
        let tokens = text::tokenize(text_input);
        if tokens.is_empty() {
            return vec![0.0; self.dimensions];
        }

        let mut counts: HashMap<String, f64> = HashMap::new();
        for token in &tokens {
            *counts.entry(token.clone()).or_insert(0.0) += 1.0;
        }
        // Bigrams get less weight than unigrams: they add word-order signal
        // without letting a long phrase dominate the vector.
        for bigram in text::bigrams(&tokens) {
            *counts.entry(bigram).or_insert(0.0) += 0.5;
        }

        let mut vector = vec![0.0_f32; self.dimensions];
        for (term, count) in counts {
            let hash = fnv1a(term.as_bytes());
            let bucket = (hash % self.dimensions as u64) as usize;
            // A second hash bit decides the sign, which keeps unrelated terms
            // from piling up constructively in the same bucket.
            let sign = if (hash >> 32) & 1 == 0 { 1.0 } else { -1.0 };
            let weight = 1.0 + count.ln_1p(); // sublinear term frequency
            vector[bucket] += (sign * weight) as f32;
        }

        normalise(&mut vector);
        vector
    }
}

fn normalise(vector: &mut [f32]) {
    let norm = vector.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>().sqrt();
    if norm > 0.0 {
        for value in vector.iter_mut() {
            *value = (*value as f64 / norm) as f32;
        }
    }
}

/// FNV-1a: stable across platforms and releases, unlike `DefaultHasher`.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl EmbeddingProvider for LocalEmbedder {
    fn id(&self) -> &str {
        "local"
    }

    fn model(&self) -> &str {
        MODEL
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn embed<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        Box::pin(async move { Ok(texts.iter().map(|t| self.embed_text(t)).collect()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::cosine_similarity;

    #[test]
    fn deterministic_and_normalised() {
        let embedder = LocalEmbedder::new(128);
        let a = embedder.embed_text("scheduler uses NATS");
        let b = embedder.embed_text("scheduler uses NATS");
        assert_eq!(a, b);
        let norm: f64 = a.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn overlapping_text_scores_higher_than_unrelated() {
        let embedder = LocalEmbedder::new(256);
        let query = embedder.embed_text("which message transport does the scheduler use");
        let related = embedder
            .embed_text("the scheduler transport was migrated to NATS after evaluating Redis");
        let unrelated = embedder.embed_text("the CLI prints a colourful status table");
        assert!(
            cosine_similarity(&query, &related) > cosine_similarity(&query, &unrelated),
            "related text must score higher"
        );
    }

    #[test]
    fn empty_text_yields_zero_vector() {
        let embedder = LocalEmbedder::new(32);
        let vector = embedder.embed_text("   ");
        assert_eq!(vector.len(), 32);
        assert!(vector.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn dimensions_have_a_floor() {
        assert_eq!(LocalEmbedder::new(1).dimensions(), 16);
    }

    #[tokio::test]
    async fn batch_matches_single() {
        let embedder = LocalEmbedder::new(64);
        let texts = vec!["one".to_string(), "two".to_string()];
        let batch = embedder.embed(&texts).await.unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0], embedder.embed_text("one"));
    }
}
