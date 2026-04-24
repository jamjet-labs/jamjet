//! Hybrid retrieval — merges vector, keyword, and graph search results.

use crate::embedding::EmbeddingProvider;
use crate::fact::{Fact, FactId};
use crate::graph::GraphStore;
use crate::scope::Scope;
use crate::store::{FactStore, MemoryError};
use crate::temporal_parser::detect_temporal_intent;
use crate::vector::{VectorFilter, VectorStore};
use chrono::{DateTime, Utc};
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
    pub temporal_score: f32,
    pub importance_score: f32,
    pub confidence_score: f32,
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
    /// Weight for temporal proximity score (0.0-1.0).
    pub temporal_weight: f32,
    /// Weight for node importance score (0.0-1.0).
    pub importance_weight: f32,
    /// Weight for calibrated confidence score (0.0-1.0).
    pub confidence_weight: f32,
    /// Half-life in days for confidence decay.
    pub confidence_half_life_days: f64,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            vector_weight: 0.25,
            keyword_weight: 0.10,
            graph_weight: 0.15,
            temporal_weight: 0.25,
            importance_weight: 0.10,
            confidence_weight: 0.15,
            confidence_half_life_days: 90.0,
        }
    }
}

impl RetrievalConfig {
    /// Redistribute the temporal weight proportionally among the other signals
    /// when the query has no temporal intent.
    pub fn non_temporal_weights(&self) -> (f32, f32, f32, f32, f32) {
        let total = self.vector_weight
            + self.keyword_weight
            + self.graph_weight
            + self.importance_weight
            + self.confidence_weight;
        let scale = (total + self.temporal_weight) / total;
        (
            self.vector_weight * scale,
            self.keyword_weight * scale,
            self.graph_weight * scale,
            self.importance_weight * scale,
            self.confidence_weight * scale,
        )
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
        let query_vec = embeddings
            .into_iter()
            .next()
            .ok_or_else(|| MemoryError::Embedding("empty embedding".to_string()))?;
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

        // 3. Graph spreading activation
        let graph_entity_ids = self.graph_store.search_entities(query, 5).await?;
        let seeds: Vec<(crate::fact::EntityId, f32)> = graph_entity_ids
            .iter()
            .map(|e| (e.id, 1.0_f32))
            .collect();

        let activations = crate::spreading::spread(
            &self.graph_store,
            &seeds,
            &crate::spreading::SpreadingActivationConfig::default(),
        )
        .await?;

        let mut graph_fact_ids: HashMap<FactId, f32> = HashMap::new();
        for (entity_id, activation) in &activations {
            if let Ok(Some(entity)) = self.graph_store.get_entity(*entity_id).await {
                let entity_facts = self
                    .fact_store
                    .keyword_search(&entity.name, scope, 5)
                    .await?;
                for f in &entity_facts {
                    let entry = graph_fact_ids.entry(f.id).or_insert(0.0);
                    *entry = entry.max(*activation);
                }
            }
        }

        // Detect temporal intent
        let temporal_intent = detect_temporal_intent(query);
        let anchor = Utc::now();

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

                let ts = if temporal_intent.is_some() {
                    temporal_proximity(&fact, anchor)
                } else {
                    0.0
                };
                let is = node_importance(&fact);
                let cs =
                    calibrated_confidence(&fact, self.config.confidence_half_life_days);

                let final_score = if temporal_intent.is_some() {
                    vs * self.config.vector_weight
                        + ks * self.config.keyword_weight
                        + gs * self.config.graph_weight
                        + ts * self.config.temporal_weight
                        + is * self.config.importance_weight
                        + cs * self.config.confidence_weight
                } else {
                    let (vw, kw, gw, iw, cw) = self.config.non_temporal_weights();
                    vs * vw + ks * kw + gs * gw + is * iw + cs * cw
                };

                results.push(ScoredFact {
                    fact,
                    score: final_score,
                    vector_score: *vs,
                    keyword_score: *ks,
                    graph_score: *gs,
                    temporal_score: ts,
                    importance_score: is,
                    confidence_score: cs,
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

// ---------------------------------------------------------------------------
// Scoring helpers
// ---------------------------------------------------------------------------

/// Confidence decayed over time — models the intuition that older facts are
/// less reliable unless recently reinforced.
fn calibrated_confidence(fact: &Fact, half_life_days: f64) -> f32 {
    let confidence = fact.confidence.unwrap_or(1.0);
    let age_days = (Utc::now() - fact.valid_from).num_days().max(0) as f64;
    let decay = 0.5_f64.powf(age_days / half_life_days);
    (confidence as f64 * decay) as f32
}

/// Importance derived from entity connectivity and access frequency.
fn node_importance(fact: &Fact) -> f32 {
    let entity_degree = fact.entity_refs.len() as f32;
    let access_log = (1.0 + fact.access_count as f32).ln();
    let raw = entity_degree + access_log;
    (raw / 10.0).min(1.0)
}

/// Gaussian proximity to a temporal anchor — facts closer in time score higher.
fn temporal_proximity(fact: &Fact, anchor: DateTime<Utc>) -> f32 {
    let distance_days = (fact.valid_from - anchor).num_days().unsigned_abs() as f64;
    let sigma = 30.0;
    ((-distance_days * distance_days) / (2.0 * sigma * sigma)).exp() as f32
}
