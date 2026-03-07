//! Protocol adapter plugin registry (E2.5).
//!
//! `ProtocolRegistry` maps protocol names (e.g. `"mcp"`, `"a2a"`, `"anp"`)
//! and URL prefixes to `ProtocolAdapter` instances.  New adapters can be
//! registered at startup without modifying the runtime core.
//!
//! # Default adapters
//!
//! Call `ProtocolRegistry::with_defaults()` to get a registry pre-loaded with
//! the built-in MCP, A2A, and ANP adapters.
//!
//! # URL-based dispatch
//!
//! When invoking via URL, the registry picks the adapter by matching the
//! longest registered prefix, then falls back to a scheme/protocol-name match.
//!
//! ```text
//! did:web:example.com/analyst  → AnpAdapter   (prefix "did:")
//! http://host/mcp              → McpAdapter   (explicit registration)
//! https://host/a2a             → A2aAdapter   (explicit registration)
//! ```

use crate::ProtocolAdapter;
use std::{collections::HashMap, sync::Arc};
use tracing::{debug, warn};

/// A registry of named protocol adapters.
///
/// Thread-safe: uses `Arc` internally so it can be cloned across Tokio tasks.
#[derive(Clone, Default)]
pub struct ProtocolRegistry {
    /// Adapters keyed by protocol name (e.g. `"mcp"`, `"a2a"`, `"anp"`).
    adapters: HashMap<String, Arc<dyn ProtocolAdapter>>,
    /// URL prefix → protocol name mapping for URL-based dispatch.
    /// Matched in registration order; first prefix that matches wins.
    url_prefixes: Vec<(String, String)>, // (prefix, protocol_name)
}

impl ProtocolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an adapter under `protocol_name`.
    ///
    /// Optionally bind one or more URL prefixes to this adapter for automatic
    /// dispatch via [`Self::adapter_for_url`].
    pub fn register(
        &mut self,
        protocol_name: impl Into<String>,
        adapter: Arc<dyn ProtocolAdapter>,
        url_prefixes: impl IntoIterator<Item = impl Into<String>>,
    ) {
        let name: String = protocol_name.into();
        for prefix in url_prefixes {
            self.url_prefixes.push((prefix.into(), name.clone()));
        }
        debug!(protocol = %name, "Registered protocol adapter");
        self.adapters.insert(name, adapter);
    }

    /// Look up an adapter by protocol name.
    pub fn adapter(&self, protocol_name: &str) -> Option<Arc<dyn ProtocolAdapter>> {
        self.adapters.get(protocol_name).cloned()
    }

    /// Look up an adapter by URL — matches on registered URL prefixes.
    ///
    /// Returns the adapter for the first matching prefix, or `None` if no
    /// prefix matches.
    pub fn adapter_for_url(&self, url: &str) -> Option<Arc<dyn ProtocolAdapter>> {
        // Longest-prefix-first: sort descending by prefix length.
        let mut candidates: Vec<_> = self
            .url_prefixes
            .iter()
            .filter(|(prefix, _)| url.starts_with(prefix.as_str()))
            .collect();
        candidates.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

        if let Some((prefix, proto)) = candidates.first() {
            debug!(url = %url, prefix = %prefix, protocol = %proto, "URL matched protocol adapter");
            self.adapters.get(proto.as_str()).cloned()
        } else {
            warn!(url = %url, "No protocol adapter matched URL");
            None
        }
    }

    /// All registered protocol names.
    pub fn protocols(&self) -> Vec<&str> {
        self.adapters.keys().map(|s| s.as_str()).collect()
    }
}

impl std::fmt::Debug for ProtocolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProtocolRegistry")
            .field("protocols", &self.protocols())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RemoteCapabilities, TaskHandle, TaskRequest, TaskStatus, TaskStream};
    use async_trait::async_trait;

    struct FakeAdapter(String);

    #[async_trait]
    impl ProtocolAdapter for FakeAdapter {
        async fn discover(&self, _url: &str) -> Result<RemoteCapabilities, String> {
            Ok(RemoteCapabilities {
                name: self.0.clone(),
                description: None,
                skills: vec![],
                protocols: vec![self.0.clone()],
            })
        }
        async fn invoke(&self, _url: &str, _task: TaskRequest) -> Result<TaskHandle, String> {
            Err("not implemented".into())
        }
        async fn stream(&self, _url: &str, _task: TaskRequest) -> Result<TaskStream, String> {
            Err("not implemented".into())
        }
        async fn status(&self, _url: &str, _task_id: &str) -> Result<TaskStatus, String> {
            Err("not implemented".into())
        }
        async fn cancel(&self, _url: &str, _task_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn test_register_and_lookup_by_name() {
        let mut reg = ProtocolRegistry::new();
        reg.register(
            "mcp",
            Arc::new(FakeAdapter("mcp".into())),
            vec!["http://mcp/"],
        );
        assert!(reg.adapter("mcp").is_some());
        assert!(reg.adapter("a2a").is_none());
    }

    #[test]
    fn test_adapter_for_url_matches_prefix() {
        let mut reg = ProtocolRegistry::new();
        reg.register("anp", Arc::new(FakeAdapter("anp".into())), vec!["did:"]);
        reg.register(
            "mcp",
            Arc::new(FakeAdapter("mcp".into())),
            vec!["http://mcp."],
        );

        assert!(reg.adapter_for_url("did:web:example.com").is_some());
        assert!(reg
            .adapter_for_url("http://mcp.example.com/tools")
            .is_some());
        assert!(reg.adapter_for_url("https://unknown.com").is_none());
    }

    #[test]
    fn test_longest_prefix_wins() {
        let mut reg = ProtocolRegistry::new();
        reg.register(
            "generic-http",
            Arc::new(FakeAdapter("generic".into())),
            vec!["http://"],
        );
        reg.register(
            "specific-mcp",
            Arc::new(FakeAdapter("specific".into())),
            vec!["http://mcp.example.com/"],
        );

        let adapter = reg
            .adapter_for_url("http://mcp.example.com/v1")
            .expect("should match");
        // The specific adapter should win (longer prefix).
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let caps = adapter.discover("").await.unwrap();
            assert_eq!(caps.name, "specific");
        });
    }

    #[test]
    fn test_protocols_list() {
        let mut reg = ProtocolRegistry::new();
        reg.register(
            "mcp",
            Arc::new(FakeAdapter("mcp".into())),
            vec![] as Vec<String>,
        );
        reg.register(
            "a2a",
            Arc::new(FakeAdapter("a2a".into())),
            vec![] as Vec<String>,
        );
        let mut protos = reg.protocols();
        protos.sort();
        assert_eq!(protos, vec!["a2a", "mcp"]);
    }
}
