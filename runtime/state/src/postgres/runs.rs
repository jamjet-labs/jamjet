//! Runs, their event logs and snapshots, and the scheduler's pass, on
//! Postgres.
//!
//! A transaction that appends to a run's log or moves its status locks the
//! run's row first ([`lock_run`]). Writers of one run take turns there;
//! writers of other runs do not wait. Under that lock the events take the
//! next sequences in the statement that inserts them ([`insert_events`]), so
//! two writers never take one sequence. The unique key on
//! `(execution_id, sequence)` is the backstop: a violation would be a
//! locking bug, and is never retried.
//!
//! The lock is `FOR UPDATE`, not `FOR NO KEY UPDATE`. The foreign key check
//! of a row inserted for the run (an event, an item) takes a share lock on
//! the run's row, which `FOR UPDATE` conflicts with in both directions. So a
//! write into the run that took no run lock still lands wholly before or
//! wholly after a locked one, and the pass's check for live items cannot
//! miss an item whose insert is still in flight.
//!
//! Rows written into a run carry the run's own tenant, read with the lock.
//! A tenant's view finds only its own runs: a write that names another
//! tenant's run finds no row to lock and is `NotFound`, as for a run nobody
//! holds, and writes nothing. Reads through a tenant's view filter on each
//! table's own `tenant_id`, as the scoped SQLite store does.

use super::tx::{no_nul, statement_error, with_tx, TxError};
use super::PgBackend;
use crate::backend::{
    same_definition, version_conflict, BackendResult, ClaimedWorkItem, ConditionalAppend,
    EventCondition, ExecutionFinish, PassCommit, RunningPage, SchedulerPass, StateBackendError,
    WorkItem, WorkflowDefinition,
};
use crate::event::{Event, EventKind, EventSequence};
use crate::snapshot::Snapshot;
use crate::tenant::{TenantId, DEFAULT_TENANT};
use chrono::{DateTime, Utc};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

// ── What the bodies share ────────────────────────────────────────────────────

pub(super) fn status_str(s: &WorkflowStatus) -> &'static str {
    match s {
        WorkflowStatus::Pending => "pending",
        WorkflowStatus::Running => "running",
        WorkflowStatus::Paused => "paused",
        WorkflowStatus::Completed => "completed",
        WorkflowStatus::Failed => "failed",
        WorkflowStatus::Cancelled => "cancelled",
        WorkflowStatus::LimitExceeded => "limit_exceeded",
    }
}

pub(super) fn parse_status(s: &str) -> BackendResult<WorkflowStatus> {
    Ok(match s {
        "pending" => WorkflowStatus::Pending,
        "running" => WorkflowStatus::Running,
        "paused" => WorkflowStatus::Paused,
        "completed" => WorkflowStatus::Completed,
        "failed" => WorkflowStatus::Failed,
        "cancelled" => WorkflowStatus::Cancelled,
        "limit_exceeded" => WorkflowStatus::LimitExceeded,
        other => {
            return Err(StateBackendError::Database(format!(
                "unknown status: {other}"
            )))
        }
    })
}

/// A column that does not read as the type asked for.
pub(super) fn decode(e: sqlx::Error) -> StateBackendError {
    StateBackendError::Database(e.to_string())
}

pub(super) fn to_i32(what: &str, n: u32) -> BackendResult<i32> {
    i32::try_from(n).map_err(|_| StateBackendError::Database(format!("{what} {n} is too large")))
}

/// A run's row. Its JSON columns must parse: a run whose input is not JSON
/// does not read back.
pub(super) fn row_to_execution(row: &PgRow) -> BackendResult<WorkflowExecution> {
    let initial_input: String = row.try_get("initial_input").map_err(decode)?;
    let current_state: String = row.try_get("current_state").map_err(decode)?;
    let segment_number: i32 = row.try_get("segment_number").map_err(decode)?;
    Ok(WorkflowExecution {
        execution_id: ExecutionId(row.try_get("execution_id").map_err(decode)?),
        workflow_id: row.try_get("workflow_id").map_err(decode)?,
        workflow_version: row.try_get("workflow_version").map_err(decode)?,
        status: parse_status(row.try_get("status").map_err(decode)?)?,
        initial_input: serde_json::from_str(&initial_input)?,
        current_state: serde_json::from_str(&current_state)?,
        started_at: row.try_get("started_at").map_err(decode)?,
        updated_at: row.try_get("updated_at").map_err(decode)?,
        completed_at: row.try_get("completed_at").map_err(decode)?,
        session_type: None,
        parent_execution_id: row
            .try_get::<Option<Uuid>, _>("parent_execution_id")
            .map_err(decode)?
            .map(ExecutionId),
        segment_number: segment_number.max(0) as u32,
    })
}

pub(super) fn row_to_event(row: &PgRow) -> BackendResult<Event> {
    let kind_json: String = row.try_get("kind_json").map_err(decode)?;
    Ok(Event {
        id: row.try_get("id").map_err(decode)?,
        execution_id: ExecutionId(row.try_get("execution_id").map_err(decode)?),
        sequence: row.try_get("sequence").map_err(decode)?,
        kind: serde_json::from_str(&kind_json)?,
        created_at: row.try_get("created_at").map_err(decode)?,
    })
}

/// An event's columns, made before its transaction starts.
pub(super) struct NewEvent {
    id: Uuid,
    kind_type: String,
    node_id: Option<String>,
    kind_json: String,
    created_at: DateTime<Utc>,
}

/// The `type` tag `serde` writes first in every event kind's JSON.
#[derive(serde::Deserialize)]
struct KindTag<'a> {
    #[serde(rename = "type", borrow)]
    tag: std::borrow::Cow<'a, str>,
}

impl NewEvent {
    /// The row for `kind`. `kind_type` is the serialized `type` and
    /// `node_id` the kind's node, if it has one: the columns the indexed
    /// reads use where SQLite reads the JSON. A node id with a NUL is
    /// refused, as Postgres text cannot hold one.
    pub(super) fn new(
        id: Uuid,
        kind: &EventKind,
        created_at: DateTime<Utc>,
    ) -> BackendResult<Self> {
        let kind_json = serde_json::to_string(kind)?;
        let kind_type = serde_json::from_str::<KindTag<'_>>(&kind_json)?
            .tag
            .into_owned();
        let node_id = kind.node_id().map(str::to_string);
        if let Some(node) = &node_id {
            no_nul("node id", node)?;
        }
        Ok(Self {
            id,
            kind_type,
            node_id,
            kind_json,
            created_at,
        })
    }

