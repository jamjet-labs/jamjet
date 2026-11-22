//! Run keys on Postgres: a caller's name for a run, claimed inside the run's
//! start transaction (`StateBackend::start_execution_keyed`, `get_run_key`).
//!
//! SQLite claims a key under its one writer. Here the key's row is the lock:
//! the insert of a key no one holds waits for a start that inserted the same
//! key and has not committed, and then finds it, so two starts with one key
//! make one run. A move from the run `replaces` names re-checks that run on
//! the row as it stands when the update gets it.

use super::runs::decode;
use super::tx::{no_nul, statement_error, TxError};
use super::PgBackend;
use crate::backend::{BackendResult, RunKeyClaim, RunKeyRecord, StateBackendError};
use crate::tenant::DEFAULT_TENANT;
use jamjet_core::workflow::ExecutionId;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

/// Refuse a claim whose text Postgres cannot store, before its transaction.
pub(super) fn check_claim(key: &RunKeyClaim) -> BackendResult<()> {
    no_nul("run key", &key.run_key)?;
    no_nul("workflow id", &key.workflow_id)?;
    no_nul("workflow version", &key.workflow_version)?;
    no_nul("input hash", &key.input_hash)
}

/// Claim `key` for the run `execution_id` under `tenant`, inside the
/// caller's transaction. `None`: the key is this run's now, given to it
/// fresh or moved from the run `key.replaces` names. `Some`: the key names
/// another run, returned as it stands; nothing was written.
///
/// A move keeps the workflow, version and input hash the key was first
/// claimed with.
pub(super) async fn claim(
    conn: &mut PgConnection,
    tenant: &str,
    key: &RunKeyClaim,
    execution_id: Uuid,
) -> Result<Option<RunKeyRecord>, TxError> {
    let fresh = sqlx::query(
        "INSERT INTO run_keys \
           (tenant_id, run_key, execution_id, workflow_id, workflow_version, input_hash, \
            created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, statement_timestamp(), statement_timestamp()) \
         ON CONFLICT (tenant_id, run_key) DO NOTHING",
    )
    .bind(tenant)
    .bind(&key.run_key)
    .bind(execution_id)
    .bind(&key.workflow_id)
    .bind(&key.workflow_version)
    .bind(&key.input_hash)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if fresh == 1 {
        return Ok(None);
    }
    if let Some(replaces) = &key.replaces {
        let moved = sqlx::query(
            "UPDATE run_keys SET execution_id = $1, updated_at = statement_timestamp() \
             WHERE tenant_id = $2 AND run_key = $3 AND execution_id = $4",
        )
        .bind(execution_id)
        .bind(tenant)
        .bind(&key.run_key)
        .bind(replaces.0)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if moved == 1 {
            return Ok(None);
        }
    }
    let held = sqlx::query(
        "SELECT execution_id, workflow_id, workflow_version, input_hash \
         FROM run_keys WHERE tenant_id = $1 AND run_key = $2",
    )
    .bind(tenant)
    .bind(&key.run_key)
    .fetch_optional(&mut *conn)
    .await?;
    match held {
        Some(row) => Ok(Some(record(&row)?)),
        // The insert found the key held, and no key is ever removed.
        None => Err(StateBackendError::Database(format!(
            "run key {} was held and then not found",
            key.run_key
        ))
        .into()),
    }
}

/// What `run_key` names in the view's tenant, the default tenant's on the
/// unscoped store. A key with a NUL cannot be held, so it names nothing.
pub(super) async fn get_run_key(
    b: &PgBackend,
    run_key: &str,
) -> BackendResult<Option<RunKeyRecord>> {
    if run_key.contains('\0') {
        return Ok(None);
    }
    let row = sqlx::query(
        "SELECT execution_id, workflow_id, workflow_version, input_hash \
         FROM run_keys WHERE tenant_id = $1 AND run_key = $2",
    )
    .bind(b.scope().unwrap_or(DEFAULT_TENANT))
    .bind(run_key)
    .fetch_optional(&b.pool)
    .await
    .map_err(statement_error)?;
    row.map(|r| record(&r)).transpose()
}

fn record(row: &PgRow) -> BackendResult<RunKeyRecord> {
    Ok(RunKeyRecord {
        execution_id: ExecutionId(row.try_get("execution_id").map_err(decode)?),
        workflow_id: row.try_get("workflow_id").map_err(decode)?,
        workflow_version: row.try_get("workflow_version").map_err(decode)?,
        input_hash: row.try_get("input_hash").map_err(decode)?,
    })
}
