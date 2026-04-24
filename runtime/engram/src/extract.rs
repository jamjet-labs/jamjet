//! Extraction pipeline — types and configuration.
//!
//! The extraction pipeline converts raw conversation messages into structured
//! facts, entities, and relationships.

use crate::fact::MemoryTier;
use serde::{Deserialize, Deserializer, Serialize};

/// Deserialize a JSON `null` (or missing field) as an empty `String`.
/// Some models (e.g. Gemini Flash-Lite) occasionally emit `null` for
/// string fields in structured output.
fn null_as_empty<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(|opt| opt.unwrap_or_default())
}

/// A single message in a conversation (role + content).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}

/// A fact extracted by the LLM (before storage — no ID yet).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedFact {
    /// The human-readable fact text.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub text: String,
    /// Extracted entities mentioned in this fact.
    #[serde(default)]
    pub entities: Vec<ExtractedEntity>,
    /// Extracted relationships between entities.
    #[serde(default)]
    pub relationships: Vec<ExtractedRelationship>,
    /// Confidence score (0.0-1.0).
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    /// Optional category.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Optional event date in ISO-8601 format (e.g. "2024-03-15").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_date: Option<String>,
}

fn default_confidence() -> f64 {
    1.0
}

/// An entity extracted by the LLM (before storage — no UUID yet).
///
/// Deserialization is lenient: the LLM may return either a bare string
/// `"alice"` (which becomes `{ name: "alice", entity_type: None }`) or a
/// full object `{ "name": "alice", "entity_type": "person" }`. This makes
/// the pipeline robust against smaller open models that sometimes drop
/// structured fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "ExtractedEntityInput")]
pub struct ExtractedEntity {
    /// Canonical name (e.g., "Austin", "Max").
    pub name: String,
    /// Entity type (e.g., "person", "place", "pet").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<String>,
}

/// Internal — lenient deserialization target. Accepts either a bare string
/// or a struct, then converts into `ExtractedEntity`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ExtractedEntityInput {
    /// Bare string form — `"alice"`.
    Name(String),
    /// Full struct form — `{ "name": "alice", "entity_type": "person" }`.
    Full {
        name: String,
        #[serde(default)]
        entity_type: Option<String>,
    },
}

impl From<ExtractedEntityInput> for ExtractedEntity {
    fn from(input: ExtractedEntityInput) -> Self {
        match input {
            ExtractedEntityInput::Name(name) => Self {
                name,
                entity_type: None,
            },
            ExtractedEntityInput::Full { name, entity_type } => Self { name, entity_type },
        }
    }
}

/// A relationship extracted by the LLM (before storage).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedRelationship {
    /// Source entity name.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub source: String,
    /// Relationship type (e.g., "lives_in", "owns_pet").
    #[serde(default, deserialize_with = "null_as_empty")]
    pub relation: String,
    /// Target entity name.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub target: String,
}

/// The full output of the extraction pipeline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractionResult {
    /// Extracted facts.
    pub facts: Vec<ExtractedFact>,
}

/// Configuration for the extraction pipeline.
#[derive(Debug, Clone)]
pub struct ExtractionConfig {
    /// Custom extraction prompt appended to the system prompt.
    pub custom_prompt: Option<String>,
    /// Categories to skip (facts with these categories are discarded).
    pub skip_categories: Vec<String>,
    /// Rules for specific categories.
    pub rules: Vec<ExtractionRule>,
    /// Similarity threshold for dedup (0.0-1.0). Default: 0.92.
    pub dedup_threshold: f32,
    /// Default tier for extracted facts.
    pub default_tier: MemoryTier,
}

impl Default for ExtractionConfig {
    fn default() -> Self {
        Self {
            custom_prompt: None,
            skip_categories: Vec::new(),
            rules: Vec::new(),
            dedup_threshold: 0.92,
            default_tier: MemoryTier::Conversation,
        }
    }
}

