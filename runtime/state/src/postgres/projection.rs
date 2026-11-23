//! The approvals read model, on Postgres: the rows the projector writes, its
//! checkpoints, and the list of every request waiting for a decision.
//!
//! No run lock: a row and a checkpoint have their own keys, and an upsert
//! decides each. A row and the checkpoint it moves land in one transaction.
//! Rows take the run's tenant, read from the run's row in the statement that
//! writes them; for a run that is not there, the unscoped store writes
//! `default`, as SQLite does. A tenant's view writes only into its own runs:
//! for another tenant's run, or one nobody holds, the statement finds no run
//! row, writes nothing and the call is `NotFound`. Reads through a view
//! filter on each row's own `tenant_id`; the pending list judges a row by its
//! run's row, as the SQLite list does.
//!
//! Node ids sort bytewise (`COLLATE "C"`), as SQLite compares text, whatever
//! collation the database has.

use super::runs::decode;
use super::tx::{no_nul, statement_error, with_tx, TxError};
use super::PgBackend;
use crate::backend::{
    ApprovalCursor, ApprovalProjectionRow, BackendResult, PendingApprovalRow, StateBackendError,
};
use chrono::{DateTime, SecondsFormat, Utc};
use jamjet_core::workflow::ExecutionId;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use std::collections::HashMap;
use uuid::Uuid;

/// A projected row's columns, made before its transaction starts.
struct NewRow {
    node_id: String,
    status: String,
    user_id: Option<String>,
    comment: Option<String>,
    last_sequence: i64,
    tool_name: Option<String>,
    approver: Option<String>,
    context_json: Option<String>,
}

impl NewRow {
    fn of(row: &ApprovalProjectionRow) -> BackendResult<Self> {
        no_nul("node id", &row.node_id)?;
        no_nul("approval status", &row.status)?;
        for text in [&row.user_id, &row.comment, &row.tool_name, &row.approver]
            .into_iter()
            .flatten()
        {
            no_nul("approval text", text)?;
        }
        Ok(Self {
            node_id: row.node_id.clone(),
            status: row.status.clone(),
            user_id: row.user_id.clone(),
            comment: row.comment.clone(),
            last_sequence: row.last_sequence,
            tool_name: row.tool_name.clone(),
            approver: row.approver.clone(),
            context_json: row
                .context
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        })
    }
}

