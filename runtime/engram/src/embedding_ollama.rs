//! Ollama-backed `EmbeddingProvider` implementation.
//!
//! Uses the Ollama `/api/embed` endpoint to embed text via a locally-running
//! Ollama instance. Defaults to `nomic-embed-text` (768 dimensions).

use crate::embedding::EmbeddingProvider;
use crate::store::MemoryError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
}

#[derive(Deserialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

// ---------------------------------------------------------------------------
// OllamaEmbeddingProvider
// ---------------------------------------------------------------------------

/// `EmbeddingProvider` backed by a locally-running Ollama instance.
///
/// # Construction
///
/// ```no_run
/// use engram::OllamaEmbeddingProvider;
///
/// // Use defaults: http://localhost:11434, nomic-embed-text, 768 dims
/// let provider = OllamaEmbeddingProvider::new();
///
/// // Or supply custom config
/// let provider = OllamaEmbeddingProvider::with_config(
///     "http://localhost:11434",
///     "nomic-embed-text",
///     768,
/// );
/// ```
pub struct OllamaEmbeddingProvider {
    base_url: String,
    model: String,
    dims: usize,
    client: reqwest::Client,
}

impl OllamaEmbeddingProvider {
    /// Create a provider with default settings:
    /// - base URL: `http://localhost:11434`
    /// - model: `nomic-embed-text`
    /// - dims: `768`
    pub fn new() -> Self {
        Self::with_config("http://localhost:11434", "nomic-embed-text", 768)
    }

    /// Create a provider with explicit configuration.
    pub fn with_config(base_url: &str, model: &str, dims: usize) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            dims,
            client: reqwest::Client::new(),
        }
    }
}

impl Default for OllamaEmbeddingProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EmbeddingProvider for OllamaEmbeddingProvider {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, MemoryError> {
        let url = format!("{}/api/embed", self.base_url);
        let body = EmbedRequest {
            model: &self.model,
            input: texts.to_vec(),
        };

        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| MemoryError::Embedding(format!("Ollama request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(MemoryError::Embedding(format!(
                "Ollama returned {status}: {text}"
            )));
        }

        let embed_response: EmbedResponse = response
            .json()
            .await
            .map_err(|e| MemoryError::Embedding(format!("Failed to parse Ollama response: {e}")))?;

        Ok(embed_response.embeddings)
    }

    fn dimensions(&self) -> usize {
        self.dims
    }
}
