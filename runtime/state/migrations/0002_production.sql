-- Phase 2: production runtime additions

-- Add max_attempts to work_items (default 3 retries)
ALTER TABLE work_items ADD COLUMN max_attempts INTEGER NOT NULL DEFAULT 3;
-- Add retry_after for delayed re-queue (backoff scheduling)
ALTER TABLE work_items ADD COLUMN retry_after TEXT; -- RFC3339, nullable

-- ── Dead-letter queue ─────────────────────────────────────────────────────────
-- Items that have exhausted all retry attempts land here permanently.

CREATE TABLE IF NOT EXISTS dead_letter_items (
    id              TEXT    PRIMARY KEY,  -- UUID (same as original work_item id)
    execution_id    TEXT    NOT NULL REFERENCES workflow_executions(execution_id),
    node_id         TEXT    NOT NULL,
    queue_type      TEXT    NOT NULL,
    payload_json    TEXT    NOT NULL,
    attempt         INTEGER NOT NULL,
    last_error      TEXT    NOT NULL,
    created_at      TEXT    NOT NULL,    -- original work item created_at
    dead_lettered_at TEXT   NOT NULL     -- RFC3339, when moved to DLQ
);

CREATE INDEX IF NOT EXISTS idx_dlq_execution ON dead_letter_items(execution_id);
CREATE INDEX IF NOT EXISTS idx_dlq_dead_lettered_at ON dead_letter_items(dead_lettered_at);

-- ── API token authentication (G2.1) ──────────────────────────────────────────

CREATE TABLE IF NOT EXISTS api_tokens (
    id          TEXT PRIMARY KEY,   -- UUID
    token_hash  TEXT NOT NULL UNIQUE, -- SHA-256 hex of the raw token
    name        TEXT NOT NULL,
    role        TEXT NOT NULL DEFAULT 'developer', -- operator|developer|reviewer|viewer
    created_at  TEXT NOT NULL,
    expires_at  TEXT,               -- RFC3339, nullable (null = never expires)
    last_used_at TEXT,
    revoked_at  TEXT                -- RFC3339, nullable (null = active)
);

CREATE INDEX IF NOT EXISTS idx_api_tokens_hash ON api_tokens(token_hash) WHERE revoked_at IS NULL;

-- ── Durable timers (A2.4) ─────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS timers (
    id              TEXT PRIMARY KEY,
    execution_id    TEXT NOT NULL,
    node_id         TEXT NOT NULL,
    fire_at         TEXT NOT NULL,       -- RFC3339
    correlation_key TEXT,
    fired           INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL,
    fired_at        TEXT                 -- RFC3339, set when fired
);

CREATE INDEX IF NOT EXISTS idx_timers_fire_at ON timers(fire_at) WHERE fired = 0;
CREATE INDEX IF NOT EXISTS idx_timers_execution ON timers(execution_id);

-- ── Cron jobs (A2.5) ──────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS cron_jobs (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL UNIQUE,
    cron_expression  TEXT NOT NULL,
    workflow_id      TEXT NOT NULL,
    workflow_version TEXT NOT NULL DEFAULT '1.0.0',
    input_json       TEXT NOT NULL DEFAULT '{}',
    enabled          INTEGER NOT NULL DEFAULT 1,
    last_run_at      TEXT,
    next_run_at      TEXT NOT NULL,     -- RFC3339, pre-computed
    created_at       TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_cron_next_run ON cron_jobs(next_run_at) WHERE enabled = 1;
