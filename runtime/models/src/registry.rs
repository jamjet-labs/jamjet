//! Model registry — resolves model names to adapters.
//!
//! Supports routing rules: e.g. "claude-*" → Anthropic, "gpt-*" → OpenAI.
//! Falls back to a default adapter if no rule matches.

use crate::adapter::{ModelAdapter, ModelError, ModelRequest, ModelResponse, StructuredRequest};
use std::collections::HashMap;
use std::sync::Arc;

/// Routes model requests to the appropriate adapter.
///
/// Register adapters by `system_name()` (e.g. "anthropic", "openai").
/// The registry selects an adapter based on the model prefix in the request config,
/// or falls back to the default adapter.
pub struct ModelRegistry {
    adapters: HashMap<String, Arc<dyn ModelAdapter>>,
    /// Prefix routing: model name prefix → system name (e.g. "claude-" → "anthropic").
    prefix_routes: Vec<(String, String)>,
    default: Option<String>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self {
            adapters: HashMap::new(),
            prefix_routes: Vec::new(),
            default: None,
        }
    }

    /// Register an adapter under its system name.
    pub fn register(mut self, adapter: Arc<dyn ModelAdapter>) -> Self {
        let name = adapter.system_name().to_string();
        self.adapters.insert(name, adapter);
        self
    }

    /// Route model name prefix to a system (e.g. "claude-" → "anthropic").
    pub fn route_prefix(mut self, prefix: impl Into<String>, system: impl Into<String>) -> Self {
        self.prefix_routes.push((prefix.into(), system.into()));
        self
    }

    /// Set the default adapter to use when no prefix matches.
    pub fn with_default(mut self, system: impl Into<String>) -> Self {
        self.default = Some(system.into());
        self
    }

    /// Resolve an adapter for the given model name.
    fn resolve(&self, model: &str) -> Option<Arc<dyn ModelAdapter>> {
        // Check prefix routes first.
        for (prefix, system) in &self.prefix_routes {
            if model.starts_with(prefix.as_str()) {
                if let Some(adapter) = self.adapters.get(system) {
                    return Some(Arc::clone(adapter));
                }
            }
        }
        // Fall back to default.
        if let Some(default) = &self.default {
            return self.adapters.get(default).map(Arc::clone);
        }
        // Only one adapter registered — use it.
        if self.adapters.len() == 1 {
            return self.adapters.values().next().map(Arc::clone);
        }
        None
    }

    /// Send a chat request, routing to the appropriate adapter.
    pub async fn chat(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        let model = request.config.model.clone().unwrap_or_default();
        let adapter = self
            .resolve(&model)
            .ok_or_else(|| ModelError::Network(format!("no adapter for model: {model}")))?;
        adapter.chat(request).await
    }

    /// Send a structured output request, routing to the appropriate adapter.
    pub async fn structured_output(
        &self,
        request: StructuredRequest,
    ) -> Result<ModelResponse, ModelError> {
        let model = request.config.model.clone().unwrap_or_default();
        let adapter = self
            .resolve(&model)
            .ok_or_else(|| ModelError::Network(format!("no adapter for model: {model}")))?;
        adapter.structured_output(request).await
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a `ModelRegistry` from environment variables.
///
/// Registers Anthropic if `ANTHROPIC_API_KEY` is set, OpenAI if `OPENAI_API_KEY` is set.
/// Sets up standard prefix routing (claude-* → anthropic, gpt-* / o1-* / o3-* → openai).
pub fn registry_from_env() -> ModelRegistry {
    use crate::{anthropic::AnthropicAdapter, openai::OpenAiAdapter};

    let mut registry = ModelRegistry::new()
        .route_prefix("claude-", "anthropic")
        .route_prefix("gpt-", "openai")
        .route_prefix("o1-", "openai")
        .route_prefix("o3-", "openai");

    if let Ok(adapter) = AnthropicAdapter::from_env() {
        registry = registry.register(Arc::new(adapter));
        registry = registry.with_default("anthropic");
    }

    if let Ok(adapter) = OpenAiAdapter::from_env() {
        registry = registry.register(Arc::new(adapter));
        if registry.default.is_none() {
            registry = registry.with_default("openai");
        }
    }

    registry
}
