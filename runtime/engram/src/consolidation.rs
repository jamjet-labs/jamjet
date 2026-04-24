//! Consolidation engine — background memory maintenance.
//!
//! Eight operations keep long-term memory healthy:
//! 1. **Decay** — exponential confidence reduction for stale facts
//! 2. **Promote** — move frequently-accessed conversation facts to knowledge tier
//! 3. **Dedup** — batch vector similarity scan to merge near-duplicates
//! 4. **Summarize** — LLM-condense conversation clusters into knowledge facts
//! 5. **Reflect** — LLM-generate higher-order insights from multiple facts
//! 6. **EntitySummary** — wiki-style per-entity summary from grouped facts
//! 7. **PreferenceRollup** — consolidate preference-category facts into a single profile
//! 8. **TimelineBuilder** — chronologically order facts with event dates

use crate::embedding::EmbeddingProvider;
use crate::fact::{Fact, FactFilter, FactId, FactPatch, MemoryTier};
use crate::llm::LlmClient;
use crate::scope::Scope;
use crate::store::{FactStore, MemoryError};
use crate::vector::{VectorFilter, VectorStore};
use chrono::Utc;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::info;

// ---------------------------------------------------------------------------
// ConsolidationConfig
// ---------------------------------------------------------------------------

/// Configuration for the consolidation engine.
#[derive(Debug, Clone)]
pub struct ConsolidationConfig {
    /// Operations to run. Default: all eight.
    pub enabled_ops: Vec<ConsolidationOp>,
    /// Decay factor per half-life period. Default: 0.95.
    pub decay_factor: f64,
    /// Half-life in days for decay formula. Default: 30.
    pub half_life_days: f64,
    /// Confidence below which facts are archived. Default: 0.3.
    pub archive_threshold: f32,
    /// Access count threshold to promote conversation → knowledge. Default: 3.
    pub promote_access_count: u64,
    /// Similarity threshold for batch dedup. Default: 0.95.
    pub dedup_similarity: f32,
    /// Max conversation facts before summarization triggers. Default: 100.
    pub summarize_threshold: usize,
    /// Minimum facts required to trigger reflection. Default: 10.
    pub reflect_min_facts: usize,
    /// Maximum LLM calls per consolidation cycle. Default: 10.
    pub max_llm_calls: usize,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            enabled_ops: vec![
                ConsolidationOp::Decay,
                ConsolidationOp::Promote,
                ConsolidationOp::Dedup,
                ConsolidationOp::Summarize,
                ConsolidationOp::Reflect,
                ConsolidationOp::EntitySummary,
                ConsolidationOp::PreferenceRollup,
                ConsolidationOp::TimelineBuilder,
            ],
            decay_factor: 0.95,
            half_life_days: 30.0,
            archive_threshold: 0.3,
            promote_access_count: 3,
            dedup_similarity: 0.95,
            summarize_threshold: 100,
            reflect_min_facts: 10,
            max_llm_calls: 10,
        }
    }
}

/// Individual consolidation operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationOp {
    Decay,
    Promote,
    Dedup,
    Summarize,
    Reflect,
    EntitySummary,
    PreferenceRollup,
    TimelineBuilder,
}

// ---------------------------------------------------------------------------
// ConsolidationResult
// ---------------------------------------------------------------------------

/// Summary of what a consolidation cycle did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ConsolidationResult {
    pub facts_decayed: usize,
    pub facts_archived: usize,
    pub facts_promoted: usize,
    pub facts_deduped: usize,
    pub facts_summarized: usize,
    pub insights_generated: usize,
    pub entity_summaries_generated: usize,
    pub preferences_rolled_up: usize,
    pub timeline_events_ordered: usize,
    pub llm_calls_used: usize,
}

// ---------------------------------------------------------------------------
// ConsolidationEngine
// ---------------------------------------------------------------------------

pub struct ConsolidationEngine {
    fact_store: Arc<dyn FactStore>,
    vector_store: Arc<dyn VectorStore>,
    embedding: Arc<dyn EmbeddingProvider>,
    config: ConsolidationConfig,
}