/// Upsert `rows` into the run's projection in one statement, every row at
/// one time. A stored row with a later `last_sequence` is kept: a stale
/// batch from a second projector never moves a row back. Returns how many
/// rows it wrote: none when a view does not own the run, or when every
/// stored row is newer. `rows` hold one row per node.
async fn upsert_rows(
    conn: &mut PgConnection,
    run: Uuid,
    scope: Option<&str>,
    rows: &[NewRow],
) -> Result<u64, TxError> {
    let nodes: Vec<&str> = rows.iter().map(|r| r.node_id.as_str()).collect();
    let statuses: Vec<&str> = rows.iter().map(|r| r.status.as_str()).collect();
    let users: Vec<Option<&str>> = rows.iter().map(|r| r.user_id.as_deref()).collect();
    let comments: Vec<Option<&str>> = rows.iter().map(|r| r.comment.as_deref()).collect();
    let sequences: Vec<i64> = rows.iter().map(|r| r.last_sequence).collect();
    let tools: Vec<Option<&str>> = rows.iter().map(|r| r.tool_name.as_deref()).collect();
    let approvers: Vec<Option<&str>> = rows.iter().map(|r| r.approver.as_deref()).collect();
    let contexts: Vec<Option<&str>> = rows.iter().map(|r| r.context_json.as_deref()).collect();
    let query = match scope {
        None => sqlx::query(
            "INSERT INTO proj_approvals (execution_id, node_id, status, user_id, comment, \
               last_sequence, tool_name, approver, context_json, tenant_id, updated_at) \
             SELECT $1, r.node_id, r.status, r.user_id, r.comment, r.last_sequence, \
               r.tool_name, r.approver, r.context_json, \
               COALESCE((SELECT tenant_id FROM workflow_executions WHERE execution_id = $1), \
                 'default'), \
               statement_timestamp() \
             FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[], $6::bigint[], \
                         $7::text[], $8::text[], $9::text[]) \
               AS r(node_id, status, user_id, comment, last_sequence, tool_name, approver, \
                    context_json) \
             ON CONFLICT (execution_id, node_id) DO UPDATE SET status = EXCLUDED.status, \
               user_id = EXCLUDED.user_id, comment = EXCLUDED.comment, \
               last_sequence = EXCLUDED.last_sequence, tool_name = EXCLUDED.tool_name, \
               approver = EXCLUDED.approver, context_json = EXCLUDED.context_json, \
               updated_at = EXCLUDED.updated_at \
             WHERE proj_approvals.last_sequence <= EXCLUDED.last_sequence",
        ),
        Some(_) => sqlx::query(
            "INSERT INTO proj_approvals (execution_id, node_id, status, user_id, comment, \
               last_sequence, tool_name, approver, context_json, tenant_id, updated_at) \
             SELECT e.execution_id, r.node_id, r.status, r.user_id, r.comment, \
               r.last_sequence, r.tool_name, r.approver, r.context_json, e.tenant_id, \
               statement_timestamp() \
             FROM workflow_executions e, \
                  UNNEST($2::text[], $3::text[], $4::text[], $5::text[], $6::bigint[], \
                         $7::text[], $8::text[], $9::text[]) \
               AS r(node_id, status, user_id, comment, last_sequence, tool_name, approver, \
                    context_json) \
             WHERE e.execution_id = $1 AND e.tenant_id = $10 \
             ON CONFLICT (execution_id, node_id) DO UPDATE SET status = EXCLUDED.status, \
               user_id = EXCLUDED.user_id, comment = EXCLUDED.comment, \
               last_sequence = EXCLUDED.last_sequence, tool_name = EXCLUDED.tool_name, \
               approver = EXCLUDED.approver, context_json = EXCLUDED.context_json, \
               updated_at = EXCLUDED.updated_at \
             WHERE proj_approvals.last_sequence <= EXCLUDED.last_sequence",
        ),
    };
    let mut query = query
        .bind(run)
        .bind(nodes)
        .bind(statuses)
        .bind(users)
        .bind(comments)
        .bind(sequences)
        .bind(tools)
        .bind(approvers)
        .bind(contexts);
    if let Some(tenant) = scope {
        query = query.bind(tenant);
    }
    Ok(query.execute(conn).await?.rows_affected())
}

/// The statement that moves a projection's checkpoint for a run, the run's
/// tenant read as [`upsert_rows`] reads it. It only moves forward: a stored
/// checkpoint past `$3` stays, and the statement writes no row. Binds `$1`
/// the projection, `$2` the run, `$3` the checkpoint and, scoped, `$4` the
/// tenant.
fn checkpoint_sql(scoped: bool) -> &'static str {
    if scoped {
        "INSERT INTO projector_checkpoints \
           (projection_name, execution_id, last_sequence, tenant_id, updated_at) \
         SELECT $1, e.execution_id, $3, e.tenant_id, statement_timestamp() \
         FROM workflow_executions e WHERE e.execution_id = $2 AND e.tenant_id = $4 \
         ON CONFLICT (projection_name, execution_id) DO UPDATE SET \
           last_sequence = EXCLUDED.last_sequence, updated_at = EXCLUDED.updated_at \
         WHERE projector_checkpoints.last_sequence <= EXCLUDED.last_sequence"
    } else {
        "INSERT INTO projector_checkpoints \
           (projection_name, execution_id, last_sequence, tenant_id, updated_at) \
         VALUES ($1, $2, $3, \
           COALESCE((SELECT tenant_id FROM workflow_executions WHERE execution_id = $2), \
             'default'), \
           statement_timestamp()) \
         ON CONFLICT (projection_name, execution_id) DO UPDATE SET \
           last_sequence = EXCLUDED.last_sequence, updated_at = EXCLUDED.updated_at \
         WHERE projector_checkpoints.last_sequence <= EXCLUDED.last_sequence"
    }
}

fn checkpoint_query<'q>(
    name: &'q str,
    run: Uuid,
    checkpoint: i64,
    scope: Option<&'q str>,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    let query = sqlx::query(checkpoint_sql(scope.is_some()))
        .bind(name)
        .bind(run)
        .bind(checkpoint);
    match scope {
        Some(tenant) => query.bind(tenant),
        None => query,
    }
}