    /// The row for a caller's event, keeping its id and time.
    pub(super) fn of(event: &Event) -> BackendResult<Self> {
        Self::new(event.id, &event.kind, event.created_at)
    }

    /// The row for an event the store makes: a new id, the time now.
    pub(super) fn made(kind: &EventKind) -> BackendResult<Self> {
        Self::new(Uuid::new_v4(), kind, Utc::now())
    }
}

/// Append `events` to the run's log in order, in one statement, at the
/// sequences after the log's last, under `tenant`. Returns the last
/// sequence. The caller holds the run's lock ([`lock_run`]), or has just
/// inserted the run's row in the same transaction, so no other writer can
/// take a sequence in between. `events` is not empty.
pub(super) async fn insert_events(
    conn: &mut PgConnection,
    execution_id: Uuid,
    tenant: &str,
    events: &[NewEvent],
) -> Result<EventSequence, TxError> {
    let ids: Vec<Uuid> = events.iter().map(|e| e.id).collect();
    let types: Vec<&str> = events.iter().map(|e| e.kind_type.as_str()).collect();
    let nodes: Vec<Option<&str>> = events.iter().map(|e| e.node_id.as_deref()).collect();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind_json.as_str()).collect();
    let times: Vec<DateTime<Utc>> = events.iter().map(|e| e.created_at).collect();
    let last: Option<i64> = sqlx::query_scalar(
        "WITH last AS (SELECT COALESCE(MAX(sequence), 0) AS sequence FROM events \
                       WHERE execution_id = $1), \
              added AS ( \
                INSERT INTO events \
                  (id, execution_id, tenant_id, sequence, kind_type, node_id, kind_json, created_at) \
                SELECT e.id, $1, $2, last.sequence + e.n, e.kind_type, e.node_id, e.kind_json, \
                       e.created_at \
                FROM last, \
                     UNNEST($3::uuid[], $4::text[], $5::text[], $6::text[], $7::timestamptz[]) \
                       WITH ORDINALITY AS e(id, kind_type, node_id, kind_json, created_at, n) \
                RETURNING sequence) \
         SELECT MAX(sequence) FROM added",
    )
    .bind(execution_id)
    .bind(tenant)
    .bind(ids)
    .bind(types)
    .bind(nodes)
    .bind(kinds)
    .bind(times)
    .fetch_one(conn)
    .await?;
    last.ok_or_else(|| StateBackendError::Database("no event to append".into()).into())
}

/// A work item's columns, made before its transaction starts.
pub(super) struct NewItem {
    id: Uuid,
    execution_id: Uuid,
    node_id: String,
    queue_type: String,
    payload_json: String,
    by_execution: bool,
    attempt: i32,
    max_attempts: i32,
    created_at: DateTime<Utc>,
}

/// Whether the item may be claimed only by a claim that names its run:
/// SQLite reads this from the payload, Postgres from a column.
pub(super) fn by_execution(payload: &serde_json::Value) -> bool {
    payload.get("claim").and_then(|c| c.as_str()) == Some("by_execution")
}

impl NewItem {
    pub(super) fn of(item: &WorkItem) -> BackendResult<Self> {
        no_nul("node id", &item.node_id)?;
        no_nul("queue type", &item.queue_type)?;
        Ok(Self {
            id: item.id,
            execution_id: item.execution_id.0,
            node_id: item.node_id.clone(),
            queue_type: item.queue_type.clone(),
            payload_json: serde_json::to_string(&item.payload)?,
            by_execution: by_execution(&item.payload),
            attempt: to_i32("attempt", item.attempt)?,
            max_attempts: to_i32("max_attempts", item.max_attempts)?,
            created_at: item.created_at,
        })
    }
}

/// Insert `items` as pending, under `tenant`, in one statement.
pub(super) async fn insert_items(
    conn: &mut PgConnection,
    tenant: &str,
    items: &[NewItem],
) -> Result<(), TxError> {
    let ids: Vec<Uuid> = items.iter().map(|i| i.id).collect();
    let runs: Vec<Uuid> = items.iter().map(|i| i.execution_id).collect();
    let nodes: Vec<&str> = items.iter().map(|i| i.node_id.as_str()).collect();
    let queues: Vec<&str> = items.iter().map(|i| i.queue_type.as_str()).collect();
    let payloads: Vec<&str> = items.iter().map(|i| i.payload_json.as_str()).collect();
    let marked: Vec<bool> = items.iter().map(|i| i.by_execution).collect();
    let attempts: Vec<i32> = items.iter().map(|i| i.attempt).collect();
    let budgets: Vec<i32> = items.iter().map(|i| i.max_attempts).collect();
    let times: Vec<DateTime<Utc>> = items.iter().map(|i| i.created_at).collect();
    sqlx::query(
        "INSERT INTO work_items \
           (id, execution_id, tenant_id, node_id, queue_type, payload_json, by_execution, \
            attempt, max_attempts, status, created_at) \
         SELECT i.id, i.execution_id, $1, i.node_id, i.queue_type, i.payload_json, \
                i.by_execution, i.attempt, i.max_attempts, 'pending', i.created_at \
         FROM UNNEST($2::uuid[], $3::uuid[], $4::text[], $5::text[], $6::text[], $7::bool[], \
                     $8::int[], $9::int[], $10::timestamptz[]) \
           AS i(id, execution_id, node_id, queue_type, payload_json, by_execution, attempt, \
                max_attempts, created_at)",
    )
    .bind(tenant)
    .bind(ids)
    .bind(runs)
    .bind(nodes)
    .bind(queues)
    .bind(payloads)
    .bind(marked)
    .bind(attempts)
    .bind(budgets)
    .bind(times)
    .execute(conn)
    .await?;
    Ok(())
}

/// A run's row as [`lock_run`] found it.
pub(super) struct LockedRun {
    pub(super) status: WorkflowStatus,
    pub(super) tenant: String,
}

