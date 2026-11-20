-- A caller's key for a run (POST /executions with run_key): a retried start
-- finds the key and answers with the run it names instead of starting a
-- second one. Claimed inside the start transaction, so two starts with one
-- key make one run. Keys are per tenant. A key moves to a new run only when
-- the start names the run it holds (reuse failed_only); the workflow,
-- version and input hash it was first claimed with stay for good.
CREATE TABLE IF NOT EXISTS run_keys (
    tenant_id        TEXT NOT NULL,
    run_key          TEXT NOT NULL,
    execution_id     TEXT NOT NULL,   -- bare uuid, as workflow_executions stores it
    workflow_id      TEXT NOT NULL,
    workflow_version TEXT NOT NULL,
    input_hash       TEXT NOT NULL,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    PRIMARY KEY (tenant_id, run_key)
);
