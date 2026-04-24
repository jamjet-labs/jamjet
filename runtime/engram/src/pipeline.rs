//! Extraction pipeline — LLM-based fact, entity, and relationship extraction.

use crate::extract::{ExtractionConfig, ExtractionResult, Message};
use crate::llm::LlmClient;
use crate::store::MemoryError;
use std::sync::Arc;

const EXTRACTION_SYSTEM_PROMPT: &str = r#"You are a memory extraction engine. Given a conversation between a user and an assistant, extract ALL discrete facts from BOTH sides of the conversation. Be thorough — extract every specific detail, even if it seems minor.

Respond with a JSON object in EXACTLY this shape — no other fields, no extra nesting:

{
  "facts": [
    {
      "text": "The assistant recommended Escovitch Fish as a Jamaican snapper dish with fruit",
      "entities": [
        { "name": "Escovitch Fish", "entity_type": "thing" }
      ],
      "relationships": [
        { "source": "assistant", "relation": "recommended", "target": "Escovitch Fish" }
      ],
      "confidence": 0.95,
      "category": "assistant_recommendation",
      "event_date": null
    },
    {
      "text": "The user volunteered at the animal shelter fundraiser on February 14th",
      "entities": [
        { "name": "animal shelter", "entity_type": "org" }
      ],
      "relationships": [
        { "source": "user", "relation": "volunteered_at", "target": "animal shelter" }
      ],
      "confidence": 0.98,
      "category": "event",
      "event_date": "2024-02-14"
    }
  ]
}

Every entity MUST be an object with a "name" string, never a bare string. Every relationship MUST be an object with "source", "relation", and "target" strings.

Allowed entity_type values: person, place, thing, concept, pet, org.
Allowed category values: preference, event, personal_info, assistant_recommendation, health, work, location, relationship, temporal, general.

Rules:
- Extract facts from BOTH user AND assistant messages.
- From USER messages: extract personal details, experiences, activities, preferences, opinions, relationships, plans, constraints.
- From ASSISTANT messages: extract recommendations, suggestions, explanations, lists, specific details provided (recipes, phone numbers, names, prices, instructions).
- Extract EVENTS: anything that happened or will happen, with dates/times when mentioned.
- Extract PREFERENCES as individual facts: likes, dislikes, habits, routines, constraints (e.g., "before 9:30pm", "no shellfish", "prefers window seats"). Use category "preference".
- Pay special attention to specific details: proper names (people, places, brands, stores, apps), exact numbers (prices, durations, distances, quantities, ages), dates and times, job titles, relationships (spouse, friend, colleague names).
- For event_date: if a date or time is explicitly mentioned or clearly inferable from conversation context, extract it as ISO-8601 (e.g., "2024-03-15", "2024-03-15T14:30:00"). Use null if no date is mentioned.
- Always include the specific value: "The user's commute is 45 minutes each way" NOT "The user has a commute."
- Always include proper nouns: "The user redeemed a coupon at Target" NOT "The user redeemed a coupon at a store."
- Each fact should be a single, atomic statement.
- Prefer MORE facts over fewer — when in doubt, extract it.
- Do NOT extract greetings, acknowledgments, or small talk.
- Confidence should reflect how explicitly the fact was stated (explicit ≥ 0.95, inferred 0.7–0.9).
- If there are no useful facts, respond with {"facts": []}."#;

pub struct ExtractionPipeline {
    llm: Arc<dyn LlmClient>,
    config: ExtractionConfig,
}

impl ExtractionPipeline {
    pub fn new(llm: Box<dyn LlmClient>, config: ExtractionConfig) -> Self {
        Self {
            llm: Arc::from(llm),
            config,
        }
    }

    /// Extract facts from a conversation.
    pub async fn extract(&self, messages: &[Message]) -> Result<ExtractionResult, MemoryError> {
        if messages.is_empty() {
            return Ok(ExtractionResult::default());
        }

        // Build the user prompt from messages
        let conversation = messages
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");

        // Build system prompt
        let mut system = EXTRACTION_SYSTEM_PROMPT.to_string();
        if let Some(ref custom) = self.config.custom_prompt {
            system.push_str("\n\nAdditional instructions:\n");
            system.push_str(custom);
        }

        // Call LLM
        let response = self.llm.structured_output(&system, &conversation).await?;

        // Parse response
        let mut result: ExtractionResult = serde_json::from_value(response)
            .map_err(|e| MemoryError::Serialization(format!("extraction parse error: {e}")))?;

        // Drop facts with empty text (can happen when models return null).
        result.facts.retain(|f| !f.text.is_empty());

        // Apply skip_categories filter
        if !self.config.skip_categories.is_empty() {
            result.facts.retain(|f| match &f.category {
                Some(cat) => !self.config.skip_categories.contains(cat),
                None => true,
            });
        }

        Ok(result)
    }
}