/// Lock the run's row for the rest of the transaction (rule L1), and read
/// its status and tenant. `None` when there is no such run, or none in
/// `scope`.
pub(super) async fn lock_run(
    conn: &mut PgConnection,
    execution_id: Uuid,
    scope: Option<&str>,
) -> Result<Option<LockedRun>, TxError> {
    let row = match scope {
        None => {
            sqlx::query(
                "SELECT status, tenant_id FROM workflow_executions \
                 WHERE execution_id = $1 FOR UPDATE",
            )
            .bind(execution_id)
            .fetch_optional(conn)
            .await?
        }
        Some(tenant) => {
            sqlx::query(
                "SELECT status, tenant_id FROM workflow_executions \
                 WHERE execution_id = $1 AND tenant_id = $2 FOR UPDATE",
            )
            .bind(execution_id)
            .bind(tenant)
            .fetch_optional(conn)
            .await?
        }
    };
    let Some(row) = row else { return Ok(None) };
    Ok(Some(LockedRun {
        status: parse_status(row.try_get("status").map_err(decode)?)?,
        tenant: row.try_get("tenant_id").map_err(decode)?,
    }))
}

/// [`lock_run`], with a run that is not there answered `NotFound`.
async fn lock_known_run(
    conn: &mut PgConnection,
    execution_id: Uuid,
    scope: Option<&str>,
) -> Result<LockedRun, TxError> {
    lock_run(conn, execution_id, scope)
        .await?
        .ok_or_else(|| StateBackendError::NotFound(execution_id.to_string()).into())
}

/// Set the run's terminal `status`, keeping a completion time it already
/// has. The caller holds the run's lock.
async fn set_terminal_status(
    conn: &mut PgConnection,
    execution_id: Uuid,
    status: &'static str,
) -> Result<(), TxError> {
    sqlx::query(
        "UPDATE workflow_executions SET status = $2, updated_at = statement_timestamp(), \
         completed_at = COALESCE(completed_at, statement_timestamp()) WHERE execution_id = $1",
    )
    .bind(execution_id)
    .bind(status)
    .execute(conn)
    .await?;
    Ok(())
}

/// On a tenant's view, refuse a run that is not the tenant's with
/// `NotFound`. A run's tenant never changes and a run is never deleted, so
/// the answer still holds when a later write lands.
async fn own_run(b: &PgBackend, execution_id: &ExecutionId) -> BackendResult<()> {
    let Some(tenant) = b.scope() else {
        return Ok(());
    };
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM workflow_executions WHERE execution_id = $1 AND tenant_id = $2",
    )
    .bind(execution_id.0)
    .bind(tenant)
    .fetch_optional(&b.pool)
    .await
    .map_err(statement_error)?;
    match found {
        Some(_) => Ok(()),
        None => Err(StateBackendError::NotFound(execution_id.0.to_string())),
    }
}

// ── Workflow definitions ─────────────────────────────────────────────────────

pub(super) async fn store_workflow(
    b: &PgBackend,
    mut def: WorkflowDefinition,
) -> BackendResult<()> {
    if let Some(tenant) = b.scope() {
        def.tenant_id = tenant.to_string();
    }
    let ir_json = serde_json::to_string(&def.ir)?;
    // Never an update: a version is immutable. A row already there is
    // compared below instead.
    let inserted = sqlx::query(
        "INSERT INTO workflow_definitions (tenant_id, workflow_id, version, ir_json, created_at) \
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (tenant_id, workflow_id, version) DO NOTHING",
    )
    .bind(&def.tenant_id)
    .bind(&def.workflow_id)
    .bind(&def.version)
    .bind(&ir_json)
    .bind(def.created_at)
    .execute(&b.pool)
    .await
    .map_err(statement_error)?;
    if inserted.rows_affected() == 1 {
        return Ok(());
    }
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT ir_json FROM workflow_definitions \
         WHERE tenant_id = $1 AND workflow_id = $2 AND version = $3",
    )
    .bind(&def.tenant_id)
    .bind(&def.workflow_id)
    .bind(&def.version)
    .fetch_optional(&b.pool)
    .await
    .map_err(statement_error)?;
    let Some(stored) = stored else {
        // No row and no insert: there is no delete, so refuse rather than guess.
        return Err(version_conflict(&def.workflow_id, &def.version));
    };
    if same_definition(&serde_json::from_str(&stored)?, &def.ir) {
        Ok(())
    } else {
        Err(version_conflict(&def.workflow_id, &def.version))
    }
}

pub(super) async fn get_workflow(
    b: &PgBackend,
    workflow_id: &str,
    version: &str,
) -> BackendResult<Option<WorkflowDefinition>> {
    // Unscoped, the first tenant to register the version, as SQLite reads
    // the first row.
    let query = match b.scope() {
        None => sqlx::query(
            "SELECT tenant_id, workflow_id, version, ir_json, created_at \
             FROM workflow_definitions WHERE workflow_id = $1 AND version = $2 \
             ORDER BY created_at, tenant_id LIMIT 1",
        )
        .bind(workflow_id)
        .bind(version),
        Some(tenant) => sqlx::query(
            "SELECT tenant_id, workflow_id, version, ir_json, created_at \
             FROM workflow_definitions WHERE workflow_id = $1 AND version = $2 AND tenant_id = $3",
        )
        .bind(workflow_id)
        .bind(version)
        .bind(tenant),
    };
    let row = query
        .fetch_optional(&b.pool)
        .await
        .map_err(statement_error)?;
    let Some(row) = row else { return Ok(None) };
    let ir_json: String = row.try_get("ir_json").map_err(decode)?;
    Ok(Some(WorkflowDefinition {
        workflow_id: row.try_get("workflow_id").map_err(decode)?,
        version: row.try_get("version").map_err(decode)?,
        ir: serde_json::from_str(&ir_json)?,
        created_at: row.try_get("created_at").map_err(decode)?,
        tenant_id: row.try_get("tenant_id").map_err(decode)?,
    }))
}

// ── Runs ─────────────────────────────────────────────────────────────────────

/// The tenant a new row of this view takes: the view's, or the default.
fn new_row_tenant(b: &PgBackend) -> &str {
    b.scope().unwrap_or(DEFAULT_TENANT)
}

