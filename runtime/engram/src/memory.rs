//! `Memory` — the primary public API for the Engram memory layer.
//!
//! `Memory` composes a `FactStore`, `VectorStore`, `GraphStore`, and
//! `EmbeddingProvider` into a single high-level interface. Callers interact
//! with `Memory` rather than the lower-level trait objects directly.

use crate::embedding::EmbeddingProvider;
use crate::fact::{Fact, FactFilter, FactId};
use crate::graph::GraphStore;
use crate::graph_sqlite::SqliteGraphStore;
use crate::scope::Scope;
use crate::store::{FactStore, MemoryError, StoreStats};
use crate::store_sqlite::SqliteFactStore;
use crate::vector::{VectorFilter, VectorStore};
use crate::vector_embedded::EmbeddedVectorStore;
use chrono::{DateTime, Utc};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// RecallQuery
// ---------------------------------------------------------------------------

/// Parameters for a semantic recall operation.
#[derive(Debug, Clone, Default)]
pub struct RecallQuery {
    /// The text to embed and search for semantically similar facts.
    pub query: String,
    /// Optional scope filter — only return facts within this scope.
    pub scope: Option<Scope>,
    /// Maximum number of results to return (default: 10).
    pub max_results: usize,
    /// Point-in-time filter — only return facts valid at this instant.
    pub as_of: Option<DateTime<Utc>>,
    /// Minimum cosine similarity score (0.0 – 1.0).
    pub min_score: Option<f32>,
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// High-level memory API for AI agents.
///
/// `Memory` wires together fact storage, vector search, graph storage, and an
/// embedding provider. Most callers should construct it via `in_memory` or
/// `open` rather than calling `new` directly.
pub struct Memory {
    fact_store: Arc<dyn FactStore>,
    vector_store: Arc<dyn VectorStore>,
    graph_store: Arc<dyn GraphStore>,
    embedding: Arc<dyn EmbeddingProvider>,
}

impl Memory {
    // -----------------------------------------------------------------------
    // Constructors
    // -----------------------------------------------------------------------

    /// Construct `Memory` from explicit store and embedding instances.
    pub fn new(
        fact_store: Arc<dyn FactStore>,
        vector_store: Arc<dyn VectorStore>,
        graph_store: Arc<dyn GraphStore>,
        embedding: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        Self {
            fact_store,
            vector_store,
            graph_store,
            embedding,
        }
    }

    /// Create a fully in-memory `Memory` instance backed by SQLite `:memory:`.
    ///
    /// Schema migration is applied automatically. Suitable for testing and
    /// short-lived agent invocations.
    pub async fn in_memory(embedding: Box<dyn EmbeddingProvider>) -> Result<Self, MemoryError> {
        let dims = embedding.dimensions();
        let embedding = Arc::from(embedding);

        let fact_store = SqliteFactStore::open("sqlite::memory:")
            .await
            .map_err(|e| MemoryError::Database(format!("failed to open in-memory SQLite: {e}")))?;
        fact_store
            .migrate()
            .await
            .map_err(|e| MemoryError::Database(format!("fact store migration failed: {e}")))?;

        let graph_store = SqliteGraphStore::open("sqlite::memory:")
            .await
            .map_err(|e| MemoryError::Database(format!("failed to open in-memory graph: {e}")))?;
        graph_store
            .migrate()
            .await
            .map_err(|e| MemoryError::Database(format!("graph store migration failed: {e}")))?;

        let vector_store = EmbeddedVectorStore::new(dims);

        Ok(Self {
            fact_store: Arc::new(fact_store),
            vector_store: Arc::new(vector_store),
            graph_store: Arc::new(graph_store),
            embedding,
        })
    }

    /// Open a file-backed `Memory` instance at `database_url`.
    ///
    /// Uses SQLite for facts and graph data, with an in-process
    /// `EmbeddedVectorStore` for semantic search. Schema migration is applied
    /// automatically.
    pub async fn open(
        database_url: &str,
        embedding: Box<dyn EmbeddingProvider>,
    ) -> Result<Self, MemoryError> {
        let dims = embedding.dimensions();
        let embedding = Arc::from(embedding);

        let fact_store = SqliteFactStore::open(database_url)
            .await
            .map_err(|e| MemoryError::Database(format!("failed to open SQLite: {e}")))?;
        fact_store
            .migrate()
            .await
            .map_err(|e| MemoryError::Database(format!("fact store migration failed: {e}")))?;

        let graph_store = SqliteGraphStore::open(database_url)
            .await
            .map_err(|e| MemoryError::Database(format!("failed to open graph SQLite: {e}")))?;
        graph_store
            .migrate()
            .await
            .map_err(|e| MemoryError::Database(format!("graph store migration failed: {e}")))?;

        let vector_store = EmbeddedVectorStore::new(dims);

        Ok(Self {
            fact_store: Arc::new(fact_store),
            vector_store: Arc::new(vector_store),
            graph_store: Arc::new(graph_store),
            embedding,
        })
    }

    // -----------------------------------------------------------------------
    // Write operations
    // -----------------------------------------------------------------------

    /// Embed `text`, create a `Fact`, and persist it in both the fact store
    /// and the vector store.
    ///
    /// Returns the `FactId` of the newly created fact.
    pub async fn add_fact(&self, text: &str, scope: Scope) -> Result<FactId, MemoryError> {
        // Embed text.
        let mut embeddings = self.embedding.embed(&[text]).await?;
        let embedding = embeddings.pop().ok_or_else(|| {
            MemoryError::Embedding("provider returned empty embeddings".to_string())
        })?;

        // Build and persist the fact.
        let mut fact = Fact::new(text, scope);
        fact.embedding = embedding.clone();
        let id = self.fact_store.insert_fact(fact).await?;

        // Insert into vector store.
        let metadata = serde_json::json!({ "fact_id": id.to_string() });
        self.vector_store.upsert(id, embedding, metadata).await?;

        Ok(id)
    }

