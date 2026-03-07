# MCP Integration Guide

Connect JamJet agents to external MCP servers, and expose your agent's tools as an MCP server.

---

## Consuming an MCP Server

### Configuration

```yaml
# agents.yaml
agents:
  researcher:
    model: default_chat
    mcp:
      servers:
        github:
          transport: http_sse
          url: https://mcp.github.com/v1
          auth:
            type: bearer
            token_env: GITHUB_TOKEN

        filesystem:
          transport: stdio
          command: npx
          args: ["-y", "@modelcontextprotocol/server-filesystem", "/data"]
```

### Using MCP tools in a workflow

```yaml
nodes:
  search_code:
    type: mcp_tool
    server: github
    tool: search_code
    input:
      query: "{{ state.search_query }}"
      language: python
    output_schema: schemas.SearchResults
    retry_policy: io_default
    next: analyze_results
```

### Discovering available tools

```bash
jamjet mcp connect http://localhost:8080/mcp
```

Output:
```
Connected to MCP server at http://localhost:8080/mcp
Available tools:
  - search_code       Search code in repositories
  - read_file         Read a file from the repository
  - list_issues       List open issues
  ...
```

---

## Exposing Your Agent as an MCP Server

```yaml
# agents.yaml
agents:
  analyst:
    model: default_chat
    mcp:
      expose_as_server:
        enabled: true
        transport: http_sse
        port: 9090
        expose:
          tools: [analyze_data, search_docs]
          resources: [agent_memory]
```

Start the agent:
```bash
jamjet agents activate analyst
```

Your agent is now an MCP server at `http://localhost:9090`. Any MCP client — VS Code, Cursor, another JamJet agent, any other framework — can discover and call `analyze_data` and `search_docs`.

---

## Transports

| Transport | Config | Use case |
|-----------|--------|----------|
| `stdio` | `command`, `args` | Local subprocess MCP servers |
| `http_sse` | `url` | Remote HTTP MCP servers |

---

## Durability

MCP tool calls are fully durable. If the runtime crashes mid-call, the node is retried after the worker lease expires. Design your MCP tools to be idempotent where possible.