/// The row and the checkpoint it moves, in one transaction.
pub(super) async fn apply_approval_projection(
    b: &PgBackend,
    row: ApprovalProjectionRow,
    projection_name: &str,
    new_checkpoint: i64,
) -> BackendResult<()> {
    no_nul("projection name", projection_name)?;
    let rows = [NewRow::of(&row)?];
    apply(
        b,
        "apply_approval_projection",
        &row.execution_id,
        &rows,
        projection_name,
        new_checkpoint,
    )
    .await
}

/// Every row under `execution_id`, whatever run a row names, and the
/// checkpoint, in one transaction; the checkpoint moves even when `rows` is
/// empty. Where the batch holds two rows of one node, the one with the
/// higher `last_sequence` is written, the later one on a tie, as one
/// forward-only upsert after another would leave it.
pub(super) async fn apply_approval_projection_batch(
    b: &PgBackend,
    rows: Vec<ApprovalProjectionRow>,
    projection_name: &str,
    execution_id: &ExecutionId,
    new_checkpoint: i64,
) -> BackendResult<()> {
    no_nul("projection name", projection_name)?;
    let mut last: HashMap<&str, usize> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        let kept = last.entry(row.node_id.as_str()).or_insert(i);
        if rows[*kept].last_sequence <= row.last_sequence {
            *kept = i;
        }
    }
    let kept = rows
        .iter()
        .enumerate()
        .filter(|(i, row)| last[row.node_id.as_str()] == *i)
        .map(|(_, row)| NewRow::of(row))
        .collect::<BackendResult<Vec<_>>>()?;
    apply(
        b,
        "apply_approval_projection_batch",
        execution_id,
        &kept,
        projection_name,
        new_checkpoint,
    )
    .await
}

/// Whether a run of the tenant `$2` has the id `$1`.
const OWNS_RUN: &str = "SELECT EXISTS (SELECT 1 FROM workflow_executions \
                        WHERE execution_id = $1 AND tenant_id = $2)";

/// Whether `run` is a run of `tenant`: what a view may write into.
async fn owns_run(conn: &mut PgConnection, run: Uuid, tenant: &str) -> Result<bool, TxError> {
    let owned: bool = sqlx::query_scalar(OWNS_RUN)
        .bind(run)
        .bind(tenant)
        .fetch_one(conn)
        .await?;
    Ok(owned)
}

/// The one body of both projection writes, as `pg.apply_approval_projection`
/// in the fault points: the rows in one statement, then the checkpoint.
/// A statement that wrote nothing found either newer rows (fine: the
/// write only moves forward) or, on a view, a run that is not the view's
/// (`NotFound`); only then is the run's owner read.
async fn apply(
    b: &PgBackend,
    op: &'static str,
    execution_id: &ExecutionId,
    rows: &[NewRow],
    projection_name: &str,
    new_checkpoint: i64,
) -> BackendResult<()> {
    let run = execution_id.0;
    let scope = b.scope();
    let not_found = || StateBackendError::NotFound(run.to_string());
    with_tx(&b.pool, op, |tx| {
        Box::pin(async move {
            // The unscoped store writes into any run. A view's ownership is
            // read, never taken from a row count: rows written into a run
            // the checkpoint then finds is not the view's are rolled back.
            let mut owned = scope.is_none();
            if !rows.is_empty() {
                let written = upsert_rows(tx, run, scope, rows).await?;
                if let (0, Some(tenant)) = (written, scope) {
                    if !owns_run(tx, run, tenant).await? {
                        return Err(not_found().into());
                    }
                    owned = true;
                }
                fault_point!("pg.apply_approval_projection", 1);
            }
            let moved = checkpoint_query(projection_name, run, new_checkpoint, scope)
                .execute(&mut **tx)
                .await?
                .rows_affected();
            if let (0, false, Some(tenant)) = (moved, owned, scope) {
                if !owns_run(tx, run, tenant).await? {
                    return Err(not_found().into());
                }
            }
            fault_point!("pg.apply_approval_projection", 2);
            Ok(())
        })
    })
    .await
}

