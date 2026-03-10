//! JamJet Model Adapters
//!
//! Provides a unified `ModelAdapter` trait with concrete implementations for:
//! - Anthropic Claude (via Messages API)
//! - OpenAI (via Chat Completions API)
//! - Google Gemini (via Generative Language API)
//! - Ollama (local inference via /api/chat)
//!
//! Each adapter records OTel GenAI span attributes and returns token counts
//! in the response for telemetry and cost attribution.

pub mod adapter;
pub mod anthropic;
pub mod google;
pub mod ollama;
pub mod openai;
pub mod registry;

pub use adapter::{
    ChatMessage, ChatRole, ModelAdapter, ModelConfig, ModelError, ModelRequest, ModelResponse,
    StructuredRequest,
};
pub use registry::ModelRegistry;
