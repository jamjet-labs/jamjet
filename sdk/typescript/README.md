# JamJet TypeScript SDK

Monorepo for JamJet's TypeScript SDK packages.

| Package | npm | Status |
|---|---|---|
| `@jamjet/cloud` | [0.2.2](https://www.npmjs.com/package/@jamjet/cloud) | Plan 1 + 2 + safety-demo wedge |
| `@jamjet/cloud-vercel` | [0.1.0](https://www.npmjs.com/package/@jamjet/cloud-vercel) | Plan 3 — Vercel AI SDK adapter |

Internal design specs live in `docs/superpowers/` (not committed). Public docs are at [jamjet.dev](https://jamjet.dev).

## Examples

Four zero-setup safety demos under `examples/`:

- `01-block-unsafe-tool` — block a destructive tool call before execution
- `02-human-approval` — pause for human approval on a risky action
- `03-budget-cap` — stop a runaway agent loop at a hard dollar cap
- `04-mcp-tool-policy` — evaluate policy against an MCP-shaped envelope (Gateway preview)

```bash
pnpm install
pnpm --filter @jamjet/example-01-block-unsafe-tool start
```

## Dev setup

```bash
pnpm install
pnpm build
pnpm test
```
