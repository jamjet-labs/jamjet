-- The scheduler's keyset walk over running executions
-- (list_running_executions_after): WHERE status = 'running' AND
-- execution_id > ? ORDER BY execution_id. Without these the plan sorted the
-- matching rows in a temporary B-tree on every page of every tick.
CREATE INDEX IF NOT EXISTS idx_executions_status_id
    ON workflow_executions (status, execution_id);
CREATE INDEX IF NOT EXISTS idx_executions_tenant_status_id
    ON workflow_executions (tenant_id, status, execution_id);
