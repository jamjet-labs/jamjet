//! `FactStore` trait — the primary storage interface for Engram.
//!
//! All persistence implementations (SQLite, Postgres, in-memory) must
//! implement this trait. The trait is `async_trait`-annotated and requires
//! `Send + Sync` so it can be used across task boundaries.

use crate::fact::{Fact, FactFilter, FactId, FactPatch};
use crate::scope::Scope;
use async_trait::async_trait;
use serde::Serialize;
use thiserror::Error;

// ---------------------------------------------------------------------------
// MemoryError
// ---------------------------------------------------------------------------

/// Errors that can be returned by `FactStore` operations.
#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("database error: {0}")]
    Database(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("embedding error: {0}")]
    Embedding(String),

    #[error("graph error: {0}")]
    Graph(String),
}

// ---------------------------------------------------------------------------
// StoreStats
// ---------------------------------------------------------------------------

/// Aggregate statistics for a `FactStore` instance.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StoreStats {
    pub total_facts: u64,
    pub valid_facts: u64,
    pub invalidated_facts: u64,
    pub total_entities: u64,
    pub total_relationships: u64,
}

// ---------------------------------------------------------------------------
// FactStore trait
// ---------------------------------------------------------------------------

/// Primary storage interface for Engram facts.
///
/// Implementations MUST be `Send + Sync` so that `Arc<dyn FactStore>` can be
/// shared across async tasks. All mutation methods are fallible and return
/// `Result<_, MemoryError>`.
#[async_trait]
pub trait FactStore: Send + Sync {
    /// Persist a new fact. The `fact.id` must be unique; implementations SHOULD
    /// return `MemoryError::Database` if a duplicate id is detected.
    async fn insert_fact(&self, fact: Fact) -> Result<FactId, MemoryError>;

    /// Retrieve a single fact by id.
    async fn get_fact(&self, id: FactId) -> Result<Fact, MemoryError>;

    /// Apply a partial patch to an existing fact.
    /// Only fields set to `Some(…)` in `patch` are updated.
    async fn update_fact(&self, id: FactId, patch: FactPatch) -> Result<Fact, MemoryError>;

    /// List facts matching the given filter.
    async fn list_facts(&self, filter: &FactFilter) -> Result<Vec<Fact>, MemoryError>;

    /// Mark a fact as invalid as of `now` (sets `invalid_at` to the current
    /// timestamp). Does not delete the record; historical queries still see it.
    async fn invalidate_fact(&self, id: FactId) -> Result<(), MemoryError>;

    /// Delete ALL data (facts, entities, relationships) belonging to `scope`.
    /// This is a hard delete and is typically used for GDPR / right-to-erasure
    /// requests. Returns the number of facts deleted.
    async fn delete_scope_data(&self, scope: &Scope) -> Result<u64, MemoryError>;

    /// Export all facts matching `filter` as a JSON-serialisable vector.
    /// Implementations SHOULD stream or batch internally to avoid loading
    /// unbounded data into memory when the result set is large.
    async fn export(&self, filter: &FactFilter) -> Result<Vec<Fact>, MemoryError>;

    /// Import a batch of facts (e.g. from a previous `export`).
    /// Existing facts with the same id SHOULD be skipped (upsert-or-ignore).
    /// Returns the number of facts successfully imported.
    async fn import(&self, facts: Vec<Fact>) -> Result<u64, MemoryError>;

    /// Return aggregate statistics for this store.
    async fn stats(&self) -> Result<StoreStats, MemoryError>;

    /// Record that a fact was accessed (increments `access_count`,
    /// updates `last_accessed`). Implementations MAY do this
    /// asynchronously / fire-and-forget; callers SHOULD NOT depend on
    /// the update being immediately visible.
    async fn record_access(&self, id: FactId) -> Result<(), MemoryError>;

    /// Full-text keyword search over fact text (BM25 ranking).
    async fn keyword_search(
        &self,
        query: &str,
        scope: &Scope,
        top_k: usize,
    ) -> Result<Vec<Fact>, MemoryError>;
}
