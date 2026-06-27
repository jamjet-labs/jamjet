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

impl ModelRegistry {
    /// The system name of the current default adapter, if one is set.
    ///
    /// Primarily for tests and introspection — not on the hot path.
    pub fn default_system(&self) -> Option<&str> {
        self.default.as_deref()
    }
}

/// Build a `ModelRegistry` from environment variables.
///
/// Registers adapters based on available API keys / services:
/// - Anthropic if `ANTHROPIC_API_KEY` is set
/// - OpenAI if `OPENAI_API_KEY` is set
/// - Google if `GOOGLE_API_KEY` or `GEMINI_API_KEY` is set
/// - Ollama if `OLLAMA_HOST` is set or defaults to localhost:11434
///
/// If `JAMJET_MODEL_SEAM_URL` is set, all native adapters and prefix routes are
/// DISCARDED and a sidecar-only registry is returned via [`apply_sidecar`].
/// Every model string — with or without a provider prefix — routes to the
/// governed Python seam.  No native bypass paths survive in seam mode.
///
/// Sets up standard prefix routing:
///   claude-* → anthropic, gpt-*/o1-*/o3-* → openai,
///   gemini-* → google, ollama model names → ollama.
///
/// **Does NOT probe the sidecar health endpoint.** Call
/// [`registry_from_env_checked`] at startup if you need the fail-loud guard.
pub fn registry_from_env() -> ModelRegistry {
    use crate::{
        anthropic::AnthropicAdapter, google::GoogleAdapter, ollama::OllamaAdapter,
        openai::OpenAiAdapter,
    };

    let mut registry = ModelRegistry::new()
        // Fully-qualified provider-prefixed strings (e.g. "anthropic/claude-sonnet-4-6").
        .route_prefix("anthropic/", "anthropic")
        .route_prefix("openai/", "openai")
        .route_prefix("google/", "google")
        // Bare model-name prefixes for backwards compat.
        .route_prefix("claude-", "anthropic")
        .route_prefix("gpt-", "openai")
        .route_prefix("o1-", "openai")
        .route_prefix("o3-", "openai")
        .route_prefix("gemini-", "google")
        // Common Ollama model name patterns.
        .route_prefix("llama", "ollama")
        .route_prefix("qwen", "ollama")
        .route_prefix("gemma", "ollama")
        .route_prefix("phi", "ollama")
        .route_prefix("mistral", "ollama")
        .route_prefix("codellama", "ollama")
        .route_prefix("deepseek", "ollama")
        .route_prefix("nomic-", "ollama");

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

    if let Ok(adapter) = GoogleAdapter::from_env() {
        registry = registry.register(Arc::new(adapter));
        if registry.default.is_none() {
            registry = registry.with_default("google");
        }
    }

    // Ollama is always available if the server is running (no API key needed).
    // Register it but don't set as default — cloud providers take priority.
    if let Ok(adapter) = OllamaAdapter::from_env() {
        registry = registry.register(Arc::new(adapter));
        if registry.default.is_none() {
            registry = registry.with_default("ollama");
        }
    }

    // Sidecar takes highest priority when configured — overrides the native default.
    // Native adapters remain registered as prefix-routed fallbacks so explicit
    // provider model strings (e.g. "claude-3-haiku") still route correctly.
    if let Ok(url) = std::env::var("JAMJET_MODEL_SEAM_URL") {
        registry = apply_sidecar(registry, url);
    }

    registry
}

/// Build a sidecar-only `ModelRegistry` for seam mode.
///
/// In seam mode ALL model calls — regardless of model name or prefix — must go
/// through the governed Python sidecar. We therefore return a FRESH registry
/// containing ONLY the `SidecarModelAdapter`, with no native adapters and no
/// prefix routes. Any model string (bare `"claude-sonnet-4-6"`, qualified
/// `"anthropic/claude-3"`, or empty) falls through to the sidecar default.
///
/// The incoming `_registry` (which may contain native adapters built from env
/// vars) is intentionally discarded — registering native adapters alongside the
/// sidecar would keep the bypass paths alive.
///
/// Extracted so tests can call it directly without touching env vars.
pub(crate) fn apply_sidecar(_registry: ModelRegistry, url: String) -> ModelRegistry {
    use crate::sidecar::SidecarModelAdapter;
    ModelRegistry::new()
        .register(Arc::new(SidecarModelAdapter::new(url)))
        .with_default("sidecar")
}

/// Like [`registry_from_env`] but also probes the sidecar `/health` endpoint.
///
/// Returns `Err` if `JAMJET_MODEL_SEAM_URL` is set but the sidecar is
/// unreachable or responds non-2xx — so a misconfigured deployment fails loud
/// at startup rather than silently falling through to the native adapters.
///
/// Call this at the `main()` call site instead of `registry_from_env()`.
pub async fn registry_from_env_checked() -> Result<ModelRegistry, ModelError> {
    let registry = registry_from_env();
    if let Ok(url) = std::env::var("JAMJET_MODEL_SEAM_URL") {
        let client = reqwest::Client::new();
        crate::sidecar::check_sidecar_health(&url, &client).await?;
    }
    Ok(registry)
}

