# RFC-007: A2A Integration

| Field | Value |
|-------|-------|
| RFC | 007 |
| Status | Draft |
| Created | 2026-03-07 |

---

## Summary

Defines JamJet's implementation of the A2A (Agent-to-Agent) protocol as both client and server, enabling cross-framework agent delegation and discovery.

---

## Key Design Points

### A2A Client
- Fetches Agent Card from `/.well-known/agent.json`
- Submits tasks via `tasks/send`, streams via `tasks/sendSubscribe` (SSE)
- Handles full task lifecycle: submitted → working → input-required → completed/failed
- `input-required` state pauses the JamJet workflow and routes to HITL or supervisor
- A2A task state durably tracked — crash safe, no duplicate submissions

### A2A Server
- Publishes Agent Card at `/.well-known/agent.json`
- Accepts `tasks/send`, `tasks/sendSubscribe`, `tasks/get`, `tasks/cancel`
- Supports multi-turn via `input-required`
- Streams progress via SSE
- Push notifications to registered webhooks

### Task Lifecycle Mapping

| A2A State | JamJet Event |
|-----------|-------------|
| submitted | node_scheduled |
| working | node_started |
| input-required | interrupt_raised |
| completed | node_completed |
| failed | node_failed |
| canceled | node_cancelled |

### Auth
- Bearer token (v1)
- OAuth2 client credentials (v1)
- mTLS (Phase 4 / cross-org)

---

## Implementation Plan
See progress-tracker.md tasks D.2.1–D.2.18 (Phase 2).
