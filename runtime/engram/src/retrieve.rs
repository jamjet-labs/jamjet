//! Hybrid retrieval — merges vector, keyword, and graph search results.

use crate::embedding::EmbeddingProvider;
use crate::fact::{Fact, FactId};
use crate::graph::GraphStore;
use crate::scope::Scope;
use crate::store::{FactStore, MemoryError};
use crate::vector::{VectorFilter, VectorStore};
use std::collections::HashMap;
use std::sync::Arc;

/// A fact with a combined retrieval score.
#[derive(Debug, Clone)]
pub struct ScoredFact {
    pub fact: Fact,
    pub score: f32,
    pub vector_score: f32,
    pub keyword_score: f32,
    pub graph_score: f32,
}

/// Configuration for hybrid retrieval scoring.
#[derive(Debug, Clone)]
pub struct RetrievalConfig {
    /// Weight for vector/semantic similarity (0.0-1.0).
    pub vector_weight: f32,
    /// Weight for keyword/BM25 score (0.0-1.0).
    pub keyword_weight: f32,
    /// Weight for graph proximity score (0.0-1.0).
    pub graph_weight: f32,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            vector_weight: 0.5,
            keyword_weight: 0.3,
            graph_weight: 0.2,
        }
    }
}

pub struct HybridRetriever {
    fact_store: Arc<dyn FactStore>,
    vector_store: Arc<dyn VectorStore>,
    graph_store: Arc<dyn GraphStore>,
    embedding: Arc<dyn EmbeddingProvider>,
    config: RetrievalConfig,
}

impl HybridRetriever {
    pub fn new(
        fact_store: Arc<dyn FactStore>,
        vector_store: Arc<dyn VectorStore>,
        graph_store: Arc<dyn GraphStore>,
        embedding: Arc<dyn EmbeddingProvider>,
        config: RetrievalConfig,
    ) -> Self {
        Self {
            fact_store,
            vector_store,
            graph_store,
            embedding,
            config,
        }
    }

    /// Hybrid search: vector + keyword + graph, merged with configurable weights.
    pub async fn search(
        &self,
        query: &str,
        scope: &Scope,
        top_k: usize,
    ) -> Result<Vec<ScoredFact>, MemoryError> {
        // Fetch more candidates from each source, then merge
        let candidate_k = top_k * 3;

        // 1. Vector search
        let embeddings = self.embedding.embed(&[query]).await?;
        let query_vec = embeddings.into_iter().next().ok_or_else(|| {
            MemoryError::Embedding("empty embedding".to_string())
        })?;
        let vector_filter = VectorFilter {
            scope: Some(scope.clone()),
            min_score: None,
        };
        let vector_matches = self
            .vector_store
            .search(&query_vec, &vector_filter, candidate_k)
            .await?;

        // 2. Keyword search
        let keyword_matches = self
            .fact_store
            .keyword_search(query, scope, candidate_k)
            .await?;

        // 3. Graph walk — find entities matching query terms, walk 1 hop
        let graph_entity_ids = self.graph_store.search_entities(query, 5).await?;
        let mut graph_fact_ids: HashMap<FactId, f32> = HashMap::new();
        for entity in &graph_entity_ids {
            let subgraph = self.graph_store.neighbors(entity.id, 1, None).await?;
            // Facts that reference entities in the subgraph get a graph score
            for _rel in &subgraph.relationships {
                let entity_facts = self
                    .fact_store
                    .keyword_search(&entity.name, scope, 5)
                    .await?;
                for f in &entity_facts {
                    let entry = graph_fact_ids.entry(f.id).or_insert(0.0);
                    *entry = (*entry + 0.5).min(1.0);
                }
            }
        }

        // Merge scores
        let mut scored: HashMap<FactId, (f32, f32, f32)> = HashMap::new(); // (vec, kw, graph)

        // Vector scores (already cosine similarity in [0,1])
        for vm in &vector_matches {
            scored.entry(vm.id).or_insert((0.0, 0.0, 0.0)).0 = vm.score;
        }

        // Keyword scores (normalize by rank position)
        for (i, fact) in keyword_matches.iter().enumerate() {
            let kw_score = 1.0 - (i as f32 / candidate_k.max(1) as f32);
            scored.entry(fact.id).or_insert((0.0, 0.0, 0.0)).1 = kw_score;
        }

        // Graph scores
        for (id, score) in &graph_fact_ids {
            scored.entry(*id).or_insert((0.0, 0.0, 0.0)).2 = *score;
        }

        // Fetch full facts and compute final scores
        let mut results: Vec<ScoredFact> = Vec::new();
        for (id, (vs, ks, gs)) in &scored {
            if let Ok(fact) = self.fact_store.get_fact(*id).await {
                if !fact.is_valid() {
                    continue;
                }
                let final_score = vs * self.config.vector_weight
                    + ks * self.config.keyword_weight
                    + gs * self.config.graph_weight;
                results.push(ScoredFact {
                    fact,
                    score: final_score,
                    vector_score: *vs,
                    keyword_score: *ks,
                    graph_score: *gs,
                });
            }
        }

        // Sort descending by score
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(top_k);

        // Record access
        for sf in &results {
            let _ = self.fact_store.record_access(sf.fact.id).await;
        }

        Ok(results)
    }
}
