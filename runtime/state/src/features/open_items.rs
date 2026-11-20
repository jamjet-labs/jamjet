//! [`StateBackend::open_work_items`](crate::backend::StateBackend::open_work_items)
//! on SQLite.

use crate::backend::{BackendResult, OpenWorkItem};
use crate::sqlite::{map_db_err, parse_datetime};
use jamjet_core::workflow::ExecutionId;
use sqlx::{Row, SqlitePool};

/// The run's pending and claimed items, oldest first. Scoped to `tenant_id`
/// by the run's own row when given, never by the item's: the scheduler
/// enqueues unscoped, so an item's own column says `default`.
pub(crate) async fn open_work_items_in(
    pool: &SqlitePool,
    tenant_id: Option<&str>,
    execution_id: &ExecutionId,
) -> BackendResult<Vec<OpenWorkItem>> {
    let (join, tenant_clause) = if tenant_id.is_some() {
        (
            " JOIN workflow_executions e ON e.execution_id = w.execution_id",
            " AND e.tenant_id = ?",
        )
    } else {
        ("", "")
    };
    let select = format!(
        "SELECT w.id, w.node_id, w.queue_type, w.status, w.lease_expires_at \
         FROM work_items w{join} \
         WHERE w.execution_id = ? AND w.status IN ('pending', 'claimed'){tenant_clause} \
         ORDER BY w.created_at, w.id"
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(select)).bind(execution_id.0.to_string());
    if let Some(t) = tenant_id {
        q = q.bind(t);
    }
    let rows = q.fetch_all(pool).await.map_err(map_db_err)?;

    rows.iter()
        .map(|r| {
            let id = r.try_get::<&str, _>("id").map_err(map_db_err)?;
            Ok(OpenWorkItem {
                id: uuid::Uuid::parse_str(id).map_err(|e| {
                    crate::backend::StateBackendError::Database(format!("invalid item id: {e}"))
                })?,
                node_id: r.try_get("node_id").map_err(map_db_err)?,
                queue_type: r.try_get("queue_type").map_err(map_db_err)?,
                claimed: r.try_get::<&str, _>("status").map_err(map_db_err)? == "claimed",
                lease_expires_at: r
                    .try_get::<Option<&str>, _>("lease_expires_at")
                    .map_err(map_db_err)?
                    .map(parse_datetime)
                    .transpose()?,
            })
        })
        .collect()
}