/// The run's projected rows, in node order.
pub(super) async fn get_approval_projection(
    b: &PgBackend,
    execution_id: &ExecutionId,
) -> BackendResult<Vec<ApprovalProjectionRow>> {
    let query = match b.scope() {
        None => sqlx::query(
            "SELECT node_id, status, user_id, comment, last_sequence, tool_name, approver, \
             context_json FROM proj_approvals WHERE execution_id = $1 \
             ORDER BY node_id COLLATE \"C\"",
        )
        .bind(execution_id.0),
        Some(tenant) => sqlx::query(
            "SELECT node_id, status, user_id, comment, last_sequence, tool_name, approver, \
             context_json FROM proj_approvals WHERE execution_id = $1 AND tenant_id = $2 \
             ORDER BY node_id COLLATE \"C\"",
        )
        .bind(execution_id.0)
        .bind(tenant),
    };
    let rows = query.fetch_all(&b.pool).await.map_err(statement_error)?;
    rows.iter()
        .map(|row| {
            let context: Option<String> = row.try_get("context_json").map_err(decode)?;
            Ok(ApprovalProjectionRow {
                execution_id: execution_id.clone(),
                node_id: row.try_get("node_id").map_err(decode)?,
                status: row.try_get("status").map_err(decode)?,
                user_id: row.try_get("user_id").map_err(decode)?,
                comment: row.try_get("comment").map_err(decode)?,
                last_sequence: row.try_get("last_sequence").map_err(decode)?,
                tool_name: row.try_get("tool_name").map_err(decode)?,
                approver: row.try_get("approver").map_err(decode)?,
                context: context.map(|c| serde_json::from_str(&c)).transpose()?,
            })
        })
        .collect()
}

/// 0 when the projection has no checkpoint for the run yet.
pub(super) async fn get_projector_checkpoint(
    b: &PgBackend,
    projection_name: &str,
    execution_id: &ExecutionId,
) -> BackendResult<i64> {
    if projection_name.contains('\0') {
        return Ok(0);
    }
    let query = match b.scope() {
        None => sqlx::query_scalar(
            "SELECT last_sequence FROM projector_checkpoints \
             WHERE projection_name = $1 AND execution_id = $2",
        )
        .bind(projection_name)
        .bind(execution_id.0),
        Some(tenant) => sqlx::query_scalar(
            "SELECT last_sequence FROM projector_checkpoints \
             WHERE projection_name = $1 AND execution_id = $2 AND tenant_id = $3",
        )
        .bind(projection_name)
        .bind(execution_id.0)
        .bind(tenant),
    };
    let checkpoint: Option<i64> = query
        .fetch_optional(&b.pool)
        .await
        .map_err(statement_error)?;
    Ok(checkpoint.unwrap_or(0))
}

/// One statement, forward only; on a view, `NotFound` for a run that is not
/// the view's, read only when the statement wrote nothing.
pub(super) async fn set_projector_checkpoint(
    b: &PgBackend,
    projection_name: &str,
    execution_id: &ExecutionId,
    new_checkpoint: i64,
) -> BackendResult<()> {
    no_nul("projection name", projection_name)?;
    let run = execution_id.0;
    let scope = b.scope();
    let moved = checkpoint_query(projection_name, run, new_checkpoint, scope)
        .execute(&b.pool)
        .await
        .map_err(statement_error)?
        .rows_affected();
    let Some(tenant) = scope.filter(|_| moved == 0) else {
        return Ok(());
    };
    let owned: bool = sqlx::query_scalar(OWNS_RUN)
        .bind(run)
        .bind(tenant)
        .fetch_one(&b.pool)
        .await
        .map_err(statement_error)?;
    if !owned {
        return Err(StateBackendError::NotFound(run.to_string()));
    }
    Ok(())
}

// ── The pending list ─────────────────────────────────────────────────────────

/// A row's time as the list and its cursor carry it: RFC 3339 in UTC with
/// six fractional digits, so text order is time order and the text reads
/// back to the stored time exactly.
fn cursor_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// The sort key a cursor names. A cursor this list did not make, with a time
/// or a run id that does not parse, is a `Database` error.
fn parse_cursor(c: &ApprovalCursor) -> BackendResult<(DateTime<Utc>, Uuid, &str)> {
    let not_ours = |what: String| {
        StateBackendError::Database(format!("not a cursor of the pending list: {what}"))
    };
    let at = DateTime::parse_from_rfc3339(&c.updated_at)
        .map_err(|e| not_ours(format!("updated_at {:?}: {e}", c.updated_at)))?
        .with_timezone(&Utc);
    let run = Uuid::parse_str(&c.execution_id)
        .map_err(|e| not_ours(format!("execution_id {:?}: {e}", c.execution_id)))?;
    no_nul("cursor node id", &c.node_id)?;
    Ok((at, run, c.node_id.as_str()))
}