impl ConsolidationEngine {
    pub fn new(
        fact_store: Arc<dyn FactStore>,
        vector_store: Arc<dyn VectorStore>,
        embedding: Arc<dyn EmbeddingProvider>,
        config: ConsolidationConfig,
    ) -> Self {
        Self {
            fact_store,
            vector_store,
            embedding,
            config,
        }
    }

    /// Run all enabled consolidation operations in sequence.
    pub async fn run(
        &self,
        scope: &Scope,
        llm: Option<&dyn LlmClient>,
    ) -> Result<ConsolidationResult, MemoryError> {
        let mut result = ConsolidationResult::default();

        for op in &self.config.enabled_ops {
            match op {
                ConsolidationOp::Decay => {
                    let (decayed, archived) = self.op_decay(scope).await?;
                    result.facts_decayed = decayed;
                    result.facts_archived = archived;
                }
                ConsolidationOp::Promote => {
                    result.facts_promoted = self.op_promote(scope).await?;
                }
                ConsolidationOp::Dedup => {
                    result.facts_deduped = self.op_dedup(scope).await?;
                }
                ConsolidationOp::Summarize => {
                    if let Some(llm) = llm {
                        if result.llm_calls_used < self.config.max_llm_calls {
                            let (summarized, calls) = self
                                .op_summarize(
                                    scope,
                                    llm,
                                    self.config.max_llm_calls - result.llm_calls_used,
                                )
                                .await?;
                            result.facts_summarized = summarized;
                            result.llm_calls_used += calls;
                        }
                    }
                }
                ConsolidationOp::Reflect => {
                    if let Some(llm) = llm {
                        if result.llm_calls_used < self.config.max_llm_calls {
                            let (insights, calls) = self
                                .op_reflect(
                                    scope,
                                    llm,
                                    self.config.max_llm_calls - result.llm_calls_used,
                                )
                                .await?;
                            result.insights_generated = insights;
                            result.llm_calls_used += calls;
                        }
                    }
                }
                ConsolidationOp::EntitySummary => {
                    if let Some(llm) = llm {
                        if result.llm_calls_used < self.config.max_llm_calls {
                            let (summaries, calls) = self
                                .op_entity_summary(
                                    scope,
                                    llm,
                                    self.config.max_llm_calls - result.llm_calls_used,
                                )
                                .await?;
                            result.entity_summaries_generated = summaries;
                            result.llm_calls_used += calls;
                        }
                    }
                }
                ConsolidationOp::PreferenceRollup => {
                    if let Some(llm) = llm {
                        if result.llm_calls_used < self.config.max_llm_calls {
                            let (rolled_up, calls) = self
                                .op_preference_rollup(
                                    scope,
                                    llm,
                                    self.config.max_llm_calls - result.llm_calls_used,
                                )
                                .await?;
                            result.preferences_rolled_up = rolled_up;
                            result.llm_calls_used += calls;
                        }
                    }
                }
                ConsolidationOp::TimelineBuilder => {
                    result.timeline_events_ordered =
                        self.op_timeline_builder(scope).await?;
                }
            }
        }

        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Operation 1: Decay
    // -----------------------------------------------------------------------

    /// Apply exponential confidence decay to knowledge-tier facts.
    /// Returns (facts_decayed, facts_archived).
    async fn op_decay(&self, scope: &Scope) -> Result<(usize, usize), MemoryError> {
        let filter = FactFilter::new()
            .with_scope(scope.clone())
            .with_tier(MemoryTier::Knowledge);
        let facts = self.fact_store.list_facts(&filter).await?;

        let now = Utc::now();
        let mut decayed = 0;
        let mut archived = 0;

        for fact in &facts {
            let last_access = fact.last_accessed.unwrap_or(fact.created_at);
            let days_elapsed = (now - last_access).num_milliseconds() as f64 / 86_400_000.0;

            if days_elapsed <= 0.0 {
                continue;
            }

            let current_confidence = fact.confidence.unwrap_or(1.0) as f64;
            let new_confidence = current_confidence
                * self
                    .config
                    .decay_factor
                    .powf(days_elapsed / self.config.half_life_days);
            let new_confidence = new_confidence as f32;

            if (new_confidence - fact.confidence.unwrap_or(1.0)).abs() < 0.001 {
                continue; // No meaningful change
            }

            if new_confidence < self.config.archive_threshold {
                // Archive: invalidate the fact
                self.fact_store.invalidate_fact(fact.id).await?;
                self.vector_store.delete(fact.id).await?;
                archived += 1;
            } else {
                // Update confidence
                let patch = FactPatch {
                    confidence: Some(new_confidence),
                    ..Default::default()
                };
                self.fact_store.update_fact(fact.id, patch).await?;
            }
            decayed += 1;
        }

        Ok((decayed, archived))
    }

    // -----------------------------------------------------------------------
    // Operation 2: Promote
    // -----------------------------------------------------------------------

    /// Promote frequently-accessed conversation facts to knowledge tier.
    /// Returns number of facts promoted.
    async fn op_promote(&self, scope: &Scope) -> Result<usize, MemoryError> {
        let filter = FactFilter::new()
            .with_scope(scope.clone())
            .with_tier(MemoryTier::Conversation);
        let facts = self.fact_store.list_facts(&filter).await?;

        let mut promoted = 0;

        for fact in &facts {
            if fact.access_count >= self.config.promote_access_count {
                let patch = FactPatch {
                    tier: Some(MemoryTier::Knowledge),
                    ..Default::default()
                };
                self.fact_store.update_fact(fact.id, patch).await?;
                promoted += 1;
            }
        }

        Ok(promoted)
    }

    // -----------------------------------------------------------------------
    // Operation 3: Dedup
    // -----------------------------------------------------------------------

    /// Batch dedup: scan all valid facts, merge near-duplicates.
    /// Returns number of facts deduped (invalidated).
    async fn op_dedup(&self, scope: &Scope) -> Result<usize, MemoryError> {
        let filter = FactFilter::new().with_scope(scope.clone());
        let facts = self.fact_store.list_facts(&filter).await?;

        let mut deduped = 0;
        let mut invalidated_ids: Vec<FactId> = Vec::new();

        for fact in &facts {
            if invalidated_ids.contains(&fact.id) {
                continue;
            }

            // Embed the fact text (SQLite doesn't store embeddings)
            let embedding = match self.embedding.embed(&[fact.text.as_str()]).await {
                Ok(mut embs) => match embs.pop() {
                    Some(e) => e,
                    None => continue,
                },
                Err(_) => continue,
            };

            // Search for similar facts
            let vector_filter = VectorFilter {
                scope: Some(scope.clone()),
                min_score: Some(self.config.dedup_similarity),
            };
            let matches = self
                .vector_store
                .search(&embedding, &vector_filter, 10)
                .await?;

            for m in &matches {
                if m.id == fact.id || invalidated_ids.contains(&m.id) {
                    continue;
                }
                if m.score > 0.99 {
                    // Auto-merge: keep the current fact, invalidate the match
                    self.fact_store.invalidate_fact(m.id).await?;
                    self.vector_store.delete(m.id).await?;

                    // Link supersession
                    let patch = FactPatch {
                        superseded_by: Some(fact.id),
                        ..Default::default()
                    };
                    let _ = self.fact_store.update_fact(m.id, patch).await;

                    invalidated_ids.push(m.id);
                    deduped += 1;
                }
            }
        }

        Ok(deduped)
    }

    // -----------------------------------------------------------------------
    // Operation 4: Summarize
    // -----------------------------------------------------------------------

    /// Summarize conversation facts into knowledge facts using LLM.
    /// Returns (facts_summarized, llm_calls_used).
    async fn op_summarize(
        &self,
        scope: &Scope,
        llm: &dyn LlmClient,
        max_calls: usize,
    ) -> Result<(usize, usize), MemoryError> {
        let filter = FactFilter::new()
            .with_scope(scope.clone())
            .with_tier(MemoryTier::Conversation);
        let facts = self.fact_store.list_facts(&filter).await?;

        if facts.len() < self.config.summarize_threshold {
            return Ok((0, 0));
        }

        // Build fact list for LLM
        let fact_texts: Vec<String> = facts.iter().map(|f| f.text.clone()).collect();
        let prompt = fact_texts.join("\n- ");

        let system = "You are a memory summarization engine. Given a list of conversation facts, produce 1-3 concise knowledge-level summaries that capture the key information. Respond with JSON: {\"summaries\": [\"summary1\", \"summary2\"]}";
        let user_msg = format!("Summarize these conversation facts into knowledge:\n- {prompt}");

        let response = llm.structured_output(system, &user_msg).await?;

        let summaries: Vec<String> = response["summaries"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();

        let mut summarized = 0;

        for summary_text in &summaries {
            // Create new knowledge fact
            let mut embeddings = self.embedding.embed(&[summary_text.as_str()]).await?;
            let embedding = embeddings.pop().unwrap_or_default();

            let mut fact = Fact::new(summary_text, scope.clone());
            fact.tier = MemoryTier::Knowledge;
            fact.source = Some("consolidation:summarize".to_string());
            fact.confidence = Some(0.85);
            fact.embedding = embedding.clone();

            let id = self.fact_store.insert_fact(fact).await?;
            self.vector_store
                .upsert(id, embedding, serde_json::json!({}))
                .await?;
            summarized += 1;
        }

        // Mark original conversation facts as summarized via metadata
        for fact in &facts {
            let mut metadata = serde_json::Map::new();
            metadata.insert("summarized".to_string(), serde_json::Value::Bool(true));
            let patch = FactPatch {
                metadata,
                ..Default::default()
            };
            let _ = self.fact_store.update_fact(fact.id, patch).await;
        }

        Ok((summarized, 1.min(max_calls)))
    }

    // -----------------------------------------------------------------------
    // Operation 5: Reflect
    // -----------------------------------------------------------------------

    /// Generate higher-order insights from top facts using LLM.
    /// Returns (insights_generated, llm_calls_used).
    async fn op_reflect(
        &self,
        scope: &Scope,
        llm: &dyn LlmClient,
        max_calls: usize,
    ) -> Result<(usize, usize), MemoryError> {
        if max_calls == 0 {
            return Ok((0, 0));
        }

        let filter = FactFilter::new()
            .with_scope(scope.clone())
            .with_tier(MemoryTier::Knowledge);
        let facts = self.fact_store.list_facts(&filter).await?;

        if facts.len() < self.config.reflect_min_facts {
            return Ok((0, 0));
        }

        let fact_texts: Vec<String> = facts.iter().take(30).map(|f| f.text.clone()).collect();
        let prompt = fact_texts.join("\n- ");

        let system = "You are a memory reflection engine. Given a list of facts about a user/topic, generate 1-2 higher-order insights or patterns. Each insight should be a concise statement that captures something not explicitly stated by any single fact. Respond with JSON: {\"insights\": [\"insight1\", \"insight2\"]}";
        let user_msg = format!("Generate insights from these facts:\n- {prompt}");

        let response = llm.structured_output(system, &user_msg).await?;

        let insights: Vec<String> = response["insights"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();

        let mut generated = 0;

        for insight_text in &insights {
            let mut embeddings = self.embedding.embed(&[insight_text.as_str()]).await?;
            let embedding = embeddings.pop().unwrap_or_default();

            let mut fact = Fact::new(insight_text, scope.clone());
            fact.tier = MemoryTier::Knowledge;
            fact.source = Some("consolidation:reflect".to_string());
            fact.confidence = Some(0.8);
            fact.embedding = embedding.clone();

            let id = self.fact_store.insert_fact(fact).await?;
            self.vector_store
                .upsert(id, embedding, serde_json::json!({}))
                .await?;
            generated += 1;
        }

        Ok((generated, 1))
    }

    // -----------------------------------------------------------------------
    // Operation 6: EntitySummary
    // -----------------------------------------------------------------------

    /// Generate wiki-style per-entity summaries from facts grouped by entity.
    /// Only entities with 3+ facts are summarized.
    /// Returns (entity_summaries_generated, llm_calls_used).
    async fn op_entity_summary(
        &self,
        scope: &Scope,
        llm: &dyn LlmClient,
        max_calls: usize,
    ) -> Result<(usize, usize), MemoryError> {
        if max_calls == 0 {
            return Ok((0, 0));
        }

        let filter = FactFilter::new().with_scope(scope.clone());
        let facts = self.fact_store.list_facts(&filter).await?;

        // Group facts by entity ref — a fact can reference multiple entities.
        let mut entity_facts: HashMap<crate::fact::EntityId, Vec<&Fact>> = HashMap::new();
        for fact in &facts {
            for entity_id in &fact.entity_refs {
                entity_facts.entry(*entity_id).or_default().push(fact);
            }
        }

        let mut generated = 0;
        let mut calls_used = 0;

        for (entity_id, group) in &entity_facts {
            if group.len() < 3 || calls_used >= max_calls {
                continue;
            }

            let fact_texts: Vec<&str> = group.iter().map(|f| f.text.as_str()).collect();
            let prompt = fact_texts.join("\n- ");

            info!(
                entity_id = %entity_id,
                fact_count = group.len(),
                "entity_summary: generating summary for entity"
            );

            let system = "You are a memory consolidation engine. Given a list of facts about a single entity, produce one concise wiki-style summary paragraph. Respond with JSON: {\"summary\": \"...\"}";
            let user_msg = format!(
                "Summarize these facts about entity {entity_id} into a single paragraph:\n- {prompt}"
            );

            let response = llm.structured_output(system, &user_msg).await?;
            calls_used += 1;

            if let Some(summary_text) = response["summary"].as_str() {
                let mut embeddings = self.embedding.embed(&[summary_text]).await?;
                let embedding = embeddings.pop().unwrap_or_default();

                let mut fact = Fact::new(summary_text, scope.clone());
                fact.tier = MemoryTier::Knowledge;
                fact.source = Some("consolidation:entity_summary".to_string());
                fact.confidence = Some(0.85);
                fact.entity_refs = vec![*entity_id];
                fact.embedding = embedding.clone();

                let id = self.fact_store.insert_fact(fact).await?;
                self.vector_store
                    .upsert(id, embedding, serde_json::json!({}))
                    .await?;
                generated += 1;
            }
        }

        Ok((generated, calls_used))
    }

    // -----------------------------------------------------------------------
    // Operation 7: PreferenceRollup
    // -----------------------------------------------------------------------

    /// Consolidate preference-category facts into a rolled-up profile fact.
    /// Returns (preferences_rolled_up, llm_calls_used).
    async fn op_preference_rollup(
        &self,
        scope: &Scope,
        llm: &dyn LlmClient,
        max_calls: usize,
    ) -> Result<(usize, usize), MemoryError> {
        if max_calls == 0 {
            return Ok((0, 0));
        }

        let mut filter = FactFilter::new().with_scope(scope.clone());
        filter.category = Some("preference".to_string());
        let facts = self.fact_store.list_facts(&filter).await?;

        if facts.is_empty() {
            return Ok((0, 0));
        }

        info!(
            preference_count = facts.len(),
            "preference_rollup: consolidating preference facts"
        );

        let fact_texts: Vec<&str> = facts.iter().map(|f| f.text.as_str()).collect();
        let prompt = fact_texts.join("\n- ");

        let system = "You are a memory consolidation engine. Given a list of user preference facts, produce a single consolidated preference profile as a concise paragraph. Respond with JSON: {\"profile\": \"...\"}";
        let user_msg = format!("Roll up these user preferences into a profile:\n- {prompt}");

        let response = llm.structured_output(system, &user_msg).await?;

        if let Some(profile_text) = response["profile"].as_str() {
            let mut embeddings = self.embedding.embed(&[profile_text]).await?;
            let embedding = embeddings.pop().unwrap_or_default();

            let mut fact = Fact::new(profile_text, scope.clone());
            fact.tier = MemoryTier::Knowledge;
            fact.category = Some("preference".to_string());
            fact.source = Some("consolidation:preference_rollup".to_string());
            fact.confidence = Some(0.9);
            fact.embedding = embedding.clone();

            let id = self.fact_store.insert_fact(fact).await?;
            self.vector_store
                .upsert(id, embedding, serde_json::json!({}))
                .await?;

            // Mark originals as rolled up
            for fact in &facts {
                let mut metadata = serde_json::Map::new();
                metadata.insert("rolled_up".to_string(), serde_json::Value::Bool(true));
                let patch = FactPatch {
                    metadata,
                    ..Default::default()
                };
                let _ = self.fact_store.update_fact(fact.id, patch).await;
            }

            return Ok((facts.len(), 1));
        }

        Ok((0, 1))
    }

    // -----------------------------------------------------------------------
    // Operation 8: TimelineBuilder
    // -----------------------------------------------------------------------

    /// Sort facts with `event_date` metadata chronologically and tag them
    /// with their ordinal position. No LLM needed.
    /// Returns the number of timeline events ordered.
    async fn op_timeline_builder(
        &self,
        scope: &Scope,
    ) -> Result<usize, MemoryError> {
        let filter = FactFilter::new().with_scope(scope.clone());
        let facts = self.fact_store.list_facts(&filter).await?;

        // Collect facts that carry an event_date in metadata.
        let mut dated: Vec<(&Fact, String)> = facts
            .iter()
            .filter_map(|f| {
                f.metadata
                    .get("event_date")
                    .and_then(|v| v.as_str())
                    .map(|d| (f, d.to_string()))
            })
            .collect();

        if dated.is_empty() {
            return Ok(0);
        }

        // Sort by the event_date string (ISO-8601 sorts lexicographically).
        dated.sort_by(|a, b| a.1.cmp(&b.1));

        info!(
            event_count = dated.len(),
            "timeline_builder: ordering facts chronologically"
        );

        // Tag each fact with its position in the timeline.
        for (position, (fact, _date)) in dated.iter().enumerate() {
            let mut metadata = serde_json::Map::new();
            metadata.insert(
                "timeline_position".to_string(),
                serde_json::Value::Number(serde_json::Number::from(position)),
            );
            let patch = FactPatch {
                metadata,
                ..Default::default()
            };
            let _ = self.fact_store.update_fact(fact.id, patch).await;
        }

        Ok(dated.len())
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = ConsolidationConfig::default();
        assert_eq!(cfg.enabled_ops.len(), 8);
        assert!((cfg.decay_factor - 0.95).abs() < f64::EPSILON);
        assert_eq!(cfg.promote_access_count, 3);
        assert_eq!(cfg.max_llm_calls, 10);
    }

    #[test]
    fn result_defaults_to_zero() {
        let r = ConsolidationResult::default();
        assert_eq!(r.facts_decayed, 0);
        assert_eq!(r.facts_promoted, 0);
        assert_eq!(r.llm_calls_used, 0);
    }

    #[test]
    fn decay_math() {
        // 0.95 confidence, 30 days elapsed, half_life=30, decay_factor=0.95
        let confidence = 0.95_f64;
        let decayed = confidence * 0.95_f64.powf(30.0 / 30.0);
        assert!((decayed - 0.9025).abs() < 0.001);

        // After 60 days
        let decayed_60 = confidence * 0.95_f64.powf(60.0 / 30.0);
        assert!((decayed_60 - 0.8574).abs() < 0.001);
    }
}
