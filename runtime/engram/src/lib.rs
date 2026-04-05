//! Engram — durable memory layer for AI agents.
//!
//! Provides a temporal knowledge graph, semantic search, and MCP-native tools
//! for agents running on the JamJet runtime. Memory is scoped, versioned, and
//! queryable across time — enabling agents to reason over what they knew, when.

pub mod fact;
pub mod scope;
pub mod store;
pub mod store_sqlite;

pub use fact::{
    Entity, EntityId, Fact, FactFilter, FactId, FactPatch, MemoryTier, Relationship,
    RelationshipId, SubGraph,
};
pub use scope::Scope;
pub use store::{FactStore, MemoryError, StoreStats};
pub use store_sqlite::SqliteFactStore;
