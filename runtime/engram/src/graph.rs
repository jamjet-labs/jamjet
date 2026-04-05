//! `GraphStore` trait — the storage interface for the entity-relationship graph.
//!
//! Implementations (SQLite, Postgres, in-memory) must implement this trait.
//! The trait is `async_trait`-annotated and requires `Send + Sync` so it can
//! be used behind an `Arc<dyn GraphStore>` across async task boundaries.

use crate::fact::{Entity, EntityId, Relationship, RelationshipId, SubGraph};
use crate::scope::Scope;
use crate::store::MemoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// Storage interface for the entity-relationship knowledge graph.
///
/// All mutation methods are fallible and return `Result<_, MemoryError>`.
/// Temporal queries use RFC 3339 `DateTime<Utc>` values for point-in-time
/// filtering consistent with `FactStore` semantics.
#[async_trait]
pub trait GraphStore: Send + Sync {
    /// Insert or update an entity.
    ///
    /// If an entity with `entity.id` already exists, its `name`, `entity_type`,
    /// `attributes`, and `updated_at` fields are overwritten. Scope and
    /// `created_at` are preserved.
    async fn upsert_entity(&self, entity: &Entity) -> Result<(), MemoryError>;

    /// Insert or update a relationship.
    ///
    /// If a relationship with `rel.id` already exists, its `relation` and
    /// `invalid_at` fields are overwritten.
    async fn upsert_relationship(&self, rel: &Relationship) -> Result<(), MemoryError>;

    /// Mark a relationship as invalid as of `invalid_at`.
    ///
    /// Does not delete the record; historical graph queries still traverse it.
    async fn invalidate_relationship(
        &self,
        id: RelationshipId,
        invalid_at: DateTime<Utc>,
    ) -> Result<(), MemoryError>;

    /// Retrieve a single entity by id. Returns `None` if not found.
    async fn get_entity(&self, id: EntityId) -> Result<Option<Entity>, MemoryError>;

    /// BFS neighbourhood query starting from `id`.
    ///
    /// Expands up to `depth` hops along currently-valid relationships. When
    /// `as_of` is `Some(t)`, a relationship is considered valid if
    /// `valid_from <= t AND (invalid_at IS NULL OR invalid_at > t)`. When
    /// `as_of` is `None`, only relationships with `invalid_at IS NULL` are
    /// traversed.
    ///
    /// Returns a `SubGraph` containing all discovered entities (excluding the
    /// start node) and the relationships that connect them.
    async fn neighbors(
        &self,
        id: EntityId,
        depth: u8,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<SubGraph, MemoryError>;

    /// Full-text search over entity names.
    ///
    /// Returns up to `top_k` entities whose `name` contains `query`
    /// (case-insensitive substring match).
    async fn search_entities(&self, query: &str, top_k: usize) -> Result<Vec<Entity>, MemoryError>;

    /// Hard-delete all entities and relationships belonging to `scope`.
    ///
    /// Relationships are deleted first to avoid foreign-key-style dangling
    /// references. Returns the total number of rows deleted (entities +
    /// relationships combined).
    async fn delete_by_scope(&self, scope: &Scope) -> Result<u64, MemoryError>;
}
