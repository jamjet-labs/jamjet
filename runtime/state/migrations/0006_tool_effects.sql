-- Migration 0006: per-node idempotency cache. A node fire records its result
-- keyed by content_hash({run, segment, step, node, input_hash}); a replay/reclaim
-- reads it back and skips re-firing the side effect. Including `node` in the key
-- prevents cross-node key collisions when sibling nodes share the same step count.
CREATE TABLE IF NOT EXISTS tool_effects (
    idempotency_key TEXT PRIMARY KEY,
    execution_id    TEXT NOT NULL,
    node_id         TEXT NOT NULL,
    result_json     TEXT NOT NULL,
    tenant_id       TEXT NOT NULL DEFAULT 'default',
    recorded_at     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tool_effects_execution ON tool_effects(execution_id);
