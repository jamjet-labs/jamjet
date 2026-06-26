-- Migration 0004: Lease Fencing
--
-- Adds a monotonic fence token to every work-item lease so a zombie worker
-- (whose lease was stolen after a stall or failover) can never double-commit.
-- lease_fence = store_identity.term * 4294967296 + lease_epoch.

-- ── Store identity (failover generation) ──────────────────────────────────────

CREATE TABLE IF NOT EXISTS store_identity (
    id      INTEGER PRIMARY KEY CHECK (id = 1),  -- single-row table
    term    INTEGER NOT NULL DEFAULT 0           -- bumped on every promotion
);
INSERT OR IGNORE INTO store_identity (id, term) VALUES (1, 0);

-- ── Per-item lease fence ──────────────────────────────────────────────────────

ALTER TABLE work_items ADD COLUMN lease_epoch INTEGER NOT NULL DEFAULT 0;
ALTER TABLE work_items ADD COLUMN lease_fence INTEGER NOT NULL DEFAULT 0;
