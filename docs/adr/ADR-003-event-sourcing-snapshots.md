# ADR-003: Event Sourcing with Periodic Snapshots

| Field | Value |
|-------|-------|
| Status | Accepted |
| Date | 2026-03-07 |

---

## Context

How should workflow state be stored to support durability, audit, replay, and efficient reads?

## Decision

**Use append-only event sourcing with periodic snapshots.** The event log is the source of truth. Snapshots are taken periodically to avoid full log replay on recovery.

## Rationale

- **Full audit trail** — every state transition is a permanent, immutable record
- **Replay** — time-travel debugging and workflow replay are natural consequences of the model
- **Crash recovery** — simply re-apply events from the last snapshot on restart
- **Debuggability** — developers can inspect every state change that led to a bug
- **Snapshots** keep recovery fast without sacrificing the audit trail

## Alternatives Considered

### Mutable state only (UPDATE rows in place)
Pros: simpler reads.
Cons: no audit trail, no replay, crash recovery requires careful transaction design.

### Pure event sourcing (no snapshots)
Pros: simplest model.
Cons: recovery requires replaying potentially thousands of events; unacceptable latency for long-running workflows.

## Consequences

- Storage grows over time — mitigated by configurable snapshot compaction
- Event schema must be versioned carefully — events are permanent
- Reads require snapshot + delta materialization — small overhead, acceptable
