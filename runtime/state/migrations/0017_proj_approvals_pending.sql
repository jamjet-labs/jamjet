-- The list of pending approvals across runs (list_pending_approvals):
-- WHERE status = 'pending' ORDER BY updated_at, execution_id, node_id, with
-- a keyset cursor on the same three. Without this each page read the rows
-- of every live run and sorted them in a temporary B-tree.
CREATE INDEX IF NOT EXISTS idx_proj_approvals_pending
    ON proj_approvals (status, updated_at, execution_id, node_id);
