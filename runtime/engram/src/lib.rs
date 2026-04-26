//! Engram — durable memory layer for AI agents.
//!
//! Provides a temporal knowledge graph, semantic search, and MCP-native tools
//! for agents running on the JamJet runtime. Memory is scoped, versioned, and
//! queryable across time — enabling agents to reason over what they knew, when.

pub mod conflict;
pub mod consolidation;
pub mod context;
pub mod embedding;
pub mod embedding_ollama;
pub mod extract;
pub mod fact;
pub mod graph;
pub mod graph_postgres;
pub mod graph_sqlite;
pub mod llm;
pub mod llm_anthropic;
pub mod llm_command;
pub mod llm_google;
pub mod llm_ollama;
pub mod llm_openai;
pub mod llm_util;
pub mod memory;
pub mod message;
pub mod message_postgres;
pub mod message_sqlite;
pub mod pipeline;
pub mod rerank;
pub mod retrieve;
pub mod scope;
pub mod spreading;
pub mod store;
pub mod store_postgres;
pub mod store_sqlite;
pub mod temporal_parser;
pub mod vector;
pub mod vector_embedded;

pub use consolidation::{
    ConsolidationConfig, ConsolidationEngine, ConsolidationOp, ConsolidationResult,
};
pub use context::{
    CharTokenEstimator, ContextBlock, ContextBuilder, ContextConfig, OutputFormat, TokenEstimator,
};
pub use embedding::EmbeddingProvider;
pub use embedding_ollama::OllamaEmbeddingProvider;
pub use extract::{ExtractedFact, ExtractionConfig, ExtractionResult, Message};
pub use fact::{
    Entity, EntityId, Fact, FactFilter, FactId, FactPatch, MemoryTier, Relationship,
    RelationshipId, SubGraph,
};
pub use graph::GraphStore;
pub use graph_postgres::PostgresGraphStore;
pub use graph_sqlite::SqliteGraphStore;
pub use llm::LlmClient;
pub use llm_anthropic::AnthropicLlmClient;
pub use llm_command::CommandLlmClient;
pub use llm_google::GoogleLlmClient;
pub use llm_ollama::OllamaLlmClient;
pub use llm_openai::OpenAiLlmClient;
pub use memory::Memory;
pub use message::{ChatMessage, MessageId, MessageStore};
pub use message_postgres::PostgresMessageStore;
pub use message_sqlite::SqliteMessageStore;
pub use pipeline::ExtractionPipeline;
pub use scope::Scope;
pub use store::{FactStore, MemoryError, StoreStats};
pub use store_postgres::PostgresFactStore;
pub use store_sqlite::SqliteFactStore;
pub use vector::{VectorFilter, VectorMatch, VectorStore};
pub use vector_embedded::EmbeddedVectorStore;
