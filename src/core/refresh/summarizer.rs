//! Pluggable summarisation.
//!
//! `contextd refresh` can consolidate a cluster of related memories into one
//! statement of what is currently true. Doing that well needs a language
//! model, which not every user wants to involve, so summarisation is a
//! provider behind a trait: the default does nothing and refresh still works.

use serde::{Deserialize, Serialize};

use crate::config::RefreshConfig;
use crate::embeddings::provider::BoxFuture;
use crate::error::{Error, Result};

/// A cluster of memories to consolidate.
#[derive(Debug, Clone)]
pub struct Cluster {
    pub topic: String,
    /// Newest first — the first entry is the most likely current truth.
    pub statements: Vec<String>,
}

/// Consolidates related memories.
pub trait Summarizer: Send + Sync {
    fn id(&self) -> &str;

    /// Return a consolidated statement, or `None` to leave the cluster alone.
    fn summarize<'a>(&'a self, cluster: &'a Cluster) -> BoxFuture<'a, Result<Option<String>>>;
}

/// Default: no summarisation, no network, no surprises.
pub struct NoopSummarizer;

impl Summarizer for NoopSummarizer {
    fn id(&self) -> &str {
        "none"
    }

    fn summarize<'a>(&'a self, _cluster: &'a Cluster) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async { Ok(None) })
    }
}

/// Summariser backed by an OpenAI-compatible chat completions endpoint.
pub struct ChatSummarizer {
    endpoint: String,
    model: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl ChatSummarizer {
    pub fn new(api_base: &str, model: &str, api_key_env: &str, timeout_secs: u64) -> Result<Self> {
        let base = api_base.trim().trim_end_matches('/');
        if base.is_empty() {
            return Err(Error::Config("refresh summarizer needs embeddings.api_base".into()));
        }
        Ok(Self {
            endpoint: format!("{base}/chat/completions"),
            model: model.to_string(),
            api_key: std::env::var(api_key_env).ok().filter(|k| !k.trim().is_empty()),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
                .build()
                .map_err(|e| Error::Config(e.to_string()))?,
        })
    }

    /// The prompt states ContextD's rule explicitly: the newest statement wins
    /// and the others are history, so the model cannot "average" a migration
    /// into a system that uses all three transports at once.
    fn prompt(cluster: &Cluster) -> String {
        let mut prompt = String::from(
            "You maintain an engineering memory. Below are statements about one topic, \
             newest first. Reply with one short paragraph stating what is CURRENTLY true, \
             then a line starting with 'Superseded:' listing what is no longer true. \
             The newest statement wins any conflict. Do not invent details.\n\n",
        );
        prompt.push_str(&format!("Topic: {}\n\n", cluster.topic));
        for (index, statement) in cluster.statements.iter().enumerate() {
            prompt.push_str(&format!("{}. {}\n", index + 1, statement.trim()));
        }
        prompt
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: String,
}

impl Summarizer for ChatSummarizer {
    fn id(&self) -> &str {
        "openai"
    }

    fn summarize<'a>(&'a self, cluster: &'a Cluster) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async move {
            if cluster.statements.len() < 2 {
                return Ok(None);
            }
            let prompt = Self::prompt(cluster);
            let mut request = self.client.post(&self.endpoint).json(&ChatRequest {
                model: &self.model,
                messages: vec![ChatMessage { role: "user", content: &prompt }],
                temperature: 0.0,
            });
            if let Some(key) = &self.api_key {
                request = request.bearer_auth(key);
            }
            let response = request
                .send()
                .await
                .map_err(|e| Error::EmbeddingProvider("summarizer".into(), e.to_string()))?;
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(Error::EmbeddingProvider(
                    "summarizer".into(),
                    format!("{status}: {}", body.trim()),
                ));
            }
            let parsed: ChatResponse = response
                .json()
                .await
                .map_err(|e| Error::EmbeddingProvider("summarizer".into(), e.to_string()))?;
            Ok(parsed
                .choices
                .into_iter()
                .next()
                .map(|c| c.message.content.trim().to_string())
                .filter(|c| !c.is_empty()))
        })
    }
}

/// Build the configured summariser.
pub fn build(
    refresh: &RefreshConfig,
    embeddings: &crate::config::EmbeddingConfig,
) -> Result<Box<dyn Summarizer>> {
    match refresh.summarizer.trim().to_lowercase().as_str() {
        "none" | "off" | "" => Ok(Box::new(NoopSummarizer)),
        "openai" | "chat" => Ok(Box::new(ChatSummarizer::new(
            &embeddings.api_base,
            &refresh.summarizer_model,
            &embeddings.api_key_env,
            embeddings.timeout_secs,
        )?)),
        other => Err(Error::Config(format!(
            "unknown refresh.summarizer `{other}` (expected: none, openai)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_returns_nothing() {
        let cluster = Cluster { topic: "queue".into(), statements: vec!["a".into(), "b".into()] };
        assert!(NoopSummarizer.summarize(&cluster).await.unwrap().is_none());
    }

    #[test]
    fn default_config_selects_noop() {
        let summarizer =
            build(&RefreshConfig::default(), &crate::config::EmbeddingConfig::default()).unwrap();
        assert_eq!(summarizer.id(), "none");
    }

    #[test]
    fn unknown_summarizer_is_rejected() {
        let config = RefreshConfig { summarizer: "magic".into(), ..RefreshConfig::default() };
        assert!(build(&config, &crate::config::EmbeddingConfig::default()).is_err());
    }

    #[test]
    fn prompt_states_the_newest_wins_rule() {
        let cluster = Cluster {
            topic: "Task queue".into(),
            statements: vec!["NATS".into(), "PostgreSQL".into(), "Redis".into()],
        };
        let prompt = ChatSummarizer::prompt(&cluster);
        assert!(prompt.contains("newest first"));
        assert!(prompt.contains("Superseded:"));
        assert!(prompt.contains("1. NATS"));
    }
}
