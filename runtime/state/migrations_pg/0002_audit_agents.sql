-- The audit log, its outbox and the agent registry on Postgres.
--
-- audit_log is one hash chain for every engine on the database. audit_head
-- names its newest entry; an append locks that row first, so appends take
-- turns and each links onto the entry before it. The constraints hold the
-- chain even for a writer that skips the lock: one entry per prev_hash, one
-- genesis (the only entry with no prev_hash), one entry per event. position
-- breaks a tie between two entries of one instant.
--
-- audit_outbox holds a row for each sealed event that has no audit entry
-- yet. The entry's append deletes it in the same transaction.

CREATE TABLE audit_log (
  position bigint GENERATED ALWAYS AS IDENTITY, id uuid PRIMARY KEY, event_id uuid NOT NULL,
  execution_id text NOT NULL, sequence bigint NOT NULL, event_type text NOT NULL,
  actor_id text NOT NULL, actor_type text NOT NULL, tool_call_hash text, policy_decision text,
  http_request_id text, http_method text, http_path text, ip_address text,
  created_at timestamptz NOT NULL, raw_event text NOT NULL, tenant_id text NOT NULL DEFAULT 'default',
  expires_at timestamptz, redacted boolean NOT NULL DEFAULT false,
  prev_hash text, entry_hash text NOT NULL, signature text,
  CONSTRAINT audit_log_one_successor UNIQUE (prev_hash),
  CONSTRAINT audit_log_one_entry_per_event UNIQUE (event_id));
CREATE UNIQUE INDEX audit_log_one_genesis ON audit_log ((true)) WHERE prev_hash IS NULL;
CREATE INDEX audit_log_execution ON audit_log (execution_id);
CREATE INDEX audit_log_created ON audit_log (created_at, position);
CREATE INDEX audit_log_tenant ON audit_log (tenant_id);
CREATE TABLE audit_head (id int PRIMARY KEY CHECK (id = 1), entry_hash text, created_at timestamptz);
INSERT INTO audit_head (id) VALUES (1);
CREATE TABLE audit_outbox (event_id uuid PRIMARY KEY REFERENCES events(id), execution_id uuid NOT NULL,
  sequence bigint NOT NULL, tenant_id text NOT NULL, kind_type text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT statement_timestamp(),
  attempts int NOT NULL DEFAULT 0, last_error text, gap_recorded boolean NOT NULL DEFAULT false);
CREATE INDEX audit_outbox_created ON audit_outbox (created_at);
CREATE TABLE agents (id uuid PRIMARY KEY, uri text NOT NULL UNIQUE, card_json text NOT NULL,
  status text NOT NULL DEFAULT 'registered', registered_at timestamptz NOT NULL, updated_at timestamptz NOT NULL,
  last_heartbeat timestamptz, tenant_id text NOT NULL DEFAULT 'default' REFERENCES tenants(id));
