# 04 — MCP-shaped tool policy (preview)

Show JamJet evaluating policy against an MCP-shaped tool request. This is a
**preview** of the upcoming JamJet Gateway, which will run as an MCP proxy
between Claude Desktop / Cursor / any MCP client and the real MCP servers.

Today the demo evaluates the policy synchronously. The Gateway will run as a
standalone process and apply the same policy across networked MCP traffic.

## Run

```bash
pnpm install
pnpm start
```

## Expected output

```text
Server: postgres-mcp
Tool: postgres/database.delete_all_customers
Policy: block '*delete*'
Decision: BLOCKED
This demo uses an MCP-shaped request envelope to show policy evaluation.
It is not yet an MCP proxy. Full MCP proxy support is planned for JamJet Gateway.
The model is mocked. The enforcement path is real.
```
