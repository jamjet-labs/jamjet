# Protocol Adapter Framework

JamJet's protocol layer is extensible. New agent communication protocols can be added without modifying the runtime core.

---

## ProtocolAdapter Trait

```rust
#[async_trait]
pub trait ProtocolAdapter: Send + Sync {
    /// Discover remote capabilities (fetch Agent Card or equivalent)
    async fn discover(&self, url: &str) -> Result<RemoteCapabilities>;

    /// Submit a task/request to the remote
    async fn invoke(&self, task: TaskRequest) -> Result<TaskHandle>;

    /// Stream task progress events
    async fn stream(&self, task: TaskRequest) -> Result<impl Stream<Item = TaskEvent>>;

    /// Poll task status
    async fn status(&self, task_id: &TaskId) -> Result<TaskStatus>;

    /// Cancel a running task
    async fn cancel(&self, task_id: &TaskId) -> Result<()>;
}
```

---

## Built-in Adapters

```
┌──────────────────────────────────────────────────┐
│                  JamJet Agent                     │
├──────────────────────────────────────────────────┤
│           Agent Communication API                │
├─────────┬──────────┬──────────┬──────────────────┤
│   MCP   │   A2A    │  gRPC    │  Future Proto    │
│ Adapter │ Adapter  │ Adapter  │    Adapter       │
└─────────┴──────────┴──────────┴──────────────────┘
```

| Adapter | Crate | Status |
|---------|-------|--------|
| MCP client/server | `jamjet-mcp` | Phase 1/2 |
| A2A client/server | `jamjet-a2a` | Phase 2 |
| gRPC (internal) | `jamjet-api` | Phase 1 |

---

## Adding a New Protocol

1. Create a new crate implementing `ProtocolAdapter`
2. Register it in the plugin registry
3. Reference it in workflow YAML as a new node type or server type

New protocol adapters require no changes to `jamjet-core`, `jamjet-scheduler`, or `jamjet-state`.