pub(super) async fn create_execution(
    b: &PgBackend,
    execution: WorkflowExecution,
) -> BackendResult<()> {
    let initial_input = serde_json::to_string(&execution.initial_input)?;
    let current_state = serde_json::to_string(&execution.current_state)?;
    sqlx::query(
        "INSERT INTO workflow_executions \
           (execution_id, tenant_id, workflow_id, workflow_version, status, initial_input, \
            current_state, started_at, updated_at, completed_at, parent_execution_id, \
            segment_number) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(execution.execution_id.0)
    .bind(new_row_tenant(b))
    .bind(&execution.workflow_id)
    .bind(&execution.workflow_version)
    .bind(status_str(&execution.status))
    .bind(&initial_input)
    .bind(&current_state)
    .bind(execution.started_at)
    .bind(execution.updated_at)
    .bind(execution.completed_at)
    .bind(execution.parent_execution_id.as_ref().map(|p| p.0))
    .bind(to_i32("segment_number", execution.segment_number)?)
    .execute(&b.pool)
    .await
    .map_err(statement_error)?;
    Ok(())
}

pub(super) async fn get_execution(
    b: &PgBackend,
    id: &ExecutionId,
) -> BackendResult<Option<WorkflowExecution>> {
    let query = match b.scope() {
        None => sqlx::query("SELECT * FROM workflow_executions WHERE execution_id = $1").bind(id.0),
        Some(tenant) => sqlx::query(
            "SELECT * FROM workflow_executions WHERE execution_id = $1 AND tenant_id = $2",
        )
        .bind(id.0)
        .bind(tenant),
    };
    let row = query
        .fetch_optional(&b.pool)
        .await
        .map_err(statement_error)?;
    row.map(|r| row_to_execution(&r)).transpose()
}

pub(super) async fn execution_tenant(
    b: &PgBackend,
    id: &ExecutionId,
) -> BackendResult<Option<TenantId>> {
    let query = match b.scope() {
        None => {
            sqlx::query_scalar("SELECT tenant_id FROM workflow_executions WHERE execution_id = $1")
                .bind(id.0)
        }
        Some(tenant) => sqlx::query_scalar(
            "SELECT tenant_id FROM workflow_executions WHERE execution_id = $1 AND tenant_id = $2",
        )
        .bind(id.0)
        .bind(tenant),
    };
    let tenant: Option<String> = query
        .fetch_optional(&b.pool)
        .await
        .map_err(statement_error)?;
    Ok(tenant.map(TenantId::from))
}

/// One statement, which locks the run's row as it updates it, so it lands
/// wholly before or after a transaction holding the run's lock. A terminal
/// status stamps the completion time, as SQLite does.
pub(super) async fn update_execution_status(
    b: &PgBackend,
    id: &ExecutionId,
    status: WorkflowStatus,
) -> BackendResult<()> {
    let terminal = status.is_terminal();
    let query = match b.scope() {
        None => sqlx::query(
            "UPDATE workflow_executions SET status = $2, updated_at = statement_timestamp(), \
             completed_at = CASE WHEN $3 THEN statement_timestamp() ELSE completed_at END \
             WHERE execution_id = $1",
        )
        .bind(id.0)
        .bind(status_str(&status))
        .bind(terminal),
        Some(tenant) => sqlx::query(
            "UPDATE workflow_executions SET status = $2, updated_at = statement_timestamp(), \
             completed_at = CASE WHEN $3 THEN statement_timestamp() ELSE completed_at END \
             WHERE execution_id = $1 AND tenant_id = $4",
        )
        .bind(id.0)
        .bind(status_str(&status))
        .bind(terminal)
        .bind(tenant),
    };
    let updated = query
        .execute(&b.pool)
        .await
        .map_err(statement_error)?
        .rows_affected();
    if updated == 0 {
        return Err(StateBackendError::NotFound(id.0.to_string()));
    }
    Ok(())
}

/// Replace the run's state. A run that is not there, or not the view's, is
/// left alone and answered `Ok`, as on SQLite.
pub(super) async fn update_execution_current_state(
    b: &PgBackend,
    id: &ExecutionId,
    current_state: &serde_json::Value,
) -> BackendResult<()> {
    let state = serde_json::to_string(current_state)?;
    let query = match b.scope() {
        None => sqlx::query(
            "UPDATE workflow_executions SET current_state = $2, \
             updated_at = statement_timestamp() WHERE execution_id = $1",
        )
        .bind(id.0)
        .bind(&state),
        Some(tenant) => sqlx::query(
            "UPDATE workflow_executions SET current_state = $2, \
             updated_at = statement_timestamp() WHERE execution_id = $1 AND tenant_id = $3",
        )
        .bind(id.0)
        .bind(&state)
        .bind(tenant),
    };
    query.execute(&b.pool).await.map_err(statement_error)?;
    Ok(())
}

/// The run's state is read under the run's lock, changed here and written
/// back in the same transaction, so two appends at once each keep their
/// value.
pub(super) async fn patch_append_array(
    b: &PgBackend,
    execution_id: &ExecutionId,
    key: &str,
    value: serde_json::Value,
) -> BackendResult<()> {
    let id = execution_id.0;
    let scope = b.scope();
    let value = &value;
    with_tx(&b.pool, "patch_append_array", |tx| {
        Box::pin(async move {
            let state: Option<String> = match scope {
                None => {
                    sqlx::query_scalar(
                        "SELECT current_state FROM workflow_executions \
                         WHERE execution_id = $1 FOR UPDATE",
                    )
                    .bind(id)
                    .fetch_optional(&mut **tx)
                    .await?
                }
                Some(tenant) => {
                    sqlx::query_scalar(
                        "SELECT current_state FROM workflow_executions \
                         WHERE execution_id = $1 AND tenant_id = $2 FOR UPDATE",
                    )
                    .bind(id)
                    .bind(tenant)
                    .fetch_optional(&mut **tx)
                    .await?
                }
            };
            fault_point!("pg.patch_append_array", 1);
            let Some(state) = state else {
                return Err(StateBackendError::NotFound(format!("execution {id}")).into());
            };
            let mut state: serde_json::Value =
                serde_json::from_str(&state).map_err(StateBackendError::from)?;
            state
                .as_object_mut()
                .ok_or_else(|| StateBackendError::Database("state is not a JSON object".into()))?
                .entry(key)
                .or_insert_with(|| serde_json::json!([]))
                .as_array_mut()
                .ok_or_else(|| StateBackendError::Database(format!("{key} is not an array")))?
                .push(value.clone());
            let state = serde_json::to_string(&state).map_err(StateBackendError::from)?;
            sqlx::query(
                "UPDATE workflow_executions SET current_state = $2, \
                 updated_at = statement_timestamp() WHERE execution_id = $1",
            )
            .bind(id)
            .bind(state)
            .execute(&mut **tx)
            .await?;
            fault_point!("pg.patch_append_array", 2);
            Ok(())
        })
    })
    .await
}

pub(super) async fn list_executions(
    b: &PgBackend,
    status: Option<WorkflowStatus>,
    limit: u32,
    offset: u32,
) -> BackendResult<Vec<WorkflowExecution>> {
    let (limit, offset) = (i64::from(limit), i64::from(offset));
    let query = match (status.as_ref().map(status_str), b.scope()) {
        (None, None) => sqlx::query(
            "SELECT * FROM workflow_executions \
             ORDER BY updated_at DESC, execution_id DESC LIMIT $1 OFFSET $2",
        )
        .bind(limit)
        .bind(offset),
        (Some(status), None) => sqlx::query(
            "SELECT * FROM workflow_executions WHERE status = $3 \
             ORDER BY updated_at DESC, execution_id DESC LIMIT $1 OFFSET $2",
        )
        .bind(limit)
        .bind(offset)
        .bind(status),
        (None, Some(tenant)) => sqlx::query(
            "SELECT * FROM workflow_executions WHERE tenant_id = $3 \
             ORDER BY updated_at DESC, execution_id DESC LIMIT $1 OFFSET $2",
        )
        .bind(limit)
        .bind(offset)
        .bind(tenant),
        (Some(status), Some(tenant)) => sqlx::query(
            "SELECT * FROM workflow_executions WHERE status = $3 AND tenant_id = $4 \
             ORDER BY updated_at DESC, execution_id DESC LIMIT $1 OFFSET $2",
        )
        .bind(limit)
        .bind(offset)
        .bind(status)
        .bind(tenant),
    };
    let rows = query.fetch_all(&b.pool).await.map_err(statement_error)?;
    rows.iter().map(row_to_execution).collect()
}

/// A keyset page in id order. `after` is a run id as text, the
/// `next_after` of the page before; text that is not a uuid is a
/// `Database` error, where SQLite compares it as text.
pub(super) async fn list_running_executions_after(
    b: &PgBackend,
    after: Option<&str>,
    limit: u32,
) -> BackendResult<RunningPage> {
    let limit = i64::from(limit);
    let query = match (after, b.scope()) {
        (None, None) => sqlx::query(
            "SELECT * FROM workflow_executions WHERE status = 'running' \
             ORDER BY execution_id LIMIT $1",
        )
        .bind(limit),
        (Some(after), None) => sqlx::query(
            "SELECT * FROM workflow_executions WHERE status = 'running' \
             AND execution_id > $2::uuid ORDER BY execution_id LIMIT $1",
        )
        .bind(limit)
        .bind(after),
        (None, Some(tenant)) => sqlx::query(
            "SELECT * FROM workflow_executions WHERE status = 'running' AND tenant_id = $2 \
             ORDER BY execution_id LIMIT $1",
        )
        .bind(limit)
        .bind(tenant),
        (Some(after), Some(tenant)) => sqlx::query(
            "SELECT * FROM workflow_executions WHERE status = 'running' AND tenant_id = $2 \
             AND execution_id > $3::uuid ORDER BY execution_id LIMIT $1",
        )
        .bind(limit)
        .bind(tenant)
        .bind(after),
    };
    let rows = query.fetch_all(&b.pool).await.map_err(statement_error)?;
    // A row that does not read is left out, but its id still moves the
    // cursor: the scheduler walks these pages, and one bad row must not
    // stop it.
    let next_after = rows
        .last()
        .map(|row| row.try_get::<Uuid, _>("execution_id"))
        .transpose()
        .map_err(decode)?
        .map(|id| id.to_string());
    let executions = rows
        .iter()
        .filter_map(|row| match row_to_execution(row) {
            Ok(e) => Some(e),
            Err(e) => {
                tracing::warn!("skipping an unreadable running execution: {e}");
                None
            }
        })
        .collect();
    Ok(RunningPage {
        executions,
        next_after,
    })
}

// ── Event log ────────────────────────────────────────────────────────────────

/// L1, then the event at the next sequence. The event takes the run's
/// tenant.
pub(super) async fn append_event(b: &PgBackend, event: Event) -> BackendResult<EventSequence> {
    let row = NewEvent::of(&event)?;
    let row = std::slice::from_ref(&row);
    let id = event.execution_id.0;
    let scope = b.scope();
    with_tx(&b.pool, "append_event", |tx| {
        Box::pin(async move {
            let run = lock_known_run(tx, id, scope).await?;
            fault_point!("pg.append_event", 1);
            let sequence = insert_events(tx, id, &run.tenant, row).await?;
            fault_point!("pg.append_event", 2);
            Ok(sequence)
        })
    })
    .await
}

pub(super) async fn get_events(
    b: &PgBackend,
    execution_id: &ExecutionId,
) -> BackendResult<Vec<Event>> {
    get_events_since(b, execution_id, 0).await
}

pub(super) async fn get_events_since(
    b: &PgBackend,
    execution_id: &ExecutionId,
    since: EventSequence,
) -> BackendResult<Vec<Event>> {
    let query = match b.scope() {
        None => sqlx::query(
            "SELECT id, execution_id, sequence, kind_json, created_at FROM events \
             WHERE execution_id = $1 AND sequence > $2 ORDER BY sequence",
        )
        .bind(execution_id.0)
        .bind(since),
        Some(tenant) => sqlx::query(
            "SELECT id, execution_id, sequence, kind_json, created_at FROM events \
             WHERE execution_id = $1 AND sequence > $2 AND tenant_id = $3 ORDER BY sequence",
        )
        .bind(execution_id.0)
        .bind(since)
        .bind(tenant),
    };
    let rows = query.fetch_all(&b.pool).await.map_err(statement_error)?;
    rows.iter().map(row_to_event).collect()
}

/// SQLite's page query: a running sum of each row's size as an event over
/// at most `limit` rows, and every row once the sum before it reaches the
/// budget left out. The size is the kind's bytes (`octet_length`), plus 36
/// for each uuid as text, 40 for the time and 96 for the keys and the
/// sequence: no smaller than the event's JSON, so a page is never read past
/// its budget.
pub(super) async fn get_events_page(
    b: &PgBackend,
    execution_id: &ExecutionId,
    after: EventSequence,
    limit: u32,
    max_bytes: usize,
) -> BackendResult<Vec<Event>> {
    let limit = i64::from(limit);
    let budget = i64::try_from(max_bytes).unwrap_or(i64::MAX);
    let query = match b.scope() {
        None => sqlx::query(
            "SELECT id, execution_id, sequence, kind_json, created_at FROM ( \
               SELECT *, SUM(octet_length(kind_json) + 208) OVER (ORDER BY sequence) \
                         - (octet_length(kind_json) + 208) AS before_bytes \
               FROM (SELECT * FROM events WHERE execution_id = $1 AND sequence > $2 \
                     ORDER BY sequence LIMIT $3) page \
             ) sized WHERE before_bytes < $4 ORDER BY sequence",
        )
        .bind(execution_id.0)
        .bind(after)
        .bind(limit)
        .bind(budget),
        Some(tenant) => sqlx::query(
            "SELECT id, execution_id, sequence, kind_json, created_at FROM ( \
               SELECT *, SUM(octet_length(kind_json) + 208) OVER (ORDER BY sequence) \
                         - (octet_length(kind_json) + 208) AS before_bytes \
               FROM (SELECT * FROM events WHERE execution_id = $1 AND sequence > $2 \
                     AND tenant_id = $5 ORDER BY sequence LIMIT $3) page \
             ) sized WHERE before_bytes < $4 ORDER BY sequence",
        )
        .bind(execution_id.0)
        .bind(after)
        .bind(limit)
        .bind(budget)
        .bind(tenant),
    };
    let rows = query.fetch_all(&b.pool).await.map_err(statement_error)?;
    rows.iter().map(row_to_event).collect()
}

pub(super) async fn latest_sequence(
    b: &PgBackend,
    execution_id: &ExecutionId,
) -> BackendResult<EventSequence> {
    let query = match b.scope() {
        None => sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence), 0) FROM events WHERE execution_id = $1",
        )
        .bind(execution_id.0),
        Some(tenant) => sqlx::query_scalar(
            "SELECT COALESCE(MAX(sequence), 0) FROM events \
             WHERE execution_id = $1 AND tenant_id = $2",
        )
        .bind(execution_id.0)
        .bind(tenant),
    };
    query.fetch_one(&b.pool).await.map_err(statement_error)
}

