//! Core domain types for the Engram memory layer.
//!
//! - `Fact` — the atomic unit of memory: a piece of text with provenance,
//!   temporal validity, and optional embedding.
//! - `Entity` — a named thing (person, place, concept) extracted from facts.
//! - `Relationship` — a typed, time-bounded edge between two entities.
//! - `SubGraph` — a snapshot of entities and relationships.
//! - `FactPatch` — a partial update payload for `FactStore::update_fact`.
//! - `FactFilter` — query parameters for `FactStore::list_facts`.

use crate::scope::Scope;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

pub type FactId = Uuid;
pub type EntityId = Uuid;
pub type RelationshipId = Uuid;

// ---------------------------------------------------------------------------
// MemoryTier
// ---------------------------------------------------------------------------

/// The retention tier of a `Fact`.
///
/// - `Working` — ephemeral; cleared between agent invocations.
/// - `Conversation` — lasts for the duration of a session (default).
/// - `Knowledge` — long-term; persisted across sessions indefinitely.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryTier {
    Working,
    #[default]
    Conversation,
    Knowledge,
}

// ---------------------------------------------------------------------------
// Fact
// ---------------------------------------------------------------------------

/// An atomic unit of agent memory.
///
/// A `Fact` is the fundamental record stored by Engram. It carries free-text
/// content alongside rich provenance metadata, temporal validity bounds, an
/// optional embedding vector for semantic search, and a supersession chain for
/// versioned knowledge updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: FactId,
    /// Human-readable text content of this fact.
    pub text: String,
    /// Memory scope this fact belongs to.
    pub scope: Scope,
    /// Retention tier.
    pub tier: MemoryTier,
    /// Optional free-form category tag (e.g. `"preference"`, `"task_result"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Identifier of the source that produced this fact (agent id, tool name, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Confidence score in [0.0, 1.0]. `None` means "unrated".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// When this fact became valid. Defaults to creation time.
    pub valid_from: DateTime<Utc>,
    /// When this fact stops being valid. `None` means "still valid".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// Dense embedding vector (e.g. 1536-dim for `text-embedding-3-small`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedding: Vec<f32>,
    /// UUIDs of entities this fact references.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entity_refs: Vec<EntityId>,
    /// ID of the fact this one supersedes (if this is an update).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<FactId>,
    /// ID of the fact that superseded this one (set when this fact is outdated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<FactId>,
    /// How many times this fact has been retrieved.
    #[serde(default)]
    pub access_count: u64,
    /// When this fact was last retrieved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_accessed: Option<DateTime<Utc>>,
    /// Arbitrary extra metadata.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

impl Fact {
    /// Create a new fact with required fields; all optional fields default to `None`/empty.
    pub fn new(text: impl Into<String>, scope: Scope) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            text: text.into(),
            scope,
            tier: MemoryTier::default(),
            category: None,
            source: None,
            confidence: None,
            valid_from: now,
            invalid_at: None,
            created_at: now,
            embedding: Vec::new(),
            entity_refs: Vec::new(),
            supersedes: None,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            metadata: serde_json::Map::new(),
        }
    }

    /// Returns `true` if this fact is currently valid (not yet expired).
    pub fn is_valid(&self) -> bool {
        self.is_valid_at(Utc::now())
    }

    /// Returns `true` if this fact was valid at the given point in time.
    pub fn is_valid_at(&self, at: DateTime<Utc>) -> bool {
        if at < self.valid_from {
            return false;
        }
        match self.invalid_at {
            Some(exp) => at < exp,
            None => true,
        }
    }

    // --- Builder helpers ---

    pub fn with_tier(mut self, tier: MemoryTier) -> Self {
        self.tier = tier;
        self
    }

    pub fn with_category(mut self, category: impl Into<String>) -> Self {
        self.category = Some(category.into());
        self
    }

    pub fn with_confidence(mut self, confidence: f32) -> Self {
        self.confidence = Some(confidence);
        self
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

/// A named, typed entity extracted from one or more facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: EntityId,
    pub name: String,
    pub entity_type: String,
    pub scope: Scope,
    /// Arbitrary key-value attributes for this entity.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub attributes: serde_json::Map<String, serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Entity {
    pub fn new(name: impl Into<String>, scope: Scope) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            entity_type: "unknown".to_string(),
            scope,
            attributes: serde_json::Map::new(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn with_type(mut self, entity_type: impl Into<String>) -> Self {
        self.entity_type = entity_type.into();
        self
    }
}

// ---------------------------------------------------------------------------
// Relationship
// ---------------------------------------------------------------------------

/// A directed, typed, time-bounded edge between two entities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub id: RelationshipId,
    /// Source entity.
    pub source_id: EntityId,
    /// Relation label (e.g. `"works_for"`, `"located_in"`).
    pub relation: String,
    /// Target entity.
    pub target_id: EntityId,
    pub scope: Scope,
    pub valid_from: DateTime<Utc>,
    /// When this relationship stops being valid. `None` means "still valid".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl Relationship {
    pub fn new(
        source_id: EntityId,
        relation: impl Into<String>,
        target_id: EntityId,
        scope: Scope,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            source_id,
            relation: relation.into(),
            target_id,
            scope,
            valid_from: now,
            invalid_at: None,
            created_at: now,
        }
    }

    /// Returns `true` if this relationship is currently valid.
    pub fn is_valid(&self) -> bool {
        self.is_valid_at(Utc::now())
    }

    /// Returns `true` if this relationship was valid at the given point in time.
    pub fn is_valid_at(&self, at: DateTime<Utc>) -> bool {
        if at < self.valid_from {
            return false;
        }
        match self.invalid_at {
            Some(exp) => at < exp,
            None => true,
        }
    }
}

