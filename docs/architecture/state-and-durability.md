# State & Durability

JamJet provides durable execution — workflows survive process crashes, machine restarts, and network failures without losing completed work.

---

## Design: Event Sourcing + Snapshots

The state model uses **append-only event sourcing** with **periodic snapshots**:

```
Event Log (append-only):
  workflow_started  →  node_scheduled  →  node_started  →  node_completed
  →  node_scheduled  →  node_started  →  interrupt_raised
  →  approval_received  →  node_completed  →  workflow_completed

Snapshot (every N events):
  { workflow_id, state_at_event_N, materialized_workflow_state }

Current State = latest_snapshot + events_since_snapshot
```

This gives:
- **Full audit trail** — every event is preserved forever
- **Fast recovery** — restore from snapshot + short delta, not full replay
- **Time-travel debugging** — replay from any event in history

---

## Event Types

| Event | Description |
|-------|-------------|
| `workflow_started` | Workflow execution created and accepted |
| `node_scheduled` | Node queued for execution |
| `node_started` | Worker acquired lease, execution began |
| `node_completed` | Node finished; output patch applied to state |
| `node_failed` | Node execution failed |
| `retry_scheduled` | Retry timer created after node failure |
| `interrupt_raised` | Workflow paused (human approval, wait, external) |
| `approval_received` | Human submitted decision on an approval node |
| `timer_created` | Durable timer registered |
| `timer_fired` | Timer elapsed, workflow woke |
| `external_event_received` | External signal resumed a waiting workflow |
| `child_workflow_started` | Sub-workflow spawned |
| `child_workflow_completed` | Sub-workflow finished; result merged into parent state |
| `workflow_completed` | All terminal nodes reached; final state written |
| `workflow_cancelled` | Explicit cancellation received |

All events are written **transactionally** per state transition. A write either fully succeeds or is fully rolled back — no partial states.

---

## Storage Backends

The `StateBackend` trait abstracts storage. Both backends implement the same interface.

### SQLite (local dev)

Used by `jamjet dev`. Auto-initialized in the project directory. No external services required.

```
.jamjet/
  state.db       # SQLite database
  snapshots/     # snapshot files
```

### Postgres (production)

Used in production. Schema managed via `sqlx migrate`.

Key tables:
```sql
-- Event log (append-only, never updated or deleted)
CREATE TABLE events (
  id          BIGSERIAL PRIMARY KEY,
  workflow_id UUID NOT NULL,
  sequence    BIGINT NOT NULL,
  event_type  TEXT NOT NULL,
  payload     JSONB NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (workflow_id, sequence)
);

-- Snapshots (periodically written, never deleted until compaction)
CREATE TABLE snapshots (
  id           BIGSERIAL PRIMARY KEY,
  workflow_id  UUID NOT NULL,
  at_sequence  BIGINT NOT NULL,
  state        JSONB NOT NULL,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Workflow index (current status, for list/filter queries)
CREATE TABLE workflows (
  id          UUID PRIMARY KEY,
  status      TEXT NOT NULL,
  definition  JSONB NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

---

## Crash Recovery

When the runtime restarts after a crash:

1. Scheduler queries for all `running` workflows
2. For each: load latest snapshot + delta events → materialize current state
3. Re-detect runnable nodes (any node whose deps are `completed` but it has no `node_started` event yet, or whose lease expired)
4. Re-dispatch runnable nodes to queues

Worker leases ensure a node that was `started` but whose worker died gets re-queued once the lease expires. The executing node sees a fresh start — idempotency is the tool author's responsibility (documented clearly).

---

## Snapshotting Policy

Snapshots are taken:
- Every 50 events by default (configurable)
- On workflow pause (interrupt raised)
- On explicit checkpoint request

Old snapshots are retained for replay and debugging. A compaction job (future) can prune snapshots older than a configurable retention window while keeping the event log intact.

---

## Replay and Time-Travel

```bash
# Replay from the beginning
jamjet replay exec_abc123

# Replay from a specific node
jamjet replay exec_abc123 --from-node summarize

# Inspect state at a point in time
jamjet inspect exec_abc123 --at-event 42
```

Replay uses the same executor but routes through a **deterministic replay adapter** — model calls and tool calls are replaced by their recorded outputs. This makes replay fully deterministic and instant.
