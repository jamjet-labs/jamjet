-- The Postgres state store: every table the SQLite store has, with the same
-- columns, except timers and cron_jobs, which nothing reads on Postgres.
--
-- Ids are uuid, times timestamptz, artifacts bytea. JSON is text, so it reads
-- back byte for byte, and a NUL inside a string, which serde writes as
-- \u0000, is kept (jsonb refuses that escape). These columns exist only here,
-- filled by the engine at insert for what SQLite reads out of the JSON:
-- events.kind_type and events.node_id, work_items.by_execution, and
-- work_items.enqueued_at, the claim order on the database clock. Status and
-- role columns carry no CHECK list: the engine owns their values.
--
-- api_tokens.id is text, as on SQLite: an id is the engine's to choose, and
-- rows written by earlier builds keep theirs.

CREATE TABLE tenants (id text PRIMARY KEY, name text NOT NULL, status text NOT NULL DEFAULT 'active',
  policy_json text, limits_json text,
  created_at timestamptz NOT NULL DEFAULT statement_timestamp(), updated_at timestamptz NOT NULL DEFAULT statement_timestamp());
INSERT INTO tenants (id, name, status) VALUES ('default', 'Default', 'active');

CREATE TABLE store_identity (id int PRIMARY KEY CHECK (id = 1), term bigint NOT NULL DEFAULT 0 CHECK (term >= 0));
INSERT INTO store_identity (id, term) VALUES (1, 0);

CREATE TABLE workflow_definitions (tenant_id text NOT NULL REFERENCES tenants(id), workflow_id text NOT NULL,
  version text NOT NULL, ir_json text NOT NULL, created_at timestamptz NOT NULL, PRIMARY KEY (tenant_id, workflow_id, version));

CREATE TABLE workflow_executions (execution_id uuid PRIMARY KEY, tenant_id text NOT NULL DEFAULT 'default' REFERENCES tenants(id),
  workflow_id text NOT NULL, workflow_version text NOT NULL, status text NOT NULL,
  initial_input text NOT NULL, current_state text NOT NULL,
  started_at timestamptz NOT NULL, updated_at timestamptz NOT NULL, completed_at timestamptz,
  parent_execution_id uuid, segment_number int NOT NULL DEFAULT 0);
CREATE INDEX workflow_executions_status_id ON workflow_executions (status, execution_id);
CREATE INDEX workflow_executions_tenant_status_id ON workflow_executions (tenant_id, status, execution_id);
CREATE INDEX workflow_executions_updated_at ON workflow_executions (updated_at);

CREATE TABLE events (id uuid PRIMARY KEY, execution_id uuid NOT NULL REFERENCES workflow_executions(execution_id),
  tenant_id text NOT NULL DEFAULT 'default' REFERENCES tenants(id), sequence bigint NOT NULL CHECK (sequence > 0),
  kind_type text NOT NULL, node_id text, kind_json text NOT NULL, created_at timestamptz NOT NULL,
  UNIQUE (execution_id, sequence));
CREATE INDEX events_node ON events (execution_id, kind_type, node_id);
CREATE INDEX events_tenant ON events (tenant_id, execution_id);

CREATE TABLE snapshots (id uuid PRIMARY KEY, execution_id uuid NOT NULL REFERENCES workflow_executions(execution_id),
  tenant_id text NOT NULL DEFAULT 'default' REFERENCES tenants(id), at_sequence bigint NOT NULL, state_json text NOT NULL,
  created_at timestamptz NOT NULL, status text NOT NULL DEFAULT 'running',
  completed_nodes_json text NOT NULL DEFAULT '{}', active_nodes_json text NOT NULL DEFAULT '[]',
  last_sequence bigint NOT NULL DEFAULT 0, UNIQUE (execution_id, at_sequence));

