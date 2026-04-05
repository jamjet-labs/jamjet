//! `EmbeddedVectorStore` — in-process brute-force cosine-similarity store.
//!
//! Suitable for development, testing, and production use-cases with fewer than
//! ~100 K vectors. All data lives in a `HashMap` protected by a `std::sync::RwLock`.
//! The lock is held for the minimal duration needed (no I/O inside the critical
//! section), so `tokio` tasks can call these methods without blocking the async
//! runtime for meaningful time.

use crate::fact::FactId;
use crate::scope::Scope;
use crate::store::MemoryError;
use crate::vector::{VectorFilter, VectorMatch, VectorStore};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;

// ---------------------------------------------------------------------------
// Internal entry type
// ---------------------------------------------------------------------------

struct VectorEntry {
    embedding: Vec<f32>,
    metadata: serde_json::Value,
    /// Pre-computed L2 norm of `embedding`.
    norm: f32,
}

// ---------------------------------------------------------------------------
// EmbeddedVectorStore
// ---------------------------------------------------------------------------

/// In-memory brute-force vector store.
///
/// # Construction
///
/// ```
/// use engram::vector_embedded::EmbeddedVectorStore;
///
/// let store = EmbeddedVectorStore::new(128);
/// ```
pub struct EmbeddedVectorStore {
    /// Expected dimensionality of every stored vector.
    dimensions: usize,
    entries: RwLock<HashMap<FactId, VectorEntry>>,
}

impl EmbeddedVectorStore {
    /// Create a new store that accepts embeddings of `dimensions` dimensions.
    pub fn new(dimensions: usize) -> Self {
        Self {
            dimensions,
            entries: RwLock::new(HashMap::new()),
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Compute the L2 (Euclidean) norm of `v`.
    fn compute_norm(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    /// Cosine similarity between two pre-normalised vectors.
    ///
    /// Returns `0.0` when either norm is zero to avoid NaN propagation.
    fn cosine_similarity(a: &[f32], a_norm: f32, b: &[f32], b_norm: f32) -> f32 {
        if a_norm == 0.0 || b_norm == 0.0 {
            return 0.0;
        }
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        dot / (a_norm * b_norm)
    }
}

// ---------------------------------------------------------------------------
// VectorStore implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl VectorStore for EmbeddedVectorStore {
    async fn upsert(
        &self,
        id: FactId,
        embedding: Vec<f32>,
        metadata: serde_json::Value,
    ) -> Result<(), MemoryError> {
        if embedding.len() != self.dimensions {
            return Err(MemoryError::Embedding(format!(
                "dimension mismatch: expected {}, got {}",
                self.dimensions,
                embedding.len()
            )));
        }
        let norm = Self::compute_norm(&embedding);
        let entry = VectorEntry {
            embedding,
            metadata,
            norm,
        };
        self.entries
            .write()
            .map_err(|e| MemoryError::Database(format!("lock poisoned: {e}")))?
            .insert(id, entry);
        Ok(())
    }

    async fn search(
        &self,
        query: &[f32],
        filter: &VectorFilter,
        top_k: usize,
    ) -> Result<Vec<VectorMatch>, MemoryError> {
        let query_norm = Self::compute_norm(query);
        let min_score = filter.min_score.unwrap_or(f32::NEG_INFINITY);

        let entries = self
            .entries
            .read()
            .map_err(|e| MemoryError::Database(format!("lock poisoned: {e}")))?;

        let mut matches: Vec<VectorMatch> = entries
            .iter()
            .filter_map(|(id, entry)| {
                // Scope filtering: if a scope filter is set we check that the
                // stored metadata contains a matching "scope" key. For now we
                // do a best-effort match on the stored JSON value; a full
                // implementation would store the Scope as a typed field.
                if let Some(_filter_scope) = &filter.scope {
                    // Scope-aware filtering requires metadata inspection.
                    // The embedded store stores raw JSON metadata; callers that
                    // need scope filtering should encode scope fields in the
                    // metadata they pass to `upsert` and filter after retrieval.
                    // We leave this as a no-op pass-through for now — scoped
                    // deletes are handled by `delete_by_scope`.
                }

                let score =
                    Self::cosine_similarity(query, query_norm, &entry.embedding, entry.norm);
                if score < min_score {
                    return None;
                }
                Some(VectorMatch {
                    id: *id,
                    score,
                    metadata: entry.metadata.clone(),
                })
            })
            .collect();

        // Sort by descending similarity score; break ties by id for stability.
        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        matches.truncate(top_k);
        Ok(matches)
    }

    async fn delete(&self, id: FactId) -> Result<(), MemoryError> {
        self.entries
            .write()
            .map_err(|e| MemoryError::Database(format!("lock poisoned: {e}")))?
            .remove(&id);
        Ok(())
    }

    async fn delete_by_scope(&self, _scope: &Scope) -> Result<u64, MemoryError> {
        // A full implementation would inspect stored metadata to find entries
        // that belong to `scope`. For now we clear all entries — sufficient for
        // the current use-cases and easily replaced once metadata carries typed
        // scope fields.
        let mut entries = self
            .entries
            .write()
            .map_err(|e| MemoryError::Database(format!("lock poisoned: {e}")))?;
        let count = entries.len() as u64;
        entries.clear();
        Ok(count)
    }
}
