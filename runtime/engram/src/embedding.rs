//! `EmbeddingProvider` trait — pluggable text-embedding interface for Engram.
//!
//! Any embedding backend (Ollama, OpenAI, local ONNX models, …) implements
//! this trait so that `Memory` and other Engram components can embed text
//! without coupling to a specific provider.

use crate::store::MemoryError;
use async_trait::async_trait;

// ---------------------------------------------------------------------------
// EmbeddingProvider trait
// ---------------------------------------------------------------------------

/// Pluggable text-embedding provider.
///
/// Implementations MUST be `Send + Sync` so that `Arc<dyn EmbeddingProvider>`
/// can be shared across async task boundaries.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed a batch of texts.
    ///
    /// Returns one vector per input text, in the same order.
    /// Each returned vector has length `self.dimensions()`.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, MemoryError>;

    /// The dimensionality of all vectors produced by this provider.
    fn dimensions(&self) -> usize;
}

// ---------------------------------------------------------------------------
// MockEmbeddingProvider — deterministic, test-only
// ---------------------------------------------------------------------------

/// Deterministic embedding provider for unit and integration tests.
///
/// Each input text is converted to a vector of length `dims` by cycling
/// through its bytes and applying the transformation:
///
/// ```text
/// value = (byte / 255.0) * 2.0 - 1.0
/// ```
///
/// This produces stable, reproducible vectors without requiring a real model.
pub struct MockEmbeddingProvider {
    dims: usize,
}

impl MockEmbeddingProvider {
    /// Create a new mock provider that produces `dims`-dimensional vectors.
    pub fn new(dims: usize) -> Self {
        Self { dims }
    }
}

#[async_trait]
impl EmbeddingProvider for MockEmbeddingProvider {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, MemoryError> {
        let embeddings = texts
            .iter()
            .map(|text| {
                let bytes = text.as_bytes();
                if bytes.is_empty() {
                    // All-zero vector for empty strings.
                    return vec![0.0_f32; self.dims];
                }
                (0..self.dims)
                    .map(|i| {
                        let byte = bytes[i % bytes.len()] as f32;
                        (byte / 255.0) * 2.0 - 1.0
                    })
                    .collect()
            })
            .collect();
        Ok(embeddings)
    }

    fn dimensions(&self) -> usize {
        self.dims
    }
}