fn row_to_pending(row: &PgRow) -> BackendResult<PendingApprovalRow> {
    let context: Option<String> = row.try_get("context_json").map_err(decode)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(decode)?;
    Ok(PendingApprovalRow {
        execution_id: ExecutionId(row.try_get("execution_id").map_err(decode)?),
        workflow_id: row.try_get("workflow_id").map_err(decode)?,
        workflow_version: row.try_get("workflow_version").map_err(decode)?,
        node_id: row.try_get("node_id").map_err(decode)?,
        tool_name: row.try_get("tool_name").map_err(decode)?,
        approver: row.try_get("approver").map_err(decode)?,
        context: context.map(|c| serde_json::from_str(&c)).transpose()?,
        request_sequence: row.try_get("last_sequence").map_err(decode)?,
        updated_at: cursor_time(updated_at),
    })
}

/// Pending rows of runs that are running or paused, by projection time, run
/// and node, after `after`, at most `limit`, read from the
/// `proj_approvals_pending` index in that order. A view lists the rows of
/// its tenant's runs, judged by each run's own row.
pub(super) async fn list_pending_approvals(
    b: &PgBackend,
    after: Option<&ApprovalCursor>,
    limit: u32,
) -> BackendResult<Vec<PendingApprovalRow>> {
    let limit = i64::from(limit);
    let cursor = after.map(parse_cursor).transpose()?;
    let query = match (b.scope(), cursor) {
        (None, None) => sqlx::query(
            "SELECT p.execution_id, e.workflow_id, e.workflow_version, p.node_id, p.tool_name, \
               p.approver, p.context_json, p.last_sequence, p.updated_at \
             FROM proj_approvals p JOIN workflow_executions e ON e.execution_id = p.execution_id \
             WHERE p.status = 'pending' AND e.status IN ('running', 'paused') \
             ORDER BY p.updated_at, p.execution_id, p.node_id COLLATE \"C\" LIMIT $1",
        )
        .bind(limit),
        (Some(tenant), None) => sqlx::query(
            "SELECT p.execution_id, e.workflow_id, e.workflow_version, p.node_id, p.tool_name, \
               p.approver, p.context_json, p.last_sequence, p.updated_at \
             FROM proj_approvals p JOIN workflow_executions e ON e.execution_id = p.execution_id \
             WHERE p.status = 'pending' AND e.status IN ('running', 'paused') \
               AND e.tenant_id = $2 \
             ORDER BY p.updated_at, p.execution_id, p.node_id COLLATE \"C\" LIMIT $1",
        )
        .bind(limit)
        .bind(tenant),
        (None, Some((at, run, node))) => sqlx::query(
            "SELECT p.execution_id, e.workflow_id, e.workflow_version, p.node_id, p.tool_name, \
               p.approver, p.context_json, p.last_sequence, p.updated_at \
             FROM proj_approvals p JOIN workflow_executions e ON e.execution_id = p.execution_id \
             WHERE p.status = 'pending' AND e.status IN ('running', 'paused') \
               AND (p.updated_at, p.execution_id, p.node_id COLLATE \"C\") > ($2, $3, $4) \
             ORDER BY p.updated_at, p.execution_id, p.node_id COLLATE \"C\" LIMIT $1",
        )
        .bind(limit)
        .bind(at)
        .bind(run)
        .bind(node),
        (Some(tenant), Some((at, run, node))) => sqlx::query(
            "SELECT p.execution_id, e.workflow_id, e.workflow_version, p.node_id, p.tool_name, \
               p.approver, p.context_json, p.last_sequence, p.updated_at \
             FROM proj_approvals p JOIN workflow_executions e ON e.execution_id = p.execution_id \
             WHERE p.status = 'pending' AND e.status IN ('running', 'paused') \
               AND e.tenant_id = $2 \
               AND (p.updated_at, p.execution_id, p.node_id COLLATE \"C\") > ($3, $4, $5) \
             ORDER BY p.updated_at, p.execution_id, p.node_id COLLATE \"C\" LIMIT $1",
        )
        .bind(limit)
        .bind(tenant)
        .bind(at)
        .bind(run)
        .bind(node),
    };
    let rows = query.fetch_all(&b.pool).await.map_err(statement_error)?;
    rows.iter().map(row_to_pending).collect()
}
