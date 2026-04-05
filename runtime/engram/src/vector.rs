//! `VectorStore` trait — semantic similarity search interface for Engram.
//!
//! Implementations store dense embedding vectors alongside metadata and
//! support nearest-neighbour queries via cosine similarity. The trait is
//! `async_trait`-annotated and requires `Send + Sync` for use across tasks.

use crate::fact::FactId;
use crate::scope::Scope;
use crate::store::MemoryError;
use async_trait::async_trait;

// ---------------------------------------------------------------------------
// VectorFilter
// ---------------------------------------------------------------------------

/// Optional filters applied during a vector similarity search.
#[derive(Debug, Clone, Default)]
pub struct VectorFilter {
    /// Restrict results to vectors belonging to this scope.
    pub scope: Option<Scope>,
    /// Exclude results whose cosine similarity score is below this threshold.
    pub min_score: Option<f32>,
}

// ---------------------------------------------------------------------------
// VectorMatch
// ---------------------------------------------------------------------------

/// A single result returned by `VectorStore::search`.
#[derive(Debug, Clone)]
pub struct VectorMatch {
    /// The fact this vector belongs to.
    pub id: FactId,
    /// Cosine similarity score in [0.0, 1.0].
    pub score: f32,
    /// Metadata stored alongside the embedding at upsert time.
    pub metadata: serde_json::Value,
}

// ---------------------------------------------------------------------------
// VectorStore trait
// ---------------------------------------------------------------------------

/// Semantic vector storage and nearest-neighbour search.
///
/// Implementations MUST be `Send + Sync` so that `Arc<dyn VectorStore>` can
/// be shared across async tasks. All methods are fallible and return
/// `Result<_, MemoryError>`.
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Insert or replace the embedding for `id`.
    ///
    /// `embedding` must have the correct dimensionality for the store;
    /// implementations SHOULD return `MemoryError::Embedding` on a mismatch.
    async fn upsert(
        &self,
        id: FactId,
        embedding: Vec<f32>,
        metadata: serde_json::Value,
    ) -> Result<(), MemoryError>;

    /// Return the top-`top_k` most similar vectors to `query`, applying
    /// `filter` constraints. Results are ordered by descending similarity score.
    async fn search(
        &self,
        query: &[f32],
        filter: &VectorFilter,
        top_k: usize,
    ) -> Result<Vec<VectorMatch>, MemoryError>;

    /// Delete the embedding for `id`. Returns `Ok(())` if the id was not found
    /// (idempotent).
    async fn delete(&self, id: FactId) -> Result<(), MemoryError>;

    /// Delete all embeddings associated with `scope`.
    ///
    /// Returns the number of entries removed.
    async fn delete_by_scope(&self, scope: &Scope) -> Result<u64, MemoryError>;
}
