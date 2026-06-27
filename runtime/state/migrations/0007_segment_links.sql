-- Migration 0007: continue-as-new segment links. A long-running run is a chain
-- of executions; segment N+1 carries the materialized state of segment N and
-- links back via parent_execution_id. segment_number is 0 for an original run.
ALTER TABLE workflow_executions ADD COLUMN parent_execution_id TEXT;
ALTER TABLE workflow_executions ADD COLUMN segment_number INTEGER NOT NULL DEFAULT 0;
