-- Migration 0008: async read-model projector. proj_approvals is the projected
-- read side for the approvals endpoint; projector_checkpoints records how far each
-- named projection has consumed each execution's event stream (resume after restart).
CREATE TABLE IF NOT EXISTS proj_approvals (
    execution_id  TEXT    NOT NULL,
    node_id       TEXT    NOT NULL,
    status        TEXT    NOT NULL,
    user_id       TEXT,
    comment       TEXT,
    last_sequence INTEGER NOT NULL,
    tenant_id     TEXT    NOT NULL DEFAULT 'default',
    updated_at    TEXT    NOT NULL,
    PRIMARY KEY (execution_id, node_id)
);
CREATE INDEX IF NOT EXISTS idx_proj_approvals_exec ON proj_approvals(execution_id);
CREATE TABLE IF NOT EXISTS projector_checkpoints (
    projection_name TEXT    NOT NULL,
    execution_id    TEXT    NOT NULL,
    last_sequence   INTEGER NOT NULL,
    tenant_id       TEXT    NOT NULL DEFAULT 'default',
    updated_at      TEXT    NOT NULL,
    PRIMARY KEY (projection_name, execution_id)
);
