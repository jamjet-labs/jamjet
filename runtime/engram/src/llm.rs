//! `LlmClient` trait — lightweight LLM interface for extraction.
//!
//! Engram defines its own LLM trait to stay standalone (no jamjet-models dep).
//! Users in the JamJet ecosystem can wrap a ModelAdapter trivially.

use crate::store::MemoryError;
use async_trait::async_trait;

/// Lightweight LLM client for memory extraction and conflict resolution.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Send a chat completion request and return the response text.
    async fn complete(&self, system: &str, user: &str) -> Result<String, MemoryError>;

    /// Send a chat completion requesting structured JSON output.
    /// Returns the parsed JSON value.
    async fn structured_output(
        &self,
        system: &str,
        user: &str,
    ) -> Result<serde_json::Value, MemoryError>;
}

/// A mock LLM client for testing. Returns configurable canned responses.
pub struct MockLlmClient {
    responses: std::sync::Mutex<Vec<serde_json::Value>>,
}

impl MockLlmClient {
    /// Create a mock that returns the given responses in order.
    /// Each call to `complete` or `structured_output` pops the next response.
    pub fn new(responses: Vec<serde_json::Value>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn complete(&self, _system: &str, _user: &str) -> Result<String, MemoryError> {
        let mut queue = self
            .responses
            .lock()
            .map_err(|e| MemoryError::Database(format!("mock lock error: {e}")))?;
        let val = queue.pop().ok_or_else(|| {
            MemoryError::Database("mock LLM: no more responses".to_string())
        })?;
        Ok(val.as_str().unwrap_or(&val.to_string()).to_string())
    }

    async fn structured_output(
        &self,
        _system: &str,
        _user: &str,
    ) -> Result<serde_json::Value, MemoryError> {
        let mut queue = self
            .responses
            .lock()
            .map_err(|e| MemoryError::Database(format!("mock lock error: {e}")))?;
        queue.pop().ok_or_else(|| {
            MemoryError::Database("mock LLM: no more responses".to_string())
        })
    }
}