/// A rule for a specific category of extracted facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionRule {
    pub category: String,
    /// Priority weight (higher = more important). Default: 1.0.
    #[serde(default = "default_priority")]
    pub priority: f64,
    /// Time-to-live before decay. None = permanent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
    /// Whether this category contains PII.
    #[serde(default)]
    pub pii: bool,
}

fn default_priority() -> f64 {
    1.0
}

/// Result of conflict detection between a new fact and an existing one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictVerdict {
    /// Identical meaning — skip the new fact (dedup).
    Duplicate,
    /// Contradicts the existing fact — invalidate old, store new.
    Contradicts,
    /// Adds new detail — store alongside existing fact.
    Refines,
    /// No conflict — store normally.
    NoConflict,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_config_defaults() {
        let cfg = ExtractionConfig::default();
        assert!((cfg.dedup_threshold - 0.92).abs() < f32::EPSILON);
        assert_eq!(cfg.default_tier, MemoryTier::Conversation);
        assert!(cfg.skip_categories.is_empty());
    }

    #[test]
    fn message_constructors() {
        let m = Message::user("hello");
        assert_eq!(m.role, "user");
        let m = Message::assistant("hi");
        assert_eq!(m.role, "assistant");
    }

    #[test]
    fn extracted_fact_deserializes_with_defaults() {
        let json = r#"{"text": "User likes pizza"}"#;
        let fact: ExtractedFact = serde_json::from_str(json).unwrap();
        assert_eq!(fact.confidence, 1.0);
        assert!(fact.entities.is_empty());
    }

    #[test]
    fn extracted_entity_accepts_bare_string() {
        let json = r#""alice""#;
        let entity: ExtractedEntity = serde_json::from_str(json).unwrap();
        assert_eq!(entity.name, "alice");
        assert!(entity.entity_type.is_none());
    }

    #[test]
    fn extracted_entity_accepts_full_struct() {
        let json = r#"{"name": "Bangalore", "entity_type": "place"}"#;
        let entity: ExtractedEntity = serde_json::from_str(json).unwrap();
        assert_eq!(entity.name, "Bangalore");
        assert_eq!(entity.entity_type.as_deref(), Some("place"));
    }

    #[test]
    fn extracted_fact_deserializes_event_date() {
        let json = r#"{"text": "User went to dentist", "event_date": "2024-03-15"}"#;
        let fact: ExtractedFact = serde_json::from_str(json).unwrap();
        assert_eq!(fact.event_date.as_deref(), Some("2024-03-15"));
    }

    #[test]
    fn extracted_fact_event_date_defaults_to_none() {
        let json = r#"{"text": "User likes pizza"}"#;
        let fact: ExtractedFact = serde_json::from_str(json).unwrap();
        assert!(fact.event_date.is_none());
    }

    #[test]
    fn extracted_fact_event_date_null_becomes_none() {
        let json = r#"{"text": "User likes pizza", "event_date": null}"#;
        let fact: ExtractedFact = serde_json::from_str(json).unwrap();
        assert!(fact.event_date.is_none());
    }

    #[test]
    fn extracted_fact_accepts_mixed_entity_shapes() {
        // A real failure case: llama3.2:3b often returns entities as a
        // mix of bare strings and partial objects in the same list.
        let json = r#"{
            "text": "The user is allergic to peanuts",
            "entities": ["user", {"name": "peanuts", "entity_type": "thing"}],
            "confidence": 0.95,
            "category": "health"
        }"#;
        let fact: ExtractedFact = serde_json::from_str(json).unwrap();
        assert_eq!(fact.entities.len(), 2);
        assert_eq!(fact.entities[0].name, "user");
        assert!(fact.entities[0].entity_type.is_none());
        assert_eq!(fact.entities[1].name, "peanuts");
        assert_eq!(fact.entities[1].entity_type.as_deref(), Some("thing"));
    }
}
