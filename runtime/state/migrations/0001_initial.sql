-- JamJet state backend initial schema
-- Supports both SQLite (local dev) and Postgres (production).
-- For SQLite: AUTOINCREMENT, TEXT for UUIDs/timestamps/JSON
-- For Postgres: SERIAL, UUID type, JSONB

-- ── Agent registry ──────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS agents (
    id              TEXT        PRIMARY KEY,  -- UUID
    uri             TEXT        NOT NULL UNIQUE,
    card_json       TEXT        NOT NULL,     -- serialized AgentCard JSON
    status          TEXT        NOT NULL DEFAULT 'registered',
    registered_at   TEXT        NOT NULL,     -- RFC3339
    updated_at      TEXT        NOT NULL,     -- RFC3339
    last_heartbeat  TEXT                      -- RFC3339, nullable
);

CREATE INDEX IF NOT EXISTS idx_agents_status ON agents(status);
CREATE INDEX IF NOT EXISTS idx_agents_uri ON agents(uri);

-- ── Workflow definitions (IR registry) ──────────────────────────────────────

CREATE TABLE IF NOT EXISTS workflow_definitions (
    workflow_id     TEXT        NOT NULL,
    version         TEXT        NOT NULL,
    ir_json         TEXT        NOT NULL,     -- serialized WorkflowIr JSON
    created_at      TEXT        NOT NULL,     -- RFC3339
    PRIMARY KEY (workflow_id, version)
);

-- ── Workflow executions ──────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS workflow_executions (
    execution_id    TEXT        PRIMARY KEY,  -- UUID as text
    workflow_id     TEXT        NOT NULL,
    workflow_version TEXT       NOT NULL,
    status          TEXT        NOT NULL,     -- pending|running|paused|completed|failed|cancelled
    initial_input   TEXT        NOT NULL,     -- JSON
    current_state   TEXT        NOT NULL,     -- JSON
    started_at      TEXT        NOT NULL,     -- RFC3339
    updated_at      TEXT        NOT NULL,     -- RFC3339
    completed_at    TEXT                      -- RFC3339, nullable
);

CREATE INDEX IF NOT EXISTS idx_executions_status ON workflow_executions(status);
CREATE INDEX IF NOT EXISTS idx_executions_updated_at ON workflow_executions(updated_at);

-- ── Event log ────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS events (
    id              TEXT        PRIMARY KEY,  -- UUID
    execution_id    TEXT        NOT NULL REFERENCES workflow_executions(execution_id),
    sequence        INTEGER     NOT NULL,
    kind_json       TEXT        NOT NULL,     -- serialized EventKind JSON
    created_at      TEXT        NOT NULL,     -- RFC3339
    UNIQUE(execution_id, sequence)
);

CREATE INDEX IF NOT EXISTS idx_events_execution_seq ON events(execution_id, sequence);

-- ── Snapshots ────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS snapshots (
    id              TEXT        PRIMARY KEY,  -- UUID
    execution_id    TEXT        NOT NULL REFERENCES workflow_executions(execution_id),
    at_sequence     INTEGER     NOT NULL,
    state_json      TEXT        NOT NULL,     -- serialized workflow state JSON
    created_at      TEXT        NOT NULL,     -- RFC3339
    UNIQUE(execution_id, at_sequence)
);

CREATE INDEX IF NOT EXISTS idx_snapshots_execution ON snapshots(execution_id, at_sequence DESC);

-- ── Work item queue ──────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS work_items (
    id                  TEXT    PRIMARY KEY,  -- UUID
    execution_id        TEXT    NOT NULL REFERENCES workflow_executions(execution_id),
    node_id             TEXT    NOT NULL,
    queue_type          TEXT    NOT NULL,
    payload_json        TEXT    NOT NULL,     -- JSON
    attempt             INTEGER NOT NULL DEFAULT 0,
    status              TEXT    NOT NULL DEFAULT 'pending', -- pending|claimed|completed|failed
    worker_id           TEXT,
    lease_expires_at    TEXT,                 -- RFC3339, nullable
    created_at          TEXT    NOT NULL,     -- RFC3339
    claimed_at          TEXT,                 -- RFC3339, nullable
    completed_at        TEXT                  -- RFC3339, nullable
);

CREATE INDEX IF NOT EXISTS idx_work_items_status_queue ON work_items(status, queue_type);
CREATE INDEX IF NOT EXISTS idx_work_items_execution ON work_items(execution_id);
CREATE INDEX IF NOT EXISTS idx_work_items_lease ON work_items(lease_expires_at) WHERE status = 'claimed';
