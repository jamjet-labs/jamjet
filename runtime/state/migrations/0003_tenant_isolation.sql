-- Migration 0003: Tenant Isolation
--
-- Adds multi-tenant data partitioning. All existing data is assigned to the
-- "default" tenant for backward compatibility.

-- ── Tenants table ─────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS tenants (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'active',
    policy_json     TEXT,
    limits_json     TEXT,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Seed the default tenant.
INSERT OR IGNORE INTO tenants (id, name, status) VALUES ('default', 'Default', 'active');

-- ── Add tenant_id to existing tables ──────────────────────────────────────────

-- workflow_definitions needs PK expanded to include tenant_id for proper isolation.
-- SQLite cannot alter PKs, so we recreate the table.
CREATE TABLE workflow_definitions_new (
    workflow_id     TEXT NOT NULL,
    version         TEXT NOT NULL,
    ir_json         TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    tenant_id       TEXT NOT NULL DEFAULT 'default' REFERENCES tenants(id),
    PRIMARY KEY (tenant_id, workflow_id, version)
);
INSERT INTO workflow_definitions_new (workflow_id, version, ir_json, created_at, tenant_id)
    SELECT workflow_id, version, ir_json, created_at, 'default' FROM workflow_definitions;
DROP TABLE workflow_definitions;
ALTER TABLE workflow_definitions_new RENAME TO workflow_definitions;
ALTER TABLE workflow_executions  ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants(id);
ALTER TABLE agents               ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants(id);
ALTER TABLE api_tokens           ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants(id);
ALTER TABLE work_items           ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants(id);
ALTER TABLE dead_letter_items    ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants(id);
ALTER TABLE events               ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants(id);
ALTER TABLE snapshots            ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants(id);
ALTER TABLE timers               ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants(id);
ALTER TABLE cron_jobs            ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants(id);

-- ── Composite indices for tenant-scoped queries ───────────────────────────────

CREATE INDEX IF NOT EXISTS idx_wf_defs_tenant     ON workflow_definitions (tenant_id, workflow_id, version);
CREATE INDEX IF NOT EXISTS idx_executions_tenant   ON workflow_executions  (tenant_id, status);
CREATE INDEX IF NOT EXISTS idx_agents_tenant       ON agents               (tenant_id, status);
CREATE INDEX IF NOT EXISTS idx_tokens_tenant       ON api_tokens           (tenant_id);
CREATE INDEX IF NOT EXISTS idx_work_items_tenant   ON work_items           (tenant_id, status, queue_type);
CREATE INDEX IF NOT EXISTS idx_events_tenant       ON events               (tenant_id, execution_id);
CREATE INDEX IF NOT EXISTS idx_snapshots_tenant    ON snapshots            (tenant_id, execution_id);
CREATE INDEX IF NOT EXISTS idx_timers_tenant       ON timers               (tenant_id);
CREATE INDEX IF NOT EXISTS idx_cron_jobs_tenant    ON cron_jobs            (tenant_id);
