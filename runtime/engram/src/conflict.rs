//! Conflict detection — Stage 2 of the extraction pipeline.
//!
//! Checks if a newly extracted fact duplicates or contradicts an existing fact.

use crate::embedding::EmbeddingProvider;
use crate::extract::ConflictVerdict;
use crate::fact::FactId;
use crate::llm::LlmClient;
use crate::scope::Scope;
use crate::store::{FactStore, MemoryError};
use crate::vector::{VectorFilter, VectorStore};
use std::sync::Arc;

/// Result of conflict detection for a single fact.
#[derive(Debug, Clone)]
pub struct ConflictResult {
    pub verdict: ConflictVerdict,
    pub existing_fact_id: Option<FactId>,
    pub similarity: Option<f32>,
    pub reason: Option<String>,
}

const CONFLICT_SYSTEM_PROMPT: &str = r#"You are comparing two facts about a user/topic. Determine if the new fact contradicts, duplicates, or refines the existing fact.

Respond with JSON:
{"verdict": "contradicts" | "duplicate" | "refines", "reason": "brief explanation"}

- "contradicts": The new fact directly contradicts the existing fact (e.g., moved from city A to city B)
- "duplicate": The new fact says the same thing as the existing fact
- "refines": The new fact adds new detail to the existing fact without contradicting it"#;

pub struct ConflictDetector {
    fact_store: Arc<dyn FactStore>,
    vector_store: Arc<dyn VectorStore>,
    embedding: Arc<dyn EmbeddingProvider>,
    llm: Arc<dyn LlmClient>,
    similarity_threshold: f32,
}

impl ConflictDetector {
    pub fn new(
        fact_store: Arc<dyn FactStore>,
        vector_store: Arc<dyn VectorStore>,
        embedding: Arc<dyn EmbeddingProvider>,
        llm: Box<dyn LlmClient>,
        similarity_threshold: f32,
    ) -> Self {
        Self {
            fact_store,
            vector_store,
            embedding,
            llm: Arc::from(llm),
            similarity_threshold,
        }
    }

    /// Check if a new fact text conflicts with existing facts in the given scope.
    pub async fn check(
        &self,
        new_text: &str,
        scope: &Scope,
    ) -> Result<ConflictResult, MemoryError> {
        // Embed the new fact
        let embeddings = self.embedding.embed(&[new_text]).await?;
        let new_embedding = embeddings.into_iter().next().ok_or_else(|| {
            MemoryError::Embedding("empty embedding result".to_string())
        })?;

        // Search for similar existing facts
        let filter = VectorFilter {
            scope: Some(scope.clone()),
            min_score: Some(self.similarity_threshold),
        };
        let matches = self.vector_store.search(&new_embedding, &filter, 5).await?;

        if matches.is_empty() {
            return Ok(ConflictResult {
                verdict: ConflictVerdict::NoConflict,
                existing_fact_id: None,
                similarity: None,
                reason: None,
            });
        }

        let top_match = &matches[0];

        // Very high similarity (>0.99) = auto-dedup without LLM call
        if top_match.score > 0.99 {
            return Ok(ConflictResult {
                verdict: ConflictVerdict::Duplicate,
                existing_fact_id: Some(top_match.id),
                similarity: Some(top_match.score),
                reason: Some("near-identical embedding".to_string()),
            });
        }

        // Fetch the existing fact text for LLM comparison
        let existing_fact = self.fact_store.get_fact(top_match.id).await;
        let existing_text = match existing_fact {
            Ok(f) if f.is_valid() => f.text,
            _ => {
                return Ok(ConflictResult {
                    verdict: ConflictVerdict::NoConflict,
                    existing_fact_id: None,
                    similarity: Some(top_match.score),
                    reason: Some("existing fact is invalid or not found".to_string()),
                });
            }
        };

        // LLM conflict resolution
        let user_prompt = format!(
            "Existing fact: \"{}\"\nNew fact: \"{}\"",
            existing_text, new_text
        );
        let response = self
            .llm
            .structured_output(CONFLICT_SYSTEM_PROMPT, &user_prompt)
            .await?;

        let verdict_str = response["verdict"].as_str().unwrap_or("refines");
        let reason = response["reason"].as_str().map(String::from);

        let verdict = match verdict_str {
            "contradicts" => ConflictVerdict::Contradicts,
            "duplicate" => ConflictVerdict::Duplicate,
            "refines" => ConflictVerdict::Refines,
            _ => ConflictVerdict::Refines,
        };

        Ok(ConflictResult {
            verdict,
            existing_fact_id: Some(top_match.id),
            similarity: Some(top_match.score),
            reason,
        })
    }
}