/// The indexed count. It must answer what the trait's scan of the log
/// answers: a node id with a NUL cannot be in the log (no row can hold
/// one), so it has no completions.
pub(super) async fn count_node_completions(
    b: &PgBackend,
    execution_id: &ExecutionId,
    node_id: &str,
) -> BackendResult<u64> {
    if node_id.contains('\0') {
        return Ok(0);
    }
    let query = match b.scope() {
        None => sqlx::query_scalar(
            "SELECT COUNT(*) FROM events WHERE execution_id = $1 \
             AND kind_type = 'node_completed' AND node_id = $2",
        )
        .bind(execution_id.0)
        .bind(node_id),
        Some(tenant) => sqlx::query_scalar(
            "SELECT COUNT(*) FROM events WHERE execution_id = $1 \
             AND kind_type = 'node_completed' AND node_id = $2 AND tenant_id = $3",
        )
        .bind(execution_id.0)
        .bind(node_id)
        .bind(tenant),
    };
    let n: i64 = query.fetch_one(&b.pool).await.map_err(statement_error)?;
    Ok(n.max(0) as u64)
}

/// Items held under a lease that has not expired by the database's clock.
pub(super) async fn list_claimed_work_items(
    b: &PgBackend,
    limit: u32,
) -> BackendResult<Vec<ClaimedWorkItem>> {
    let limit = i64::from(limit);
    let query = match b.scope() {
        None => sqlx::query(
            "SELECT id, execution_id, node_id, queue_type, worker_id, lease_expires_at \
             FROM work_items WHERE status = 'claimed' AND worker_id IS NOT NULL \
             AND lease_expires_at > statement_timestamp() \
             ORDER BY worker_id, lease_expires_at LIMIT $1",
        )
        .bind(limit),
        Some(tenant) => sqlx::query(
            "SELECT id, execution_id, node_id, queue_type, worker_id, lease_expires_at \
             FROM work_items WHERE status = 'claimed' AND worker_id IS NOT NULL \
             AND lease_expires_at > statement_timestamp() AND tenant_id = $2 \
             ORDER BY worker_id, lease_expires_at LIMIT $1",
        )
        .bind(limit)
        .bind(tenant),
    };
    let rows = query.fetch_all(&b.pool).await.map_err(statement_error)?;
    rows.iter()
        .map(|row| {
            Ok(ClaimedWorkItem {
                worker_id: row.try_get("worker_id").map_err(decode)?,
                work_item_id: row.try_get("id").map_err(decode)?,
                execution_id: ExecutionId(row.try_get("execution_id").map_err(decode)?),
                node_id: row.try_get("node_id").map_err(decode)?,
                queue_type: row.try_get("queue_type").map_err(decode)?,
                lease_expires_at: row.try_get("lease_expires_at").map_err(decode)?,
            })
        })
        .collect()
}

