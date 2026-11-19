-- A claim that names an execution (`POST /work-items/claim` with
-- `execution_id`) looks up that execution's items by status; this keeps it
-- from scanning the whole queue.
CREATE INDEX IF NOT EXISTS idx_work_items_execution_status ON work_items(execution_id, status);
