//! [`StateBackend::list_pending_approvals`](crate::backend::StateBackend::list_pending_approvals):
//! the SQLite query, and the order and cursor rule the in-memory backend
//! applies the same way.

use crate::backend::{ApprovalCursor, BackendResult, PendingApprovalRow};
use crate::sqlite::{map_db_err, parse_execution_id};
use sqlx::{Row, SqlitePool};

/// The pending rows of live runs, scoped to `tenant_id` by the run's own row
/// when given. The projection's `tenant_id` column is never read: the
/// projector writes `default` there for every row.
pub(crate) async fn list_pending_approvals_in(
    pool: &SqlitePool,
    tenant_id: Option<&str>,
    after: Option<&ApprovalCursor>,
    limit: u32,
) -> BackendResult<Vec<PendingApprovalRow>> {
    let tenant_clause = if tenant_id.is_some() {
        " AND e.tenant_id = ?"
    } else {
        ""
    };
    let cursor_clause = if after.is_some() {
        " AND (p.updated_at, p.execution_id, p.node_id) > (?, ?, ?)"
    } else {
        ""
    };
    let select = format!(
        "SELECT p.execution_id, e.workflow_id, e.workflow_version, p.node_id, p.tool_name, \
                p.approver, p.context_json, p.last_sequence, p.updated_at \
         FROM proj_approvals p JOIN workflow_executions e ON e.execution_id = p.execution_id \
         WHERE p.status = 'pending' AND e.status IN ('running', 'paused')\
         {tenant_clause}{cursor_clause} \
         ORDER BY p.updated_at, p.execution_id, p.node_id \
         LIMIT ?"
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(select));
    if let Some(t) = tenant_id {
        q = q.bind(t);
    }
    if let Some(c) = after {
        q = q
            .bind(c.updated_at.as_str())
            .bind(c.execution_id.as_str())
            .bind(c.node_id.as_str());
    }
    let rows = q
        .bind(i64::from(limit))
        .fetch_all(pool)
        .await
        .map_err(map_db_err)?;

    rows.iter()
        .map(|r| {
            let context = r
                .try_get::<Option<String>, _>("context_json")
                .map_err(map_db_err)?
                .map(|s| serde_json::from_str::<serde_json::Value>(&s))
                .transpose()?;
            Ok(PendingApprovalRow {
                execution_id: parse_execution_id(
                    r.try_get::<&str, _>("execution_id").map_err(map_db_err)?,
                )?,
                workflow_id: r.try_get("workflow_id").map_err(map_db_err)?,
                workflow_version: r.try_get("workflow_version").map_err(map_db_err)?,
                node_id: r.try_get("node_id").map_err(map_db_err)?,
                tool_name: r.try_get("tool_name").map_err(map_db_err)?,
                approver: r.try_get("approver").map_err(map_db_err)?,
                context,
                request_sequence: r.try_get("last_sequence").map_err(map_db_err)?,
                updated_at: r.try_get("updated_at").map_err(map_db_err)?,
            })
        })
        .collect()
}

/// One page of `rows` (pending rows of live runs) as the SQL above pages
/// them: in `(updated_at, execution_id, node_id)` order, compared as text,
/// after `after`, at most `limit`.
pub(crate) fn page(
    mut rows: Vec<PendingApprovalRow>,
    after: Option<&ApprovalCursor>,
    limit: u32,
) -> Vec<PendingApprovalRow> {
    fn sort_key(r: &PendingApprovalRow) -> (String, String, String) {
        (
            r.updated_at.clone(),
            r.execution_id.0.to_string(),
            r.node_id.clone(),
        )
    }
    if let Some(c) = after {
        let from = (
            c.updated_at.clone(),
            c.execution_id.clone(),
            c.node_id.clone(),
        );
        rows.retain(|r| sort_key(r) > from);
    }
    rows.sort_by_cached_key(sort_key);
    rows.truncate(limit as usize);
    rows
}
