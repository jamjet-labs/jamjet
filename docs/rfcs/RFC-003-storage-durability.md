# RFC-003: Storage and Durability Model

| Field | Value |
|-------|-------|
| RFC | 003 |
| Status | Draft |
| Created | 2026-03-07 |

---

## Summary

Defines the event sourcing model, snapshot strategy, storage backend abstraction, and crash recovery guarantees.

---

## Key Design Points

### Event Sourcing
- Append-only event log is the source of truth for all workflow state
- Every state transition appends one or more events atomically
- Events are never updated or deleted (except via explicit compaction after retention)
- The event log enables full audit, replay, and time-travel debugging

### Snapshotting
- Snapshots taken every 50 events (configurable) and on workflow pause
- Current state = latest snapshot + delta events since that snapshot
- Snapshots are stored alongside the event log, never replacing events

### Storage Backends
- `StateBackend` trait abstracts all storage operations
- SQLite for local dev (`jamjet dev`), Postgres for production
- Future: FoundationDB or cloud-native backends

### Crash Recovery
On restart: load snapshot + replay delta → re-detect runnable nodes → re-dispatch. Worker lease expiry prevents duplicate execution.

### Delivery Semantics
- State transitions: transactional and durable (exactly once via DB transactions)
- Worker task delivery: at-least-once (idempotency is the tool author's responsibility)
- Side-effect safety: document the idempotency key pattern in guides

---

## Implementation Plan
See progress-tracker.md tasks A.3.1–A.3.7.