// ── Snapshots ────────────────────────────────────────────────────────────────

/// One statement: the row takes the run's tenant, a snapshot at a sequence
/// the run already has one at replaces it, and a run that is not there, or
/// not the view's, gets no row and is `NotFound`.
pub(super) async fn write_snapshot(b: &PgBackend, snapshot: Snapshot) -> BackendResult<()> {
    let state = serde_json::to_string(&snapshot.state)?;
    let completed = serde_json::to_string(&snapshot.completed_nodes)?;
    let active = serde_json::to_string(&snapshot.active_nodes)?;
    let query = match b.scope() {
        None => sqlx::query(
            "INSERT INTO snapshots \
               (id, execution_id, tenant_id, at_sequence, state_json, created_at, status, \
                completed_nodes_json, active_nodes_json, last_sequence) \
             SELECT $1, r.execution_id, r.tenant_id, $3, $4, $5, $6, $7, $8, $9 \
             FROM workflow_executions r WHERE r.execution_id = $2 \
             ON CONFLICT (execution_id, at_sequence) DO UPDATE SET \
               id = EXCLUDED.id, tenant_id = EXCLUDED.tenant_id, state_json = EXCLUDED.state_json, \
               created_at = EXCLUDED.created_at, status = EXCLUDED.status, \
               completed_nodes_json = EXCLUDED.completed_nodes_json, \
               active_nodes_json = EXCLUDED.active_nodes_json, \
               last_sequence = EXCLUDED.last_sequence",
        ),
        Some(_) => sqlx::query(
            "INSERT INTO snapshots \
               (id, execution_id, tenant_id, at_sequence, state_json, created_at, status, \
                completed_nodes_json, active_nodes_json, last_sequence) \
             SELECT $1, r.execution_id, r.tenant_id, $3, $4, $5, $6, $7, $8, $9 \
             FROM workflow_executions r WHERE r.execution_id = $2 AND r.tenant_id = $10 \
             ON CONFLICT (execution_id, at_sequence) DO UPDATE SET \
               id = EXCLUDED.id, tenant_id = EXCLUDED.tenant_id, state_json = EXCLUDED.state_json, \
               created_at = EXCLUDED.created_at, status = EXCLUDED.status, \
               completed_nodes_json = EXCLUDED.completed_nodes_json, \
               active_nodes_json = EXCLUDED.active_nodes_json, \
               last_sequence = EXCLUDED.last_sequence",
        ),
    };
    let mut query = query
        .bind(snapshot.id)
        .bind(snapshot.execution_id.0)
        .bind(snapshot.at_sequence)
        .bind(&state)
        .bind(snapshot.created_at)
        .bind(status_str(&snapshot.status))
        .bind(&completed)
        .bind(&active)
        .bind(snapshot.last_sequence);
    if let Some(tenant) = b.scope() {
        query = query.bind(tenant);
    }
    let written = query
        .execute(&b.pool)
        .await
        .map_err(statement_error)?
        .rows_affected();
    if written == 0 {
        return Err(StateBackendError::NotFound(
            snapshot.execution_id.0.to_string(),
        ));
    }
    Ok(())
}

