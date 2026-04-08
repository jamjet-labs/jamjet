//! Extraction pipeline — LLM-based fact, entity, and relationship extraction.

use crate::extract::{ExtractionConfig, ExtractionResult, Message};
use crate::llm::LlmClient;
use crate::store::MemoryError;
use std::sync::Arc;

const EXTRACTION_SYSTEM_PROMPT: &str = r#"You are a memory extraction engine. Given a conversation, extract discrete facts about the user or topic.

Respond with a JSON object in EXACTLY this shape — no other fields, no extra nesting:

{
  "facts": [
    {
      "text": "The user is allergic to peanuts",
      "entities": [
        { "name": "user", "entity_type": "person" },
        { "name": "peanuts", "entity_type": "thing" }
      ],
      "relationships": [
        { "source": "user", "relation": "allergic_to", "target": "peanuts" }
      ],
      "confidence": 0.98,
      "category": "health"
    },
    {
      "text": "The user lives in Bangalore",
      "entities": [
        { "name": "user", "entity_type": "person" },
        { "name": "Bangalore", "entity_type": "place" }
      ],
      "relationships": [
        { "source": "user", "relation": "lives_in", "target": "Bangalore" }
      ],
      "confidence": 0.95,
      "category": "location"
    }
  ]
}

Every entity MUST be an object with a "name" string, never a bare string. Every relationship MUST be an object with "source", "relation", and "target" strings.

Allowed entity_type values: person, place, thing, concept, pet, org.
Allowed category values: preferences, personal_info, intent, health, work, location, relationships, general.

Rules:
- Extract facts that would be useful to remember for future conversations.
- Each fact should be a single, atomic statement.
- Do NOT extract greetings, acknowledgments, or small talk.
- Do NOT repeat facts that are already implied by other facts.
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
