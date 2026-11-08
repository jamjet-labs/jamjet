# MCP Integration Guide

The runtime is an MCP server for its own operations. A workflow cannot call an MCP tool: registration refuses `mcp_tool` nodes. To put policy in front of an MCP client's tool calls, use the [`@jamjet/mcp-shim`](https://npmjs.com/package/@jamjet/mcp-shim) adapter, or call the MCP server from a Python step and check policy there, as [`examples/05-mcp-tool-call`](../../examples/05-mcp-tool-call) does.

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

Registration refuses this node kind (400): the engine has no executor for it, so it would never run. See [Node types](yaml-reference.md#node-types).

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

### Listing a server's tools

```bash
jamjet mcp connect http://localhost:7700      # posts to <url>/mcp
```

It runs `initialize` and `tools/list` and prints the server's name and tools.
It also takes `stdio:<command>` to start a local server. It sends no token, so
against a JamJet runtime it works in dev mode only.

---

## The runtime's MCP endpoint

`POST /mcp` (JSON-RPC) and `GET /mcp/sse` expose the runtime as eight tools:

| Tool | What it does | Role outside dev mode |
|------|--------------|-----------------------|
| `jamjet_run_workflow` | Start an execution of a registered workflow | `developer` |
| `jamjet_get_execution` | Read one execution | `viewer` |
| `jamjet_list_executions` | List executions, with a status filter | `viewer` |
| `jamjet_cancel_execution` | Cancel an execution | `developer` |
| `jamjet_get_events` | Read an execution's event log | `viewer` |
| `jamjet_approve` | Decide a pending approval | `reviewer` |
| `jamjet_list_agents` | List registered agents | `viewer` |
| `jamjet_discover_agent` | Fetch a remote Agent Card and register it | `developer` |

Outside dev mode the endpoint needs the same Bearer token as the REST API, and
each tool checks the caller's role as the matching REST route does. Approving
over MCP follows the same rules as `POST /executions/:id/approve`, including
separation of duties. See [Security](security.md#rbac).

```bash
curl -s -X POST http://localhost:7700/mcp \
  -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```