pub(super) async fn latest_snapshot(
    b: &PgBackend,
    execution_id: &ExecutionId,
) -> BackendResult<Option<Snapshot>> {
    let query = match b.scope() {
        None => sqlx::query(
            "SELECT * FROM snapshots WHERE execution_id = $1 ORDER BY at_sequence DESC LIMIT 1",
        )
        .bind(execution_id.0),
        Some(tenant) => sqlx::query(
            "SELECT * FROM snapshots WHERE execution_id = $1 AND tenant_id = $2 \
             ORDER BY at_sequence DESC LIMIT 1",
        )
        .bind(execution_id.0)
        .bind(tenant),
    };
    let row = query
        .fetch_optional(&b.pool)
        .await
        .map_err(statement_error)?;
    let Some(row) = row else { return Ok(None) };
    let state: String = row.try_get("state_json").map_err(decode)?;
    let completed: String = row.try_get("completed_nodes_json").map_err(decode)?;
    let active: String = row.try_get("active_nodes_json").map_err(decode)?;
    let status: String = row.try_get("status").map_err(decode)?;
    Ok(Some(Snapshot {
        id: row.try_get("id").map_err(decode)?,
        execution_id: ExecutionId(row.try_get("execution_id").map_err(decode)?),
        at_sequence: row.try_get("at_sequence").map_err(decode)?,
        state: serde_json::from_str(&state)?,
        // An unknown status reads as running, as on SQLite.
        status: parse_status(&status).unwrap_or(WorkflowStatus::Running),
        completed_nodes: serde_json::from_str(&completed)?,
        active_nodes: serde_json::from_str(&active)?,
        last_sequence: row.try_get("last_sequence").map_err(decode)?,
        created_at: row.try_get("created_at").map_err(decode)?,
    }))
}

// ── Starting a run ───────────────────────────────────────────────────────────