// ── Registry wiring tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialise env-var-mutating tests to avoid races between parallel test threads.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn apply_sidecar_sets_sidecar_as_default() {
        let registry = apply_sidecar(ModelRegistry::new(), "http://127.0.0.1:4280".into());
        assert_eq!(
            registry.default_system(),
            Some("sidecar"),
            "sidecar must be the default when URL is wired"
        );
    }

    #[test]
    fn apply_sidecar_registers_adapter_by_name() {
        let registry = apply_sidecar(ModelRegistry::new(), "http://127.0.0.1:4280".into());
        // The adapter must be reachable (resolve returns Some for empty model name).
        let adapter = registry.resolve("");
        assert!(adapter.is_some(), "sidecar adapter must be registered");
        assert_eq!(adapter.unwrap().system_name(), "sidecar");
    }

    #[test]
    fn registry_from_env_sets_sidecar_default_when_url_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Safety: guarded by ENV_LOCK; removed immediately after.
        unsafe {
            std::env::set_var("JAMJET_MODEL_SEAM_URL", "http://127.0.0.1:4280");
        }
        let registry = registry_from_env();
        unsafe {
            std::env::remove_var("JAMJET_MODEL_SEAM_URL");
        }
        assert_eq!(
            registry.default_system(),
            Some("sidecar"),
            "registry_from_env must make sidecar the default when JAMJET_MODEL_SEAM_URL is set"
        );
    }

    #[test]
    fn registry_from_env_no_sidecar_when_url_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("JAMJET_MODEL_SEAM_URL");
        }
        let registry = registry_from_env();
        assert_ne!(
            registry.default_system(),
            Some("sidecar"),
            "sidecar must not be default when JAMJET_MODEL_SEAM_URL is absent"
        );
    }

    #[tokio::test]
    async fn registry_from_env_checked_errors_on_unreachable_sidecar() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            // Port 1 is never open — connection will be refused immediately.
            std::env::set_var("JAMJET_MODEL_SEAM_URL", "http://127.0.0.1:1");
        }
        let result = registry_from_env_checked().await;
        unsafe {
            std::env::remove_var("JAMJET_MODEL_SEAM_URL");
        }
        assert!(
            result.is_err(),
            "registry_from_env_checked must fail when sidecar is unreachable"
        );
    }

    /// C2A: in seam mode ALL model strings must resolve to the sidecar adapter.
    ///
    /// This test would FAIL under the old `apply_sidecar` (which kept native
    /// prefix routes alive — a bare "claude-..." would bypass the sidecar if
    /// ANTHROPIC_API_KEY was set). It passes after the fix where `apply_sidecar`
    /// returns a fresh sidecar-only registry.
    #[test]
    fn seam_mode_all_model_strings_route_to_sidecar() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("JAMJET_MODEL_SEAM_URL", "http://127.0.0.1:4280");
        }
        let registry = registry_from_env();
        unsafe {
            std::env::remove_var("JAMJET_MODEL_SEAM_URL");
        }

        // Every model string — bare, qualified, empty — must route to sidecar.
        let cases = [
            "claude-sonnet-4-6",  // bare string: old bug — routed to native anthropic
            "anthropic/claude-3", // qualified: also bypassed via prefix route
            "gpt-4",              // would have routed to native openai
            "",                   // unspecified default
        ];
        for model in &cases {
            let adapter = registry.resolve(model);
            assert!(
                adapter.is_some(),
                "seam mode: adapter must exist for model string {model:?}"
            );
            assert_eq!(
                adapter.unwrap().system_name(),
                "sidecar",
                "seam mode: model string {model:?} must route to sidecar, not a native adapter"
            );
        }
    }

    /// C2A non-seam: prefix routing to native adapters still works without sidecar.
    ///
    /// Builds a registry manually (no env vars needed) with a stub adapter and
    /// verifies that prefix routes function correctly in non-seam mode.
    #[test]
    fn non_seam_prefix_routes_work() {
        use crate::adapter::{
            ModelAdapter, ModelError, ModelRequest, ModelResponse, StructuredRequest,
        };

        struct StubAdapter(&'static str);
        #[async_trait::async_trait]
        impl ModelAdapter for StubAdapter {
            fn system_name(&self) -> &'static str {
                self.0
            }
            fn default_model(&self) -> &str {
                "stub"
            }
            async fn chat(&self, _: ModelRequest) -> Result<ModelResponse, ModelError> {
                unimplemented!()
            }
            async fn structured_output(
                &self,
                _: StructuredRequest,
            ) -> Result<ModelResponse, ModelError> {
                unimplemented!()
            }
        }

        let registry = ModelRegistry::new()
            .route_prefix("anthropic/", "anthropic")
            .route_prefix("claude-", "anthropic")
            .route_prefix("gpt-", "openai")
            .register(Arc::new(StubAdapter("anthropic")))
            .register(Arc::new(StubAdapter("openai")))
            .with_default("anthropic");

        // Qualified prefix routes to the right adapter.
        let a = registry.resolve("anthropic/claude-sonnet-4-6");
        assert_eq!(a.unwrap().system_name(), "anthropic");

        // Bare model prefix routes correctly.
        let b = registry.resolve("claude-3-haiku");
        assert_eq!(b.unwrap().system_name(), "anthropic");

        let c = registry.resolve("gpt-4");
        assert_eq!(c.unwrap().system_name(), "openai");

        // Unrecognised string falls to the default.
        let d = registry.resolve("unknown-model");
        assert_eq!(d.unwrap().system_name(), "anthropic");
    }
}
