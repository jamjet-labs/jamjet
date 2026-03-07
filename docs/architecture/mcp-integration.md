# MCP Integration Architecture

JamJet provides native, first-class MCP (Model Context Protocol) support — as both client and server. MCP is a core protocol layer, not a plugin.

---

## MCP Client

JamJet agents consume external MCP servers.

### Transport support

| Transport | Use case |
|-----------|----------|
| `stdio` | Local/embedded MCP servers (subprocess) |
| `http_sse` | Remote MCP servers (Streamable HTTP) |
| `websocket` | Bidirectional streaming (future) |

### Client capabilities

- **Tool discovery** — `tools/list` RPC; auto-refreshed periodically
- **Tool invocation** — `tools/call` RPC with typed I/O, durably checkpointed
- **Resource access** — `resources/read` for files, data, context
- **Prompt templates** — `prompts/get` from MCP servers
- **Sampling** — server-initiated model calls through the client

### Durability guarantee

MCP tool calls go through the standard durable node pipeline:
```
mcp_tool node → work item → worker lease → MCP call → node_completed event
```
A crash during an MCP call: the lease expires, the node is re-queued, the call is retried. Tool authors should design for idempotency.

---

## MCP Server

JamJet agents expose capabilities as MCP servers, consumable by any MCP client (VS Code, Cursor, other agents, other frameworks).

### Server capabilities

- **Tool serving** — expose agent tools to external MCP clients
- **Resource serving** — expose agent state, memory, or data as resources
- **Prompt serving** — expose prompt templates

### Configuration

```yaml
mcp:
  expose_as_server:
    enabled: true
    transport: http_sse
    port: 9090
    expose:
      tools: [search_web, analyze_document]
      resources: [agent_state, memory_store]
```

---

## Dynamic Tool Discovery

- Runtime periodically calls `tools/list` on connected MCP servers
- New tools become available to agents without workflow redefinition
- Schema changes are detected and validated against node expectations
- Capability negotiation at connection time (via `initialize` handshake)

---

## Observability

All MCP interactions appear in execution traces:
- `mcp_tool_called` span with server, tool name, input, latency
- `mcp_tool_completed` with output summary
- `mcp_tool_failed` with error and retry count
- Protocol-level spans for round-trip latency measurement
