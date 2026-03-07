# RFC-006: MCP Integration

| Field | Value |
|-------|-------|
| RFC | 006 |
| Status | Draft |
| Created | 2026-03-07 |

---

## Summary

Defines how JamJet implements MCP (Model Context Protocol) as both a client (consuming external MCP servers) and a server (exposing agent tools to MCP clients).

---

## Key Design Points

### MCP Client
- Supports `stdio` and `http_sse` transports (WebSocket future)
- Tool discovery via `tools/list` — auto-refreshed periodically
- Tool invocation via `tools/call` — durable, checkpointed like any other tool node
- Resource access, prompt templates, sampling support
- MCP servers configured in YAML `mcp.servers` block

### MCP Server
- Exposes agent tools as MCP-compliant server
- HTTP+SSE transport
- Configured via `mcp.expose_as_server` YAML block
- Any MCP client (VS Code, Cursor, other agents) can consume tools

### Durability
MCP tool invocations go through the standard scheduler → worker → event log pipeline. They are checkpointed like any other node. Crash during MCP call → retry after lease expiry.

### Dynamic Discovery
Runtime periodically refreshes `tools/list` from connected servers. New tools become available without workflow redefinition. Schema changes are validated against node expectations.

---

## Implementation Plan
See progress-tracker.md tasks D.1–D.10 (Phase 1: client), C.2.1–C.2.7 (Phase 2: server).