// ---------------------------------------------------------------------------
// SubGraph
// ---------------------------------------------------------------------------

/// A snapshot of a portion of the entity-relationship graph.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubGraph {
    pub entities: Vec<Entity>,
    pub relationships: Vec<Relationship>,
}

// ---------------------------------------------------------------------------
// FactPatch
// ---------------------------------------------------------------------------

/// Partial update payload for `FactStore::update_fact`.
///
/// Only `Some` fields are applied; `None` fields leave the stored value unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FactPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<MemoryTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedding: Vec<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<FactId>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// FactFilter
// ---------------------------------------------------------------------------

/// Query parameters for `FactStore::list_facts`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FactFilter {
    /// Restrict to facts within this scope (and child scopes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
    /// Filter by memory tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<MemoryTier>,
    /// Filter by category tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// When `true`, only return facts that are currently valid. Default `true`.
    #[serde(default = "default_valid_only")]
    pub valid_only: bool,
    /// Point-in-time query — return facts that were valid at this instant.
    /// If `None`, uses the current time (combined with `valid_only`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of: Option<DateTime<Utc>>,
    /// Substring filter on `Fact::text` (case-insensitive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_contains: Option<String>,
    /// Maximum number of facts to return. Default 50.
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Pagination offset. Default 0.
    #[serde(default)]
    pub offset: u32,
}

fn default_valid_only() -> bool {
    true
}

fn default_limit() -> u32 {
    50
}

impl FactFilter {
    pub fn new() -> Self {
        Self {
            valid_only: true,
            limit: 50,
            ..Default::default()
        }
    }

    pub fn with_scope(mut self, scope: Scope) -> Self {
        self.scope = Some(scope);
        self
    }

    pub fn with_tier(mut self, tier: MemoryTier) -> Self {
        self.tier = Some(tier);
        self
    }

    /// Also include facts that have been invalidated.
    pub fn include_invalid(mut self) -> Self {
        self.valid_only = false;
        self
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn org_scope() -> Scope {
        Scope::org("acme")
    }

    #[test]
    fn fact_new_defaults() {
        let f = Fact::new("Alice likes Rust", org_scope());
        assert_eq!(f.text, "Alice likes Rust");
        assert_eq!(f.tier, MemoryTier::Conversation);
        assert!(f.is_valid());
        assert!(f.embedding.is_empty());
        assert_eq!(f.access_count, 0);
    }

    #[test]
    fn fact_builder_methods() {
        let f = Fact::new("test", org_scope())
            .with_tier(MemoryTier::Knowledge)
            .with_category("preference")
            .with_confidence(0.9)
            .with_source("gpt-4o");
        assert_eq!(f.tier, MemoryTier::Knowledge);
        assert_eq!(f.category.as_deref(), Some("preference"));
        assert_eq!(f.confidence, Some(0.9));
        assert_eq!(f.source.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn fact_is_valid_at_before_valid_from() {
        let future = Utc::now() + chrono::Duration::hours(1);
        let mut f = Fact::new("future fact", org_scope());
        f.valid_from = future;
        assert!(!f.is_valid());
    }

    #[test]
    fn fact_is_valid_at_after_invalid_at() {
        let past = Utc::now() - chrono::Duration::hours(1);
        let mut f = Fact::new("expired fact", org_scope());
        f.invalid_at = Some(past);
        assert!(!f.is_valid());
    }

    #[test]
    fn entity_new_and_with_type() {
        let e = Entity::new("Anthropic", org_scope()).with_type("organization");
        assert_eq!(e.name, "Anthropic");
        assert_eq!(e.entity_type, "organization");
    }

    #[test]
    fn relationship_new_and_validity() {
        let src = Uuid::new_v4();
        let tgt = Uuid::new_v4();
        let r = Relationship::new(src, "founded_by", tgt, org_scope());
        assert_eq!(r.relation, "founded_by");
        assert!(r.is_valid());
    }

    #[test]
    fn fact_filter_defaults() {
        let f = FactFilter::new();
        assert!(f.valid_only);
        assert_eq!(f.limit, 50);
        assert_eq!(f.offset, 0);
    }
}
