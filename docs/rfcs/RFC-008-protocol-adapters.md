# RFC-008: Protocol Adapter Framework

| Field | Value |
|-------|-------|
| RFC | 008 |
| Status | Draft |
| Created | 2026-03-07 |

---

## Summary

Defines the extensible protocol adapter framework that allows new agent communication protocols to be added without modifying the runtime core.

---

## Key Design Points

### ProtocolAdapter Trait
```rust
#[async_trait]
pub trait ProtocolAdapter: Send + Sync {
    async fn discover(&self, url: &str) -> Result<RemoteCapabilities>;
    async fn invoke(&self, task: TaskRequest) -> Result<TaskHandle>;
    async fn stream(&self, task: TaskRequest) -> Result<impl Stream<Item = TaskEvent>>;
    async fn status(&self, task_id: &TaskId) -> Result<TaskStatus>;
    async fn cancel(&self, task_id: &TaskId) -> Result<()>;
}
```

### Adapter Registry
Adapters are registered by protocol name. The scheduler routes protocol nodes to the appropriate adapter. New protocols require no changes to core, scheduler, or state crates.

### Built-in Adapters
- `jamjet-mcp` — MCP client/server
- `jamjet-a2a` — A2A client/server
- gRPC internal transport

### Extension Mechanism
In v1: Rust crate implementing the trait, registered at startup.
In v2: Out-of-process gRPC-based adapters for stronger isolation.

---

## Implementation Plan
See progress-tracker.md tasks E.2.1–E.2.5 (Phase 2).
