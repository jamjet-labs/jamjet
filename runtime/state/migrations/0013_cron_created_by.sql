-- Cron jobs belong to a tenant, and record who created them.
--
-- `created_by`: the principal (`token:<id>`) that created or last replaced the
-- job. Runs the job starts record it as their initiator, so the person who
-- chose the workflow, the input and the schedule cannot approve the run
-- (separation of duties). NULL for jobs created before this column existed and
-- for dev-mode jobs: their runs have no initiator, and an approval on such a
-- run is refused outside dev mode.
--
-- Tenancy: `tenant_id` has existed since 0003, but `name` was UNIQUE across all
-- tenants, so a job was addressable by name from any tenant and replacing one
-- by name crossed tenants. The table is rebuilt with UNIQUE (tenant_id, name).
-- Rows keep their tenant; rows from before 0003 already read `default`.
CREATE TABLE cron_jobs_new (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    cron_expression  TEXT NOT NULL,
    workflow_id      TEXT NOT NULL,
    workflow_version TEXT NOT NULL DEFAULT '1.0.0',
    input_json       TEXT NOT NULL DEFAULT '{}',
    enabled          INTEGER NOT NULL DEFAULT 1,
    last_run_at      TEXT,
    next_run_at      TEXT NOT NULL,
    created_at       TEXT NOT NULL,
    tenant_id        TEXT NOT NULL DEFAULT 'default' REFERENCES tenants(id),
    created_by       TEXT,
    UNIQUE (tenant_id, name)
);
INSERT INTO cron_jobs_new
    (id, name, cron_expression, workflow_id, workflow_version, input_json, enabled,
     last_run_at, next_run_at, created_at, tenant_id)
    SELECT id, name, cron_expression, workflow_id, workflow_version, input_json, enabled,
           last_run_at, next_run_at, created_at, tenant_id
    FROM cron_jobs;
DROP TABLE cron_jobs;
ALTER TABLE cron_jobs_new RENAME TO cron_jobs;
CREATE INDEX IF NOT EXISTS idx_cron_next_run ON cron_jobs(next_run_at) WHERE enabled = 1;
CREATE INDEX IF NOT EXISTS idx_cron_jobs_tenant ON cron_jobs (tenant_id);