CREATE TABLE work_items (id uuid PRIMARY KEY, execution_id uuid NOT NULL REFERENCES workflow_executions(execution_id),
  tenant_id text NOT NULL DEFAULT 'default' REFERENCES tenants(id), node_id text NOT NULL, queue_type text NOT NULL,
  payload_json text NOT NULL, by_execution boolean NOT NULL DEFAULT false,
  attempt int NOT NULL DEFAULT 0 CHECK (attempt >= 0), max_attempts int NOT NULL DEFAULT 3,
  status text NOT NULL DEFAULT 'pending', worker_id text, lease_expires_at timestamptz, lease_secs int,
  lease_epoch bigint NOT NULL DEFAULT 0, lease_fence bigint NOT NULL DEFAULT 0, released_fence bigint,
  last_worker_id text, last_lease_fence bigint, retry_after timestamptz,
  created_at timestamptz NOT NULL, enqueued_at timestamptz NOT NULL DEFAULT statement_timestamp(),
  claimed_at timestamptz, completed_at timestamptz);
CREATE INDEX work_items_claim ON work_items (queue_type, enqueued_at, id) WHERE status = 'pending';
CREATE INDEX work_items_execution_status ON work_items (execution_id, status);
CREATE INDEX work_items_node_live ON work_items (execution_id, node_id) WHERE status IN ('pending', 'claimed');
CREATE INDEX work_items_lease ON work_items (lease_expires_at) WHERE status = 'claimed';
CREATE INDEX work_items_tenant ON work_items (tenant_id, status, queue_type);

CREATE TABLE dead_letter_items (id uuid PRIMARY KEY, execution_id uuid NOT NULL REFERENCES workflow_executions(execution_id),
  tenant_id text NOT NULL DEFAULT 'default' REFERENCES tenants(id), node_id text NOT NULL, queue_type text NOT NULL,
  payload_json text NOT NULL, attempt int NOT NULL, last_error text NOT NULL,
  created_at timestamptz NOT NULL, dead_lettered_at timestamptz NOT NULL);

CREATE TABLE api_tokens (id text PRIMARY KEY, token_hash text NOT NULL UNIQUE, name text NOT NULL,
  role text NOT NULL DEFAULT 'developer', tenant_id text NOT NULL DEFAULT 'default' REFERENCES tenants(id),
  created_at timestamptz NOT NULL, expires_at timestamptz, last_used_at timestamptz, revoked_at timestamptz);

CREATE TABLE tool_effects (idempotency_key text PRIMARY KEY, execution_id uuid NOT NULL, node_id text NOT NULL,
  result_json text NOT NULL, tenant_id text NOT NULL DEFAULT 'default', recorded_at timestamptz NOT NULL);

CREATE TABLE tool_reservations (tenant_id text NOT NULL DEFAULT 'default', idempotency_key text NOT NULL,
  execution_id uuid NOT NULL, node_id text NOT NULL, owner text NOT NULL, lease_fence bigint NOT NULL,
  expires_at timestamptz NOT NULL, reserved_at timestamptz NOT NULL, PRIMARY KEY (tenant_id, idempotency_key));

CREATE TABLE proj_approvals (execution_id uuid NOT NULL, node_id text NOT NULL, status text NOT NULL, user_id text,
  comment text, last_sequence bigint NOT NULL, tool_name text, approver text, context_json text,
  tenant_id text NOT NULL DEFAULT 'default', updated_at timestamptz NOT NULL, PRIMARY KEY (execution_id, node_id));

CREATE TABLE projector_checkpoints (projection_name text NOT NULL, execution_id uuid NOT NULL,
  last_sequence bigint NOT NULL, tenant_id text NOT NULL DEFAULT 'default', updated_at timestamptz NOT NULL,
  PRIMARY KEY (projection_name, execution_id));

CREATE TABLE artifacts (tenant_id text NOT NULL DEFAULT 'default', hash text NOT NULL, bytes bytea NOT NULL,
  size bigint NOT NULL, media_type text, created_at timestamptz NOT NULL, PRIMARY KEY (tenant_id, hash));
CREATE INDEX artifacts_hash ON artifacts (hash);
