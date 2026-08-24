//! The provider abstraction.

use std::future::Future;
use std::pin::Pin;

use crate::error::Result;

/// Boxed future returned by [`EmbeddingProvider::embed`].
///
/// A hand-rolled boxed future keeps the trait object-safe without depending on
/// `async-trait`, so `Arc<dyn EmbeddingProvider>` can be stored in the app.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Turns text into vectors.
///
/// Implementations must be deterministic for a given `(id, model)` pair, since
/// stored vectors are only re-computed when the text, provider or model
/// changes.
pub trait EmbeddingProvider: Send + Sync {
    /// Stable provider identifier, stored alongside each vector.
    fn id(&self) -> &str;

    /// Model identifier, stored alongside each vector.
    fn model(&self) -> &str;

    /// Vector width.
    fn dimensions(&self) -> usize;

    /// Whether the provider reaches the network. Used to decide whether an
    /// operation should be attempted in an offline context.
    fn is_remote(&self) -> bool {
        false
    }

    /// Embed a batch. The result has one vector per input, in order.
    fn embed<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, Result<Vec<Vec<f32>>>>;
}

/// Embed a single text.
pub async fn embed_one(provider: &dyn EmbeddingProvider, text: &str) -> Result<Vec<f32>> {
    let batch = vec![text.to_string()];
    let mut vectors = provider.embed(&batch).await?;
    Ok(if vectors.is_empty() { Vec::new() } else { vectors.remove(0) })
}
