-- Migration 0009: add pending-approval metadata columns to proj_approvals.
-- These fields are populated from ToolApprovalRequired events so that the
-- GET /executions/:id/approvals endpoint can serve the full pending-approval
-- shape (tool_name, approver, context) without reading the event log.
-- Cleared to NULL when a node transitions to "approved" or "rejected".
ALTER TABLE proj_approvals ADD COLUMN tool_name    TEXT;
ALTER TABLE proj_approvals ADD COLUMN approver     TEXT;
ALTER TABLE proj_approvals ADD COLUMN context_json TEXT;
