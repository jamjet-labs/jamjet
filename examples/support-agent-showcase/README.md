# JamJet Cost Intelligence: Support Agent Showcase

A support agent built on `@jamjet/cloud` that demonstrates JamJet's cost intelligence end-to-end. The demo DETECTS token waste (the same large system prompt sent repeatedly as regular input tokens) and PREVENTS it (cache_inject adds `cache_control` so subsequent calls read the prefix from Anthropic's prompt cache instead of paying for it again). PII redaction, a per-session budget cap, a human-approval gate for refunds, and an audit trail ride along to show the full governance picture in one place.

## Prerequisites

- Node 20 or later (tested on Node 26)
- npm

## Run it (mock mode, no keys needed)

```bash
npm install
npm run dev
```

Open http://localhost:3000 and click the presets in order:

1. **Ask 5 KB questions**: sends five knowledge-base questions with the same large system prompt. After the second call the Waste Detection panel lights up, showing how many tokens you are re-paying for and the cost in cents.
2. **Enable cache_inject → re-run**: flips `cache_inject` on the session, then re-runs a question. The Cache Savings panel updates with the cache-read tokens and the cents saved.
3. **Rapid-fire (trip budget)**: fires eight questions quickly. Once the session's $0.05 budget is exhausted, the spend bar hits the cap and further turns are blocked.
4. **Send PII**: sends a message containing a social security number. The governance strip shows a redaction event; the number never reaches the model.
5. **Request refund**: sends a refund request. The governance strip shows an approval card. Click **Approve** and an audit entry appears, written by `AuditWriter` from `@jamjet/cloud/node`.

## The cost story

Every API call includes a system prompt (the full knowledge base, ~1 750 tokens). Without caching, each call re-pays for those tokens at the full input rate. After five identical-prefix calls the Waste Detection panel shows the repeated spend.

`cache_inject` adds Anthropic's `cache_control: { type: "ephemeral" }` breakpoint to the system prompt block. Subsequent calls serve those tokens from the prompt cache at the cache-read rate (10x cheaper for Sonnet). The savings panel tracks the delta.

The demo computes the same SHA-256 prefix hash the Cloud SDK uses to group repeated prefixes, so what you see in Waste Detection is the same signal that `@jamjet/cloud` acts on in production.

## Real numbers and dashboard (optional)

Set environment variables before `npm run dev`:

| Variable | Effect |
|---|---|
| `ANTHROPIC_API_KEY` | Switches from mock model to real Anthropic API; cache savings become real Anthropic cache-read savings |
| `JAMJET_API_KEY` | Streams trace events to app.jamjet.dev so you can see costs and governance in the live dashboard |

## How it works

| Capability | File / API |
|---|---|
| Waste detection | `lib/engine/prefix-hash.ts` + `lib/engine/waste.ts` |
| Cache prevention | `applyCacheInject` (`@jamjet/cloud`) + `lib/engine/savings.ts` |
| Budget cap | `lib/session.ts` |
| PII redaction | `redact` (`@jamjet/cloud`) |
| Human approval gate | `requireApproval` pattern in `lib/engine/run-turn.ts` (live mode) |
| Audit trail | `AuditWriter` (`@jamjet/cloud/node`) in `lib/engine/refund.ts` |
| Cost accounting | `estimateCost` (`@jamjet/cloud`) in `lib/engine/run-turn.ts` |

Requires `@jamjet/cloud@0.4.0-alpha.2` (already pinned in `package.json`).
