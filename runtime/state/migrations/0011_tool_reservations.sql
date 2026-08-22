-- Reserve-before-fire (spec Move 4b).
--
-- `tool_effects` records a result AFTER a node commits, so the replay guard can
-- only answer "has this already run to completion". Between two live work items
-- for the same node, both read "no" and both fire — the effect happens twice.
-- The spec called the reservation load-bearing, not optional.
--
-- A reservation is a claim on an idempotency key taken BEFORE the executor runs.
-- It is deliberately its own table rather than a nullable `result_json` on
-- tool_effects: that column is NOT NULL, relaxing it in SQLite means rebuilding
-- the table, and the two rows answer different questions ("someone is running
-- this" vs "this produced that").
--
-- `expires_at` is what stops the reservation becoming a new permanent wedge. A
-- worker that dies mid-tool leaves its row behind; without expiry the key would
-- be unrunnable forever, which is worse than the double-fire it replaced.
-- Keyed by (tenant_id, idempotency_key), NOT by the key alone. A global primary
-- key lets one tenant's reservation collide with another's: the scoped upsert
-- conflicts on a row it cannot see, affects zero rows, and its tenant-filtered
-- lookup then finds nothing — which reads as "free" and hands out a key someone
-- else holds. Tenant is part of the identity of a reservation, as it is for
-- every other row here.
CREATE TABLE IF NOT EXISTS tool_reservations (
    idempotency_key TEXT NOT NULL,
    execution_id    TEXT NOT NULL,
    node_id         TEXT NOT NULL,
    -- Diagnostics: which worker holds it, and under which lease.
    owner           TEXT NOT NULL,
    lease_fence     INTEGER NOT NULL,
    expires_at      TEXT NOT NULL,
    tenant_id       TEXT NOT NULL DEFAULT 'default',
    reserved_at     TEXT NOT NULL,
    PRIMARY KEY (tenant_id, idempotency_key)
);

CREATE INDEX IF NOT EXISTS idx_tool_reservations_execution ON tool_reservations(execution_id);
CREATE INDEX IF NOT EXISTS idx_tool_reservations_expiry ON tool_reservations(expires_at);
