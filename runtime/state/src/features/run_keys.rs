//! Run keys: a caller's name for a run, claimed in the run's start
//! transaction (`StateBackend::start_execution_keyed`, `get_run_key`).

use crate::backend::{
    BackendResult, KeyedStart, RunKeyClaim, RunKeyRecord, StateBackendError, WorkItem,
};
use crate::event::EventKind;
use crate::sqlite::{map_db_err, parse_execution_id};
use crate::tenant::DEFAULT_TENANT;
use jamjet_core::workflow::WorkflowExecution;
use sqlx::{Row, SqliteConnection, SqlitePool};

/// `started` with `run_key` written into it, so the log names the key the
/// run was started with. A keyed start must begin with `WorkflowStarted`.
pub(crate) fn with_run_key(mut started: EventKind, key: &str) -> BackendResult<EventKind> {
    match &mut started {
        EventKind::WorkflowStarted { run_key, .. } => {
            *run_key = Some(key.to_string());
            Ok(started)
        }
        _ => Err(StateBackendError::Database(
            "a keyed start must begin with WorkflowStarted".into(),
        )),
    }
}

/// [`crate::backend::StateBackend::start_execution_keyed`] for both SQLite
/// backends: the run's rows and the key in one `create_run_in` transaction.
pub(crate) async fn start_keyed_in(
    pool: &SqlitePool,
    tenant_id: Option<&str>,
    execution: WorkflowExecution,
    started_event: EventKind,
    scheduled_event: EventKind,
    work_item: WorkItem,
    key: RunKeyClaim,
) -> BackendResult<KeyedStart> {
    let started_event = with_run_key(started_event, &key.run_key)?;
    crate::sqlite::create_run_in(
        pool,
        tenant_id,
        execution,
        None,
        started_event,
        scheduled_event,
        work_item,
        Some(&key),
    )
    .await
}

/// Claim `key` for `execution_id` (a bare uuid) under `tenant`, inside the
/// caller's write transaction. `None`: the key is this run's now, given to it
/// fresh or moved from the run `key.replaces` names. `Some`: the key names
/// another run, returned as it stands; nothing was written.
///
/// A move keeps the workflow, version and input hash the key was first
/// claimed with. The `execution_id` condition of the move is the check that
/// `replaces` still names the key's run: read and moved under one write lock,
/// a stale `replaces` matches no row.
pub(crate) async fn claim_in(
    conn: &mut SqliteConnection,
    tenant: &str,
    key: &RunKeyClaim,
    execution_id: &str,
) -> BackendResult<Option<RunKeyRecord>> {
    let now = chrono::Utc::now().to_rfc3339();
    let held = sqlx::query(
        "SELECT execution_id, workflow_id, workflow_version, input_hash \
         FROM run_keys WHERE tenant_id = ? AND run_key = ?",
    )
    .bind(tenant)
    .bind(&key.run_key)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_db_err)?;

    let Some(row) = held else {
        sqlx::query(
            "INSERT INTO run_keys \
             (tenant_id, run_key, execution_id, workflow_id, workflow_version, input_hash, \
              created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(tenant)
        .bind(&key.run_key)
        .bind(execution_id)
        .bind(&key.workflow_id)
        .bind(&key.workflow_version)
        .bind(&key.input_hash)
        .bind(&now)
        .bind(&now)
        .execute(&mut *conn)
        .await
        .map_err(map_db_err)?;
        return Ok(None);
    };
    let held = record_from_row(&row)?;

    if let Some(replaces) = &key.replaces {
        let moved = sqlx::query(
            "UPDATE run_keys SET execution_id = ?, updated_at = ? \
             WHERE tenant_id = ? AND run_key = ? AND execution_id = ?",
        )
        .bind(execution_id)
        .bind(&now)
        .bind(tenant)
        .bind(&key.run_key)
        .bind(replaces.0.to_string())
        .execute(&mut *conn)
        .await
        .map_err(map_db_err)?
        .rows_affected();
        if moved == 1 {
            return Ok(None);
        }
    }
    Ok(Some(held))
}

/// [`crate::backend::StateBackend::get_run_key`] for both SQLite backends.
/// `None` (the unscoped backend) reads the default tenant's keys.
pub(crate) async fn get_run_key_in(
    pool: &SqlitePool,
    tenant_id: Option<&str>,
    run_key: &str,
) -> BackendResult<Option<RunKeyRecord>> {
    let row = sqlx::query(
        "SELECT execution_id, workflow_id, workflow_version, input_hash \
         FROM run_keys WHERE tenant_id = ? AND run_key = ?",
    )
    .bind(tenant_id.unwrap_or(DEFAULT_TENANT))
    .bind(run_key)
    .fetch_optional(pool)
    .await
    .map_err(map_db_err)?;
    row.map(|r| record_from_row(&r)).transpose()
}

fn record_from_row(row: &sqlx::sqlite::SqliteRow) -> BackendResult<RunKeyRecord> {
    Ok(RunKeyRecord {
        execution_id: parse_execution_id(row.try_get("execution_id").map_err(map_db_err)?)?,
        workflow_id: row.try_get("workflow_id").map_err(map_db_err)?,
        workflow_version: row.try_get("workflow_version").map_err(map_db_err)?,
        input_hash: row.try_get("input_hash").map_err(map_db_err)?,
    })
}
