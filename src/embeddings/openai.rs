//! OpenAI-compatible embedding endpoint.
//!
//! The same wire format is spoken by OpenAI, Ollama (`/v1`), vLLM, LM Studio,
//! LocalAI and most gateways, so one implementation covers all of them; the
//! endpoint is configuration, not code. The API key is read from an
//! environment variable named in the config — never stored in the config file
//! and never written to the database.

use serde::{Deserialize, Serialize};

use crate::config::EmbeddingConfig;
use crate::embeddings::provider::{BoxFuture, EmbeddingProvider};
use crate::error::{Error, Result};

/// Client for an OpenAI-compatible `/embeddings` endpoint.
#[derive(Debug)]
pub struct OpenAiEmbedder {
    endpoint: String,
    model: String,
    api_key: Option<String>,
    dimensions: usize,
    batch_size: usize,
    client: reqwest::Client,
}

impl OpenAiEmbedder {
    /// Build from configuration.
    ///
    /// A missing API key is not an error here: local gateways such as Ollama
    /// need none, and a remote endpoint will report the problem itself with a
    /// far clearer message than a guess made at startup.
    pub fn from_config(config: &EmbeddingConfig) -> Result<Self> {
        let base = config.api_base.trim().trim_end_matches('/');
        if base.is_empty() {
            return Err(Error::EmbeddingProvider(
                "openai".into(),
                "embeddings.api_base must not be empty".into(),
            ));
        }
        let api_key = std::env::var(&config.api_key_env).ok().filter(|k| !k.trim().is_empty());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs.max(1)))
            .build()
            .map_err(|e| Error::EmbeddingProvider("openai".into(), e.to_string()))?;

        Ok(Self {
            endpoint: format!("{base}/embeddings"),
            model: config.model.clone(),
            api_key,
            dimensions: config.dimensions,
            batch_size: config.batch_size.max(1),
            client,
        })
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut request = self
            .client
            .post(&self.endpoint)
            .json(&EmbeddingRequest { model: &self.model, input: texts });
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }

        let response = request
            .send()
            .await
            .map_err(|e| Error::EmbeddingProvider("openai".into(), e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::EmbeddingProvider(
                "openai".into(),
                format!("{} from {}: {}", status, self.endpoint, body.trim()),
            ));
        }

        let parsed: EmbeddingResponse = response
            .json()
            .await
            .map_err(|e| Error::EmbeddingProvider("openai".into(), e.to_string()))?;

        // The API is documented to echo the input order, but `index` is
        // authoritative; sorting by it keeps vectors attached to the right
        // records even if a gateway reorders them.
        let mut data = parsed.data;
        data.sort_by_key(|d| d.index);
        if data.len() != texts.len() {
            return Err(Error::EmbeddingProvider(
                "openai".into(),
                format!("expected {} vectors, received {}", texts.len(), data.len()),
            ));
        }
        Ok(data.into_iter().map(|d| d.embedding).collect())
    }
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

impl EmbeddingProvider for OpenAiEmbedder {
    fn id(&self) -> &str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    /// The configured width; the true width is whatever the endpoint returns
    /// and is recorded per vector when stored.
    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn is_remote(&self) -> bool {
        true
    }

    fn embed<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(texts.len());
            for chunk in texts.chunks(self.batch_size) {
                out.extend(self.embed_batch(chunk).await?);
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_is_built_from_base() {
        let config = EmbeddingConfig {
            api_base: "http://localhost:11434/v1/".into(),
            ..EmbeddingConfig::default()
        };
        let embedder = OpenAiEmbedder::from_config(&config).unwrap();
        assert_eq!(embedder.endpoint, "http://localhost:11434/v1/embeddings");
        assert!(embedder.is_remote());
    }

    #[test]
    fn empty_base_is_rejected() {
        let config = EmbeddingConfig { api_base: "  ".into(), ..EmbeddingConfig::default() };
        assert!(OpenAiEmbedder::from_config(&config).is_err());
    }

    #[test]
    fn api_key_comes_from_the_named_environment_variable() {
        // SAFETY: single-threaded test process section; the variable name is
        // unique to this test.
        unsafe { std::env::set_var("CONTEXTD_TEST_KEY", "sk-test") };
        let config = EmbeddingConfig {
            api_key_env: "CONTEXTD_TEST_KEY".into(),
            ..EmbeddingConfig::default()
        };
        let embedder = OpenAiEmbedder::from_config(&config).unwrap();
        assert_eq!(embedder.api_key.as_deref(), Some("sk-test"));
        unsafe { std::env::remove_var("CONTEXTD_TEST_KEY") };

        let missing = EmbeddingConfig {
            api_key_env: "CONTEXTD_TEST_KEY_ABSENT".into(),
            ..EmbeddingConfig::default()
        };
        assert!(OpenAiEmbedder::from_config(&missing).unwrap().api_key.is_none());
    }

    #[test]
    fn response_vectors_are_ordered_by_index() {
        let json =
            r#"{"data":[{"index":1,"embedding":[0.0,1.0]},{"index":0,"embedding":[1.0,0.0]}]}"#;
        let mut parsed: EmbeddingResponse = serde_json::from_str(json).unwrap();
        parsed.data.sort_by_key(|d| d.index);
        assert_eq!(parsed.data[0].embedding, vec![1.0, 0.0]);
    }
}
