//! MCP client pool with dynamic tool list refresh (D.6).
//!
//! `McpClientPool` maintains one `McpClient` per registered MCP server and
//! spawns a background Tokio task that periodically calls `tools/list` on
//! every connected server.  Callers can look up the current tool list without
//! paying a round-trip on every workflow node execution.

use crate::client::McpClient;
use crate::types::McpTool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// A shared, auto-refreshing cache of tools per MCP server.
type ToolCache = Arc<RwLock<HashMap<String, Vec<McpTool>>>>;

/// Pool of MCP clients with a background tool-refresh task.
pub struct McpClientPool {
    /// One client per server name.
    clients: Arc<RwLock<HashMap<String, Arc<McpClient>>>>,
    /// Cached tool lists keyed by server name.
    tool_cache: ToolCache,
    /// How often to re-discover tools from each server.
    refresh_interval: Duration,
}

impl McpClientPool {
    /// Create a new pool.
    ///
    /// `refresh_interval` controls how often `tools/list` is called on each
    /// server in the background.  A value of `Duration::ZERO` disables
    /// automatic refresh (tools are only loaded once on `add_client`).
    pub fn new(refresh_interval: Duration) -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            tool_cache: Arc::new(RwLock::new(HashMap::new())),
            refresh_interval,
        }
    }

    /// Register a connected `McpClient` under `server_name` and perform an
    /// initial `tools/list` discovery.
    pub async fn add_client(&self, server_name: String, client: McpClient) {
        let client = Arc::new(client);

        // Seed the cache with the initial tool list.
        match client.list_tools().await {
            Ok(tools) => {
                info!(
                    server = %server_name,
                    tool_count = tools.len(),
                    "Initial MCP tool discovery complete"
                );
                self.tool_cache
                    .write()
                    .await
                    .insert(server_name.clone(), tools);
            }
            Err(e) => {
                warn!(server = %server_name, error = %e, "Initial MCP tool discovery failed");
                self.tool_cache
                    .write()
                    .await
                    .insert(server_name.clone(), vec![]);
            }
        }

        self.clients.write().await.insert(server_name, client);
    }

    /// Return a snapshot of the cached tool list for `server_name`.
    pub async fn tools(&self, server_name: &str) -> Vec<McpTool> {
        self.tool_cache
            .read()
            .await
            .get(server_name)
            .cloned()
            .unwrap_or_default()
    }

    /// Return cached tool lists for all registered servers.
    pub async fn all_tools(&self) -> HashMap<String, Vec<McpTool>> {
        self.tool_cache.read().await.clone()
    }

    /// Look up a client by server name (for direct invocations).
    pub async fn client(&self, server_name: &str) -> Option<Arc<McpClient>> {
        self.clients.read().await.get(server_name).cloned()
    }

    /// Spawn the background refresh loop.
    ///
    /// The returned `JoinHandle` can be aborted to shut down the refresh task.
    /// If `refresh_interval` is zero this is a no-op and returns immediately.
    pub fn spawn_refresh_task(&self) -> tokio::task::JoinHandle<()> {
        if self.refresh_interval.is_zero() {
            return tokio::spawn(async {});
        }

        let interval = self.refresh_interval;
        let clients = Arc::clone(&self.clients);
        let cache = Arc::clone(&self.tool_cache);

        tokio::spawn(async move {
            info!(
                interval_secs = interval.as_secs(),
                "MCP tool refresh task started"
            );
            loop {
                tokio::time::sleep(interval).await;
                Self::refresh_all(&clients, &cache).await;
            }
        })
    }

    /// Perform a single refresh pass across all registered servers.
    async fn refresh_all(
        clients: &Arc<RwLock<HashMap<String, Arc<McpClient>>>>,
        cache: &ToolCache,
    ) {
        let snapshot: Vec<(String, Arc<McpClient>)> = clients
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect();

        for (name, client) in snapshot {
            match client.list_tools().await {
                Ok(tools) => {
                    let count = tools.len();
                    cache.write().await.insert(name.clone(), tools);
                    debug!(server = %name, tool_count = count, "MCP tools refreshed");
                }
                Err(e) => {
                    warn!(server = %name, error = %e, "MCP tool refresh failed — keeping stale cache");
                }
            }
        }
    }

    /// Trigger an immediate refresh for a specific server (e.g. after a
    /// server-side `tools/list_changed` notification).
    pub async fn refresh_server(&self, server_name: &str) {
        let client = self.clients.read().await.get(server_name).cloned();
        let Some(client) = client else {
            warn!(server = %server_name, "refresh_server: unknown server");
            return;
        };
        match client.list_tools().await {
            Ok(tools) => {
                let count = tools.len();
                self.tool_cache
                    .write()
                    .await
                    .insert(server_name.to_string(), tools);
                info!(server = %server_name, tool_count = count, "MCP tools refreshed on demand");
            }
            Err(e) => {
                warn!(server = %server_name, error = %e, "On-demand MCP tool refresh failed");
            }
        }
    }
}
