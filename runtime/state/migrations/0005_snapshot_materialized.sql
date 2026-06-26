-- Migration 0005: snapshots carry the full materialized state, so a per-turn
-- snapshot is a complete resume base (status + completed/active nodes), not just
-- current_state. Without these, materialize() would lose pre-snapshot node state.
ALTER TABLE snapshots ADD COLUMN status               TEXT    NOT NULL DEFAULT 'running';
ALTER TABLE snapshots ADD COLUMN completed_nodes_json TEXT    NOT NULL DEFAULT '{}';
ALTER TABLE snapshots ADD COLUMN active_nodes_json    TEXT    NOT NULL DEFAULT '[]';
ALTER TABLE snapshots ADD COLUMN last_sequence        INTEGER NOT NULL DEFAULT 0;