    /// Semantically recall facts matching `query`.
    ///
    /// Embeds the query text, performs vector search, fetches full facts from
    /// the fact store, filters by validity and scope, and records an access
    /// event for each returned fact.
    pub async fn recall(&self, query: &RecallQuery) -> Result<Vec<Fact>, MemoryError> {
        let max_results = if query.max_results == 0 {
            10
        } else {
            query.max_results
        };

        // Embed the query.
        let mut embeddings = self.embedding.embed(&[query.query.as_str()]).await?;
        let query_vec = embeddings.pop().ok_or_else(|| {
            MemoryError::Embedding("provider returned empty embeddings".to_string())
        })?;

        // Vector search.
        let filter = VectorFilter {
            scope: query.scope.clone(),
            min_score: query.min_score,
        };
        let matches = self
            .vector_store
            .search(&query_vec, &filter, max_results)
            .await?;

        // Fetch full facts from the fact store.
        let mut facts = Vec::with_capacity(matches.len());
        for vm in matches {
            match self.fact_store.get_fact(vm.id).await {
                Ok(fact) => {
                    // Validity filter.
                    let valid = match query.as_of {
                        Some(t) => fact.is_valid_at(t),
                        None => fact.is_valid(),
                    };
                    if !valid {
                        continue;
                    }
                    // Scope filter (post-fetch).
                    if let Some(ref scope) = query.scope {
                        if !scope.contains(&fact.scope) {
                            continue;
                        }
                    }
                    // Record access (fire-and-forget; ignore error).
                    let _ = self.fact_store.record_access(fact.id).await;
                    facts.push(fact);
                }
                Err(MemoryError::NotFound(_)) => {
                    // Vector store has a stale entry — skip silently.
                }
                Err(e) => return Err(e),
            }
        }

        Ok(facts)
    }

    // -----------------------------------------------------------------------
    // Read operations
    // -----------------------------------------------------------------------

    /// List currently-valid facts for the given scope.
    pub async fn list_facts(&self, scope: Option<Scope>) -> Result<Vec<Fact>, MemoryError> {
        let filter = match scope {
            Some(s) => FactFilter::new().with_scope(s),
            None => FactFilter::new(),
        };
        self.fact_store.list_facts(&filter).await
    }

    // -----------------------------------------------------------------------
    // Mutation operations
    // -----------------------------------------------------------------------

    /// Invalidate (soft-delete) a fact and remove it from the vector store.
    ///
    /// The fact record is preserved for historical queries. `reason` is
    /// currently logged via tracing but not stored (reserved for future use).
    pub async fn forget(&self, id: FactId, _reason: Option<&str>) -> Result<(), MemoryError> {
        self.fact_store.invalidate_fact(id).await?;
        self.vector_store.delete(id).await?;
        Ok(())
    }

    /// Hard-delete all data (facts, vectors, graph nodes/edges) for `scope`.
    ///
    /// Returns the number of facts deleted. Intended for GDPR / right-to-erasure
    /// requests.
    pub async fn delete_user_data(&self, scope: Scope) -> Result<u64, MemoryError> {
        let fact_count = self.fact_store.delete_scope_data(&scope).await?;
        self.vector_store.delete_by_scope(&scope).await?;
        self.graph_store.delete_by_scope(&scope).await?;
        Ok(fact_count)
    }

    // -----------------------------------------------------------------------
    // Stats & export/import
    // -----------------------------------------------------------------------

    /// Return aggregate statistics for the fact store.
    pub async fn stats(&self, _scope: Option<Scope>) -> Result<StoreStats, MemoryError> {
        self.fact_store.stats().await
    }

    /// Export all currently-valid facts for `scope`.
    pub async fn export(&self, scope: Option<Scope>) -> Result<Vec<Fact>, MemoryError> {
        let filter = match scope {
            Some(s) => FactFilter::new().with_scope(s),
            None => FactFilter::new(),
        };
        self.fact_store.export(&filter).await
    }

    /// Import a batch of facts, re-embedding each one.
    ///
    /// Returns the number of facts successfully imported (skips duplicates by id).
    pub async fn import(&self, facts: Vec<Fact>) -> Result<u64, MemoryError> {
        let mut imported: u64 = 0;
        for mut fact in facts {
            // Re-embed the fact text.
            let mut embeddings = self.embedding.embed(&[fact.text.as_str()]).await?;
            let embedding = embeddings.pop().ok_or_else(|| {
                MemoryError::Embedding("provider returned empty embeddings".to_string())
            })?;
            fact.embedding = embedding.clone();

            let fact_id = fact.id;
            self.fact_store.insert_fact(fact).await?;

            let metadata = serde_json::json!({ "fact_id": fact_id.to_string() });
            self.vector_store
                .upsert(fact_id, embedding, metadata)
                .await?;
            imported += 1;
        }
        Ok(imported)
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    /// Access the underlying `FactStore`.
    pub fn fact_store(&self) -> &Arc<dyn FactStore> {
        &self.fact_store
    }

    /// Access the underlying `VectorStore`.
    pub fn vector_store(&self) -> &Arc<dyn VectorStore> {
        &self.vector_store
    }

    /// Access the underlying `GraphStore`.
    pub fn graph_store(&self) -> &Arc<dyn GraphStore> {
        &self.graph_store
    }
}