/// `start_execution_atomic` and `create_segment_atomic`: the run's row, its
/// seed snapshot when it is a segment, `WorkflowStarted` and the start
/// node's `NodeScheduled`, and the start node's item, in one transaction.
/// The run's row goes in first, so a run id already taken fails there, on
/// its primary key, before anything else is written. On a tenant's view the
/// segment's parent and the item's run, where it is not the new run, must be
/// the tenant's.
pub(super) async fn create_run(
    b: &PgBackend,
    execution: WorkflowExecution,
    seed_snapshot: Option<Snapshot>,
    started_event: EventKind,
    scheduled_event: EventKind,
    work_item: WorkItem,
) -> BackendResult<()> {
    if let Some(parent) = &execution.parent_execution_id {
        own_run(b, parent).await?;
    }
    if work_item.execution_id != execution.execution_id {
        own_run(b, &work_item.execution_id).await?;
    }
    let id = execution.execution_id.0;
    let tenant = new_row_tenant(b);
    let initial_input = serde_json::to_string(&execution.initial_input)?;
    let current_state = serde_json::to_string(&execution.current_state)?;
    let segment_number = to_i32("segment_number", execution.segment_number)?;
    let seed = match &seed_snapshot {
        Some(snap) => Some((
            snap,
            serde_json::to_string(&snap.state)?,
            serde_json::to_string(&snap.completed_nodes)?,
            serde_json::to_string(&snap.active_nodes)?,
        )),
        None => None,
    };
    let events = [
        NewEvent::made(&started_event)?,
        NewEvent::made(&scheduled_event)?,
    ];
    let item = [NewItem::of(&work_item)?];
    let (execution, seed, events, item) = (&execution, &seed, &events, &item);
    let (initial_input, current_state) = (&initial_input, &current_state);
    with_tx(&b.pool, "create_run", |tx| {
        Box::pin(async move {
            sqlx::query(
                "INSERT INTO workflow_executions \
                   (execution_id, tenant_id, workflow_id, workflow_version, status, \
                    initial_input, current_state, started_at, updated_at, completed_at, \
                    parent_execution_id, segment_number) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            )
            .bind(id)
            .bind(tenant)
            .bind(&execution.workflow_id)
            .bind(&execution.workflow_version)
            .bind(status_str(&execution.status))
            .bind(initial_input)
            .bind(current_state)
            .bind(execution.started_at)
            .bind(execution.updated_at)
            .bind(execution.completed_at)
            .bind(execution.parent_execution_id.as_ref().map(|p| p.0))
            .bind(segment_number)
            .execute(&mut **tx)
            .await?;
            fault_point!("pg.create_run", 1);
            if let Some((snap, state, completed, active)) = seed {
                sqlx::query(
                    "INSERT INTO snapshots \
                       (id, execution_id, tenant_id, at_sequence, state_json, created_at, \
                        status, completed_nodes_json, active_nodes_json, last_sequence) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                )
                .bind(snap.id)
                .bind(id)
                .bind(tenant)
                .bind(snap.at_sequence)
                .bind(state)
                .bind(snap.created_at)
                .bind(status_str(&snap.status))
                .bind(completed)
                .bind(active)
                .bind(snap.last_sequence)
                .execute(&mut **tx)
                .await?;
                fault_point!("pg.create_run", 2);
            }
            // The run's row is not visible to anyone else until the commit,
            // so nobody can append between these and the start.
            insert_events(tx, id, tenant, events).await?;
            fault_point!("pg.create_run", 3);
            insert_items(tx, tenant, item).await?;
            fault_point!("pg.create_run", 4);
            Ok(())
        })
    })
    .await
}

// ── Ending a run, conditional appends, the scheduler's pass ──────────────────

/// L1, the terminal check, the status and the terminal event, in one
/// transaction.
pub(super) async fn finish_execution(
    b: &PgBackend,
    event: Event,
    status: WorkflowStatus,
) -> BackendResult<ExecutionFinish> {
    if !status.is_terminal() {
        return Err(StateBackendError::Database(format!(
            "finish_execution requires a terminal status, got {status:?}"
        )));
    }
    let row = NewEvent::of(&event)?;
    let row = std::slice::from_ref(&row);
    let id = event.execution_id.0;
    let scope = b.scope();
    let status = status_str(&status);
    with_tx(&b.pool, "finish_execution", |tx| {
        Box::pin(async move {
            let run = lock_known_run(tx, id, scope).await?;
            fault_point!("pg.finish_execution", 1);
            if run.status.is_terminal() {
                return Ok(ExecutionFinish::AlreadyTerminal(run.status));
            }
            set_terminal_status(tx, id, status).await?;
            fault_point!("pg.finish_execution", 2);
            let sequence = insert_events(tx, id, &run.tenant, row).await?;
            fault_point!("pg.finish_execution", 3);
            Ok(ExecutionFinish::Finished(sequence))
        })
    })
    .await
}

/// L1, then the whole log, across tenants, once the run is known to be the
/// view's: a condition that says nothing has resolved a request must see
/// every row that could have. The condition runs here, under the lock, and
/// the append follows in the same transaction.
pub(super) async fn append_event_if(
    b: &PgBackend,
    event: Event,
    condition: &EventCondition,
) -> BackendResult<ConditionalAppend> {
    let row = NewEvent::of(&event)?;
    let row = std::slice::from_ref(&row);
    let id = event.execution_id.0;
    let scope = b.scope();
    with_tx(&b.pool, "append_event_if", |tx| {
        Box::pin(async move {
            let run = lock_known_run(tx, id, scope).await?;
            fault_point!("pg.append_event_if", 1);
            if run.status.is_terminal() {
                return Ok(ConditionalAppend::ExecutionTerminal(run.status));
            }
            let rows = sqlx::query(
                "SELECT id, execution_id, sequence, kind_json, created_at FROM events \
                 WHERE execution_id = $1 ORDER BY sequence",
            )
            .bind(id)
            .fetch_all(&mut **tx)
            .await?;
            fault_point!("pg.append_event_if", 2);
            let log = rows
                .iter()
                .map(row_to_event)
                .collect::<BackendResult<Vec<_>>>()?;
            if !condition(&log) {
                return Ok(ConditionalAppend::Refused);
            }
            let sequence = insert_events(tx, id, &run.tenant, row).await?;
            fault_point!("pg.append_event_if", 3);
            Ok(ConditionalAppend::Appended(sequence))
        })
    })
    .await
}

/// As on SQLite, in one transaction under L1: the run's status and tenant,
/// the log's last sequence, and whether a node the pass dispatches has a
/// pending or claimed item (the `work_items_node_live` index), then the
/// events, the items and the finish. Rows take the run's tenant.
pub(super) async fn commit_scheduler_pass(
    b: &PgBackend,
    pass: SchedulerPass,
) -> BackendResult<PassCommit> {
    pass.check_shape()?;
    if pass.is_empty() {
        // No lock for a run with nothing to record.
        return Ok(PassCommit::Committed {
            last_sequence: pass.seen_sequence,
        });
    }
    let id = pass.execution_id.0;
    let seen = pass.seen_sequence;
    let scope = b.scope();
    let events = pass
        .events
        .iter()
        .map(NewEvent::made)
        .collect::<BackendResult<Vec<_>>>()?;
    let items = pass
        .work_items
        .iter()
        .map(NewItem::of)
        .collect::<BackendResult<Vec<_>>>()?;
    let nodes: Vec<&str> = pass.work_items.iter().map(|i| i.node_id.as_str()).collect();
    let finish = match &pass.finish {
        Some((status, kind)) => Some((status_str(status), [NewEvent::made(kind)?])),
        None => None,
    };
    let (events, items, nodes, finish) = (&events, &items, &nodes, &finish);
    with_tx(&b.pool, "commit_scheduler_pass", |tx| {
        Box::pin(async move {
            let run = lock_known_run(tx, id, scope).await?;
            fault_point!("pg.commit_scheduler_pass", 1);
            if run.status.is_terminal() {
                return Ok(PassCommit::ExecutionTerminal(run.status));
            }
            let latest: i64 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(sequence), 0) FROM events WHERE execution_id = $1",
            )
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
            fault_point!("pg.commit_scheduler_pass", 2);
            if latest != seen {
                return Ok(PassCommit::Stale {
                    latest_sequence: latest,
                });
            }
            if !nodes.is_empty() {
                let live: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM work_items WHERE execution_id = $1 \
                     AND node_id = ANY($2) AND status IN ('pending', 'claimed'))",
                )
                .bind(id)
                .bind(nodes)
                .fetch_one(&mut **tx)
                .await?;
                fault_point!("pg.commit_scheduler_pass", 3);
                if live {
                    return Ok(PassCommit::Stale {
                        latest_sequence: latest,
                    });
                }
            }
            let mut last_sequence = latest;
            if !events.is_empty() {
                last_sequence = insert_events(tx, id, &run.tenant, events).await?;
                fault_point!("pg.commit_scheduler_pass", 4);
            }
            if !items.is_empty() {
                insert_items(tx, &run.tenant, items).await?;
                fault_point!("pg.commit_scheduler_pass", 5);
            }
            if let Some((status, event)) = finish {
                set_terminal_status(tx, id, status).await?;
                fault_point!("pg.commit_scheduler_pass", 6);
                last_sequence = insert_events(tx, id, &run.tenant, event).await?;
                fault_point!("pg.commit_scheduler_pass", 7);
            }
            fault_point!("pg.commit_scheduler_pass", 8);
            Ok(PassCommit::Committed { last_sequence })
        })
    })
    .await
}
