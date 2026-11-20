//! The work queue and its leases, on Postgres.
//!
//! A claim takes the oldest pending item in one statement, which picks it
//! with `FOR UPDATE SKIP LOCKED` and leases it: claimers never wait on each
//! other and never take one item twice. Before it looks, a claim reclaims
//! the leases that have lapsed, one run at a time, and skips a run or an
//! item another transaction holds ([`expire_stale_leases`]).
//!
//! The times that decide something are the database's
//! (`statement_timestamp()`): when a lease ends, when a retry is due, the
//! order items were queued in. Engines whose clocks disagree still agree on
//! them. The one time a caller gives, a park's `retry_after`, is taken as a
//! delay from this process's clock ([`park_work_item`]).
//!
//! Locks go run before item. A transaction that locks a run does so before
//! it locks any item, and so does every transaction that writes a row
//! naming the run (an event, a dead letter): the foreign key check of such
//! a row takes a share lock on the run's row, and taken after an item lock
//! it could wait on a transaction that holds the run and waits for the
//! item. A claim, a heartbeat, a park, a gate mark and a settle change one
//! item in one statement and lock no run.
//!
//! A fence is the store's term above the item's lease epoch, `term * 2^32 +
//! epoch`, minted in the statement that leases the item, so a failover that
//! raises the term makes every older fence lose.

use super::runs::{
    by_execution, decode, insert_events, lock_run, status_str, to_i32, LockedRun, NewEvent,
};
use super::tx::{no_nul, statement_error, with_tx, TxError};
use super::PgBackend;
use crate::backend::{
    epoch_after_expiry_of, granted_lease_secs, BackendResult, ClaimFilter, FailOutcome, GateMark,
    LeaseRenewal, LeasedItem, OpenWorkItem, ReclaimResult, StateBackendError, WorkItem, WorkItemId,
    DEFAULT_LEASE_SECS,
};
use crate::lease_events::{fail_events, sweep_events, SweepOutcome};
use chrono::{DateTime, Utc};
use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use std::fmt::Write as _;
use uuid::Uuid;

// ── What the bodies share ────────────────────────────────────────────────────

/// A work item's row.
pub(super) fn row_to_item(row: &PgRow) -> BackendResult<WorkItem> {
    let payload: String = row.try_get("payload_json").map_err(decode)?;
    let attempt: i32 = row.try_get("attempt").map_err(decode)?;
    let max_attempts: i32 = row.try_get("max_attempts").map_err(decode)?;
    Ok(WorkItem {
        id: row.try_get("id").map_err(decode)?,
        execution_id: ExecutionId(row.try_get("execution_id").map_err(decode)?),
        node_id: row.try_get("node_id").map_err(decode)?,
        queue_type: row.try_get("queue_type").map_err(decode)?,
        payload: serde_json::from_str(&payload)?,
        attempt: attempt.max(0) as u32,
        max_attempts: max_attempts.max(0) as u32,
        created_at: row.try_get("created_at").map_err(decode)?,
        lease_expires_at: row.try_get("lease_expires_at").map_err(decode)?,
        worker_id: row.try_get("worker_id").map_err(decode)?,
        lease_fence: row.try_get("lease_fence").map_err(decode)?,
        tenant_id: row.try_get("tenant_id").map_err(decode)?,
    })
}

/// The stored names of the statuses a run ends in.
fn ended_statuses() -> Vec<&'static str> {
    use WorkflowStatus::*;
    // Every status a run can have; the ones it ends in are kept.
    [
        Pending,
        Running,
        Paused,
        Completed,
        Failed,
        Cancelled,
        LimitExceeded,
    ]
    .iter()
    .filter(|s| s.is_terminal())
    .map(status_str)
    .collect()
}

/// The wait before the next attempt, in seconds, after a lapse or a failure
/// that leaves `attempt` as the item's attempt: 2^attempt, at most 64. The
/// rule SQLite and the sweep's events use.
fn backoff_secs(attempt: u32) -> u64 {
    1u64 << attempt.min(6)
}

/// A lease length the store holds as `int`: every one it grants is under
/// an hour.
fn lease_secs(granted: u64) -> i32 {
    i32::try_from(granted).unwrap_or(i32::MAX)
}

/// The `released_fence` that records `mark` under `fence`: the fence for a
/// release, its negation while a gate runs (fences are positive), none for
/// undecided.
fn gate_value(mark: GateMark, fence: i64) -> Option<i64> {
    match mark {
        GateMark::Released => Some(fence),
        GateMark::Gating => Some(-fence),
        GateMark::Undecided => None,
    }
}

/// The item's run and tenant, read without a lock: neither ever changes.
/// `None` when there is no such item, or none in `scope`.
async fn item_run(b: &PgBackend, item: WorkItemId) -> BackendResult<Option<(Uuid, String)>> {
    let query = match b.scope() {
        None => sqlx::query_as("SELECT execution_id, tenant_id FROM work_items WHERE id = $1")
            .bind(item),
        Some(tenant) => sqlx::query_as(
            "SELECT execution_id, tenant_id FROM work_items WHERE id = $1 AND tenant_id = $2",
        )
        .bind(item)
        .bind(tenant),
    };
    query.fetch_optional(&b.pool).await.map_err(statement_error)
}

/// Lock the item's run (L1), then the item (L2), which must be claimed
/// under `lease_fence` and, with `lapsed`, past its lease by the database's
/// clock. `None` when the item is not that, or not in `scope`.
async fn lock_claimed(
    conn: &mut PgConnection,
    run: Uuid,
    item: WorkItemId,
    lease_fence: i64,
    lapsed: bool,
    scope: Option<&str>,
) -> Result<Option<(LockedRun, WorkItem)>, TxError> {
    let Some(locked) = lock_run(&mut *conn, run, scope).await? else {
        return Ok(None);
    };
    let select = match (lapsed, scope.is_some()) {
        (false, false) => {
            "SELECT * FROM work_items WHERE id = $1 AND lease_fence = $2 AND status = 'claimed' \
             FOR UPDATE"
        }
        (true, false) => {
            "SELECT * FROM work_items WHERE id = $1 AND lease_fence = $2 AND status = 'claimed' \
             AND lease_expires_at < statement_timestamp() FOR UPDATE"
        }
        (false, true) => {
            "SELECT * FROM work_items WHERE id = $1 AND lease_fence = $2 AND status = 'claimed' \
             AND tenant_id = $3 FOR UPDATE"
        }
        (true, true) => {
            "SELECT * FROM work_items WHERE id = $1 AND lease_fence = $2 AND status = 'claimed' \
             AND lease_expires_at < statement_timestamp() AND tenant_id = $3 FOR UPDATE"
        }
    };
    let mut query = sqlx::query(select).bind(item).bind(lease_fence);
    if let Some(tenant) = scope {
        query = query.bind(tenant);
    }
    let Some(row) = query.fetch_optional(&mut *conn).await? else {
        return Ok(None);
    };
    Ok(Some((locked, row_to_item(&row)?)))
}

// The writes below follow `lock_claimed`: the caller holds the item's lock
// and has checked its state, and `tenant` is the item's own.

/// Settle the item of a run that has ended: failed, nothing recorded.
pub(super) async fn settle_failed(
    conn: &mut PgConnection,
    item: WorkItemId,
    tenant: &str,
) -> Result<(), TxError> {
    sqlx::query(
        "UPDATE work_items SET status = 'failed', worker_id = NULL, lease_expires_at = NULL \
         WHERE id = $1 AND tenant_id = $2",
    )
    .bind(item)
    .bind(tenant)
    .execute(conn)
    .await?;
    Ok(())
}

/// Settle the item as dead-lettered at its final `attempt`.
async fn settle_dead_lettered(
    conn: &mut PgConnection,
    item: WorkItemId,
    tenant: &str,
    attempt: u32,
) -> Result<(), TxError> {
    sqlx::query(
        "UPDATE work_items SET status = 'dead_lettered', attempt = $3, lease_expires_at = NULL, \
         worker_id = NULL WHERE id = $1 AND tenant_id = $2",
    )
    .bind(item)
    .bind(tenant)
    .bind(to_i32("attempt", attempt)?)
    .execute(conn)
    .await?;
    Ok(())
}

/// Who a requeue remembers as the item's last holder, so its heartbeat can
/// take the lease back: the holder of a lease that lapsed, not a worker
/// that reported a failure.
#[derive(Clone, Copy)]
enum Holder {
    Remember,
    Forget,
}

/// Put the item back to pending at `attempt`, due `backoff` seconds from
/// now by the database's clock, under a fresh epoch and no fence.
async fn requeue(
    conn: &mut PgConnection,
    item: WorkItemId,
    tenant: &str,
    attempt: u32,
    backoff: u64,
    holder: Holder,
) -> Result<(), TxError> {
    let update = match holder {
        Holder::Remember => {
            "UPDATE work_items SET status = 'pending', attempt = $3, last_worker_id = worker_id, \
             last_lease_fence = lease_fence, worker_id = NULL, lease_expires_at = NULL, \
             retry_after = statement_timestamp() + make_interval(secs => $4), \
             lease_epoch = lease_epoch + 1, lease_fence = 0 WHERE id = $1 AND tenant_id = $2"
        }
        Holder::Forget => {
            "UPDATE work_items SET status = 'pending', attempt = $3, worker_id = NULL, \
             lease_expires_at = NULL, \
             retry_after = statement_timestamp() + make_interval(secs => $4), \
             lease_epoch = lease_epoch + 1, lease_fence = 0 WHERE id = $1 AND tenant_id = $2"
        }
    };
    sqlx::query(update)
        .bind(item)
        .bind(tenant)
        .bind(to_i32("attempt", attempt)?)
        .bind(backoff as i32)
        .execute(conn)
        .await?;
    Ok(())
}

/// The item's dead-letter row, copied from the item as it now stands, with
/// `last_error`. A row already there is kept.
async fn insert_dead_letter(
    conn: &mut PgConnection,
    item: WorkItemId,
    tenant: &str,
    last_error: &str,
) -> Result<(), TxError> {
    sqlx::query(
        "INSERT INTO dead_letter_items (id, execution_id, tenant_id, node_id, queue_type, \
           payload_json, attempt, last_error, created_at, dead_lettered_at) \
         SELECT id, execution_id, tenant_id, node_id, queue_type, payload_json, attempt, $3, \
                created_at, statement_timestamp() \
         FROM work_items WHERE id = $1 AND tenant_id = $2 \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(item)
    .bind(tenant)
    .bind(last_error)
    .execute(conn)
    .await?;
    Ok(())
}

/// The rows for `kinds`, made by the store now.
fn made(kinds: &[crate::event::EventKind]) -> BackendResult<Vec<NewEvent>> {
    kinds.iter().map(NewEvent::made).collect()
}

// ── Enqueue and claim ────────────────────────────────────────────────────────

/// One statement, which takes the run's tenant from the run's row: an item
/// for a run that is not there, or not the view's, gets no row and is
/// `NotFound`. `by_execution` is filled from the payload's `claim`.
pub(super) async fn enqueue_work_item(b: &PgBackend, item: WorkItem) -> BackendResult<WorkItemId> {
    no_nul("node id", &item.node_id)?;
    no_nul("queue type", &item.queue_type)?;
    let payload = serde_json::to_string(&item.payload)?;
    let query = match b.scope() {
        None => sqlx::query(
            "INSERT INTO work_items (id, execution_id, tenant_id, node_id, queue_type, \
               payload_json, by_execution, attempt, max_attempts, status, created_at) \
             SELECT $1, r.execution_id, r.tenant_id, $3, $4, $5, $6, $7, $8, 'pending', $9 \
             FROM workflow_executions r WHERE r.execution_id = $2",
        ),
        Some(_) => sqlx::query(
            "INSERT INTO work_items (id, execution_id, tenant_id, node_id, queue_type, \
               payload_json, by_execution, attempt, max_attempts, status, created_at) \
             SELECT $1, r.execution_id, r.tenant_id, $3, $4, $5, $6, $7, $8, 'pending', $9 \
             FROM workflow_executions r WHERE r.execution_id = $2 AND r.tenant_id = $10",
        ),
    };
    let mut query = query
        .bind(item.id)
        .bind(item.execution_id.0)
        .bind(&item.node_id)
        .bind(&item.queue_type)
        .bind(&payload)
        .bind(by_execution(&item.payload))
        .bind(to_i32("attempt", item.attempt)?)
        .bind(to_i32("max_attempts", item.max_attempts)?)
        .bind(item.created_at);
    if let Some(tenant) = b.scope() {
        query = query.bind(tenant);
    }
    let inserted = query
        .execute(&b.pool)
        .await
        .map_err(statement_error)?
        .rows_affected();
    if inserted == 0 {
        return Err(StateBackendError::NotFound(item.execution_id.0.to_string()));
    }
    Ok(item.id)
}

/// The claim: the fast reclaim for this view first, then the oldest pending
/// item on the queues that is due by the database's clock, picked with
/// `FOR UPDATE SKIP LOCKED` and leased in the same statement.
pub(super) async fn claim_work_item_filtered(
    b: &PgBackend,
    worker_id: &str,
    queue_types: &[&str],
    lease_secs_asked: Option<u64>,
    filter: &ClaimFilter,
) -> BackendResult<Option<WorkItem>> {
    if queue_types.is_empty() {
        return Ok(None);
    }
    no_nul("worker id", worker_id)?;
    expire_stale_leases(b).await?;
    let sql = claim_sql(
        filter.execution_id.is_some(),
        filter.node_id.is_some(),
        b.scope().is_some(),
    );
    // Built from the literals below, chosen by which filters are given;
    // every value is bound.
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(queue_types)
        .bind(worker_id)
        .bind(lease_secs(granted_lease_secs(lease_secs_asked)));
    if let Some(run) = &filter.execution_id {
        query = query.bind(run.0);
    }
    if let Some(node) = &filter.node_id {
        query = query.bind(node.as_str());
    }
    if let Some(tenant) = b.scope() {
        query = query.bind(tenant);
    }
    let row = query
        .fetch_optional(&b.pool)
        .await
        .map_err(statement_error)?;
    row.map(|r| row_to_item(&r)).transpose()
}

/// The claim's statement. Binds `$1` the queues, `$2` the worker, `$3` the
/// lease's seconds, then the run, the node and the tenant, each only when
/// its clause is in, in that order. A claim that names a run may take an
/// item marked claimable only by its run, and marks the new lease as gating
/// in the same statement; a claim that names none never takes such an item.
/// The text is built from literal fragments chosen by condition, never
/// `$n IS NULL OR ...`, so the planner can use `work_items_claim`.
fn claim_sql(by_run: bool, by_node: bool, scoped: bool) -> String {
    let mut filters = String::new();
    let mut n = 3;
    if by_run {
        n += 1;
        let _ = write!(filters, " AND execution_id = ${n}");
    } else {
        filters.push_str(" AND NOT by_execution");
    }
    if by_node {
        n += 1;
        let _ = write!(filters, " AND node_id = ${n}");
    }
    if scoped {
        n += 1;
        let _ = write!(filters, " AND tenant_id = ${n}");
    }
    let gating = if by_run {
        ", released_fence = -((SELECT term FROM store_identity WHERE id = 1) * 4294967296 \
         + w.lease_epoch + 1)"
    } else {
        ""
    };
    format!(
        "WITH cand AS ( \
           SELECT id FROM work_items \
           WHERE status = 'pending' AND queue_type = ANY($1) \
             AND (retry_after IS NULL OR retry_after <= statement_timestamp()){filters} \
           ORDER BY enqueued_at, id \
           LIMIT 1 \
           FOR UPDATE SKIP LOCKED) \
         UPDATE work_items w \
         SET status = 'claimed', worker_id = $2, claimed_at = statement_timestamp(), \
             lease_secs = $3, lease_expires_at = statement_timestamp() + make_interval(secs => $3), \
             lease_epoch = w.lease_epoch + 1, \
             lease_fence = (SELECT term FROM store_identity WHERE id = 1) * 4294967296 \
               + w.lease_epoch + 1{gating} \
         FROM cand WHERE w.id = cand.id \
         RETURNING w.*"
    )
}

// ── The fast reclaim ─────────────────────────────────────────────────────────

/// Reclaim this view's lapsed leases, as a claim does before it looks
/// (rule L6). A read without locks finds the runs with a lease that lapsed
/// by the database's clock; then each run, in id order, in a transaction of
/// its own: its row locked with `SKIP LOCKED`, so a run another
/// transaction holds is left for the next poll and the claim never waits.
/// In a run that has ended, the lapsed items are settled failed and
/// nothing is recorded. Otherwise each lapsed item with an attempt left is
/// requeued at once, its holder remembered for a re-attach, and the lost
/// lease and the retry are recorded. An item out of attempts is left
/// claimed for the sweep, which dead-letters it. Items another transaction
/// holds are skipped too.
async fn expire_stale_leases(b: &PgBackend) -> BackendResult<()> {
    let ended = ended_statuses();
    let query = match b.scope() {
        None => sqlx::query_scalar(
            "SELECT DISTINCT w.execution_id FROM work_items w \
             JOIN workflow_executions e ON e.execution_id = w.execution_id \
             WHERE w.status = 'claimed' AND w.lease_expires_at < statement_timestamp() \
             AND (w.attempt + 1 < w.max_attempts OR e.status = ANY($1)) \
             ORDER BY w.execution_id",
        )
        .bind(&ended),
        Some(tenant) => sqlx::query_scalar(
            "SELECT DISTINCT w.execution_id FROM work_items w \
             JOIN workflow_executions e ON e.execution_id = w.execution_id \
             WHERE w.status = 'claimed' AND w.lease_expires_at < statement_timestamp() \
             AND (w.attempt + 1 < w.max_attempts OR e.status = ANY($1)) AND w.tenant_id = $2 \
             ORDER BY w.execution_id",
        )
        .bind(&ended)
        .bind(tenant),
    };
    let runs: Vec<Uuid> = query.fetch_all(&b.pool).await.map_err(statement_error)?;
    for run in runs {
        expire_stale_leases_of(b, run).await?;
    }
    Ok(())
}

/// The fast reclaim of one run, in one transaction.
async fn expire_stale_leases_of(b: &PgBackend, run: Uuid) -> BackendResult<()> {
    let scope = b.scope();
    with_tx(&b.pool, "expire_stale_leases", |tx| {
        Box::pin(async move {
            let locked = try_lock_run(tx, run, scope).await?;
            fault_point!("pg.expire_stale_leases", 1);
            let Some(locked) = locked else {
                // Another transaction holds the run: the next poll.
                return Ok(());
            };
            if locked.status.is_terminal() {
                let settle = match scope {
                    None => sqlx::query(
                        "WITH lapsed AS (SELECT id FROM work_items WHERE execution_id = $1 \
                           AND status = 'claimed' AND lease_expires_at < statement_timestamp() \
                           ORDER BY id FOR UPDATE SKIP LOCKED) \
                         UPDATE work_items w SET status = 'failed', worker_id = NULL, \
                           lease_expires_at = NULL \
                         FROM lapsed WHERE w.id = lapsed.id",
                    )
                    .bind(run),
                    Some(tenant) => sqlx::query(
                        "WITH lapsed AS (SELECT id FROM work_items WHERE execution_id = $1 \
                           AND status = 'claimed' AND lease_expires_at < statement_timestamp() \
                           AND tenant_id = $2 ORDER BY id FOR UPDATE SKIP LOCKED) \
                         UPDATE work_items w SET status = 'failed', worker_id = NULL, \
                           lease_expires_at = NULL \
                         FROM lapsed WHERE w.id = lapsed.id",
                    )
                    .bind(run)
                    .bind(tenant),
                };
                settle.execute(&mut **tx).await?;
                fault_point!("pg.expire_stale_leases", 2);
            } else {
                let requeue = match scope {
                    None => sqlx::query(
                        "WITH lapsed AS (SELECT id FROM work_items WHERE execution_id = $1 \
                           AND status = 'claimed' AND lease_expires_at < statement_timestamp() \
                           AND attempt + 1 < max_attempts ORDER BY id FOR UPDATE SKIP LOCKED) \
                         UPDATE work_items w SET status = 'pending', \
                           last_worker_id = w.worker_id, last_lease_fence = w.lease_fence, \
                           worker_id = NULL, lease_expires_at = NULL, \
                           lease_epoch = w.lease_epoch + 1, lease_fence = 0, \
                           attempt = w.attempt + 1 \
                         FROM lapsed WHERE w.id = lapsed.id RETURNING w.*",
                    )
                    .bind(run),
                    Some(tenant) => sqlx::query(
                        "WITH lapsed AS (SELECT id FROM work_items WHERE execution_id = $1 \
                           AND status = 'claimed' AND lease_expires_at < statement_timestamp() \
                           AND attempt + 1 < max_attempts AND tenant_id = $2 \
                           ORDER BY id FOR UPDATE SKIP LOCKED) \
                         UPDATE work_items w SET status = 'pending', \
                           last_worker_id = w.worker_id, last_lease_fence = w.lease_fence, \
                           worker_id = NULL, lease_expires_at = NULL, \
                           lease_epoch = w.lease_epoch + 1, lease_fence = 0, \
                           attempt = w.attempt + 1 \
                         FROM lapsed WHERE w.id = lapsed.id RETURNING w.*",
                    )
                    .bind(run)
                    .bind(tenant),
                };
                let rows = requeue.fetch_all(&mut **tx).await?;
                fault_point!("pg.expire_stale_leases", 3);
                let mut requeued = rows
                    .iter()
                    .map(row_to_item)
                    .collect::<BackendResult<Vec<_>>>()?;
                requeued.sort_by_key(|item| item.id);
                let mut kinds = Vec::new();
                for mut item in requeued {
                    // The trail names the attempt that held the lease.
                    item.attempt = item.attempt.saturating_sub(1);
                    kinds.extend(crate::sqlite::lease_expiry_events(&item));
                }
                if !kinds.is_empty() {
                    insert_events(tx, run, &locked.tenant, &made(&kinds)?).await?;
                }
                fault_point!("pg.expire_stale_leases", 4);
            }
            fault_point!("pg.expire_stale_leases", 5);
            Ok(())
        })
    })
    .await
}

/// L1 with `SKIP LOCKED`: the run's row, or `None` when there is no such
/// run (in `scope`) or another transaction holds it.
async fn try_lock_run(
    conn: &mut PgConnection,
    run: Uuid,
    scope: Option<&str>,
) -> Result<Option<LockedRun>, TxError> {
    let row = match scope {
        None => {
            sqlx::query(
                "SELECT status, tenant_id FROM workflow_executions \
                 WHERE execution_id = $1 FOR UPDATE SKIP LOCKED",
            )
            .bind(run)
            .fetch_optional(conn)
            .await?
        }
        Some(tenant) => {
            sqlx::query(
                "SELECT status, tenant_id FROM workflow_executions \
                 WHERE execution_id = $1 AND tenant_id = $2 FOR UPDATE SKIP LOCKED",
            )
            .bind(run)
            .bind(tenant)
            .fetch_optional(conn)
            .await?
        }
    };
    let Some(row) = row else { return Ok(None) };
    Ok(Some(LockedRun {
        status: super::runs::parse_status(row.try_get("status").map_err(decode)?)?,
        tenant: row.try_get("tenant_id").map_err(decode)?,
    }))
}

// ── Leases held ──────────────────────────────────────────────────────────────

/// The optional clauses of a read of one run's items, numbered from
/// `$next`: the node, then the tenant, each only when present. The caller
/// binds in the same order.
fn node_and_tenant(mut next: u32, by_node: bool, scoped: bool) -> String {
    let mut clauses = String::new();
    if by_node {
        let _ = write!(clauses, " AND node_id = ${next}");
        next += 1;
    }
    if scoped {
        let _ = write!(clauses, " AND tenant_id = ${next}");
    }
    clauses
}

/// The run's items on the queues whose lease has not lapsed by the
/// database's clock, with their lease length and gate mark.
pub(super) async fn leased_items(
    b: &PgBackend,
    queue_types: &[&str],
    filter: &ClaimFilter,
) -> BackendResult<Vec<LeasedItem>> {
    let Some(run) = &filter.execution_id else {
        return Ok(Vec::new());
    };
    if queue_types.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT *, CASE WHEN released_fence = lease_fence THEN 2 \
                        WHEN released_fence = -lease_fence THEN 1 ELSE 0 END AS gate_mark \
         FROM work_items WHERE status = 'claimed' AND queue_type = ANY($1) \
         AND execution_id = $2 AND lease_expires_at > statement_timestamp(){}",
        node_and_tenant(3, filter.node_id.is_some(), b.scope().is_some())
    );
    // Built from literals chosen by which filters are given; values bound.
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(queue_types)
        .bind(run.0);
    if let Some(node) = &filter.node_id {
        query = query.bind(node.as_str());
    }
    if let Some(tenant) = b.scope() {
        query = query.bind(tenant);
    }
    let rows = query.fetch_all(&b.pool).await.map_err(statement_error)?;
    rows.iter()
        .map(|row| {
            let lease: Option<i32> = row.try_get("lease_secs").map_err(decode)?;
            let gate: i32 = row.try_get("gate_mark").map_err(decode)?;
            Ok(LeasedItem {
                item: row_to_item(row)?,
                lease_secs: lease
                    .map(|s| s.max(0) as u64)
                    .unwrap_or_else(|| granted_lease_secs(None)),
                gate: match gate {
                    2 => GateMark::Released,
                    1 => GateMark::Gating,
                    _ => GateMark::Undecided,
                },
            })
        })
        .collect()
}

/// Whether the run has an item on the queues that is claimed, or pending
/// and due by the database's clock. A read, no lock.
pub(super) async fn has_claimable(
    b: &PgBackend,
    queue_types: &[&str],
    filter: &ClaimFilter,
) -> BackendResult<bool> {
    let Some(run) = &filter.execution_id else {
        return Ok(true);
    };
    if queue_types.is_empty() {
        return Ok(false);
    }
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM work_items WHERE queue_type = ANY($1) \
         AND execution_id = $2{} AND (status = 'claimed' OR (status = 'pending' \
         AND (retry_after IS NULL OR retry_after <= statement_timestamp()))))",
        node_and_tenant(3, filter.node_id.is_some(), b.scope().is_some())
    );
    // Built from literals chosen by which filters are given; values bound.
    let mut query = sqlx::query_scalar::<_, bool>(sqlx::AssertSqlSafe(sql))
        .bind(queue_types)
        .bind(run.0);
    if let Some(node) = &filter.node_id {
        query = query.bind(node.as_str());
    }
    if let Some(tenant) = b.scope() {
        query = query.bind(tenant);
    }
    query.fetch_one(&b.pool).await.map_err(statement_error)
}

/// Record the gate's mark under `lease_fence`, in one statement.
pub(super) async fn mark_gate(
    b: &PgBackend,
    item: WorkItemId,
    lease_fence: i64,
    mark: GateMark,
) -> BackendResult<bool> {
    let query = match b.scope() {
        None => sqlx::query(
            "UPDATE work_items SET released_fence = $1 \
             WHERE id = $2 AND lease_fence = $3 AND status = 'claimed'",
        ),
        Some(_) => sqlx::query(
            "UPDATE work_items SET released_fence = $1 \
             WHERE id = $2 AND lease_fence = $3 AND status = 'claimed' AND tenant_id = $4",
        ),
    };
    let mut query = query
        .bind(gate_value(mark, lease_fence))
        .bind(item)
        .bind(lease_fence);
    if let Some(tenant) = b.scope() {
        query = query.bind(tenant);
    }
    let done = query.execute(&b.pool).await.map_err(statement_error)?;
    Ok(done.rows_affected() == 1)
}

/// Move the gate's mark from `from` to `to`, only while it is `from`, and
/// with `renew` (the lease's seconds) only while the lease has not lapsed
/// by the database's clock, which it renews: one statement.
pub(super) async fn swap_gate(
    b: &PgBackend,
    item: WorkItemId,
    lease_fence: i64,
    from: GateMark,
    to: GateMark,
    renew: Option<u64>,
) -> BackendResult<bool> {
    // $1 the new mark, $2 the item, $3 the fence; then the mark it must be
    // now, the lease's seconds and the tenant, each only when its clause is in.
    let mut n = 3;
    let mut set = String::from("released_fence = $1");
    let mut clauses = String::new();
    match from {
        // Undecided is any value that is neither mark of this fence: none,
        // or one an earlier lease left.
        GateMark::Undecided => clauses.push_str(
            " AND (released_fence IS NULL OR (released_fence <> $3 AND released_fence <> -$3))",
        ),
        _ => {
            n += 1;
            let _ = write!(clauses, " AND released_fence = ${n}");
        }
    }
    if renew.is_some() {
        n += 1;
        let _ = write!(
            set,
            ", lease_expires_at = statement_timestamp() + make_interval(secs => ${n})"
        );
        clauses.push_str(" AND lease_expires_at > statement_timestamp()");
    }
    if b.scope().is_some() {
        n += 1;
        let _ = write!(clauses, " AND tenant_id = ${n}");
    }
    let sql = format!(
        "UPDATE work_items SET {set} \
         WHERE id = $2 AND lease_fence = $3 AND status = 'claimed'{clauses}"
    );
    // Built from literals chosen by the marks and the renewal; values bound.
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(gate_value(to, lease_fence))
        .bind(item)
        .bind(lease_fence);
    if from != GateMark::Undecided {
        query = query.bind(gate_value(from, lease_fence));
    }
    if let Some(secs) = renew {
        query = query.bind(lease_secs(granted_lease_secs(Some(secs))));
    }
    if let Some(tenant) = b.scope() {
        query = query.bind(tenant);
    }
    let done = query.execute(&b.pool).await.map_err(statement_error)?;
    Ok(done.rows_affected() == 1)
}

/// Extend a live lease by the length it was granted, from now by the
/// database's clock. `FenceLost` unless `worker_id` holds it under
/// `lease_fence` and it has not lapsed.
///
/// A lease that lapsed by the database's clock is over, even before a
/// reclaim has taken it back: its holder gets the item again only by
/// re-attaching once the reclaim has requeued it ([`heartbeat_lease`]), as
/// a gate's release already refuses a lapsed lease. SQLite renews a lapsed
/// lease that nobody has reclaimed yet.
pub(super) async fn renew_lease(
    b: &PgBackend,
    item: WorkItemId,
    worker_id: &str,
    lease_fence: i64,
) -> BackendResult<()> {
    no_nul("worker id", worker_id)?;
    let query = match b.scope() {
        None => sqlx::query(
            "UPDATE work_items SET lease_expires_at = statement_timestamp() \
               + make_interval(secs => COALESCE(lease_secs, $4)) \
             WHERE id = $1 AND worker_id = $2 AND status = 'claimed' AND lease_fence = $3 \
             AND lease_expires_at > statement_timestamp()",
        ),
        Some(_) => sqlx::query(
            "UPDATE work_items SET lease_expires_at = statement_timestamp() \
               + make_interval(secs => COALESCE(lease_secs, $4)) \
             WHERE id = $1 AND worker_id = $2 AND status = 'claimed' AND lease_fence = $3 \
             AND lease_expires_at > statement_timestamp() AND tenant_id = $5",
        ),
    };
    let mut query = query
        .bind(item)
        .bind(worker_id)
        .bind(lease_fence)
        .bind(lease_secs(DEFAULT_LEASE_SECS));
    if let Some(tenant) = b.scope() {
        query = query.bind(tenant);
    }
    let done = query.execute(&b.pool).await.map_err(statement_error)?;
    if done.rows_affected() == 0 {
        return Err(StateBackendError::FenceLost(item.to_string()));
    }
    Ok(())
}

/// Renew, or re-attach a lease lost only to expiry while nobody claimed the
/// item since.
pub(super) async fn heartbeat_lease(
    b: &PgBackend,
    item: WorkItemId,
    worker_id: &str,
    lease_fence: i64,
) -> BackendResult<LeaseRenewal> {
    match renew_lease(b, item, worker_id, lease_fence).await {
        Ok(()) => Ok(LeaseRenewal {
            lease_fence,
            reattached: false,
        }),
        Err(StateBackendError::FenceLost(_)) => {
            reattach_lease(b, item, worker_id, lease_fence).await
        }
        Err(e) => Err(e),
    }
}

/// Give the lease lost as `lost_fence` back to its worker, in one
/// statement: only while the item is pending from that expiry, nobody has
/// claimed it since (its epoch is the one the expiry left), and the worker
/// is the one that held it. The new fence takes the store's term now, the
/// attempt the expiry charged is given back, and a release under the lost
/// fence carries over to the new one.
async fn reattach_lease(
    b: &PgBackend,
    item: WorkItemId,
    worker_id: &str,
    lost_fence: i64,
) -> BackendResult<LeaseRenewal> {
    let query = match b.scope() {
        None => sqlx::query_scalar(
            "UPDATE work_items SET status = 'claimed', worker_id = $2, \
               claimed_at = statement_timestamp(), \
               lease_expires_at = statement_timestamp() \
                 + make_interval(secs => COALESCE(lease_secs, $5)), \
               lease_epoch = lease_epoch + 1, \
               lease_fence = (SELECT term FROM store_identity WHERE id = 1) * 4294967296 \
                 + lease_epoch + 1, \
               attempt = CASE WHEN attempt > 0 THEN attempt - 1 ELSE 0 END, \
               last_worker_id = NULL, last_lease_fence = NULL, \
               released_fence = CASE WHEN released_fence = $3 \
                 THEN (SELECT term FROM store_identity WHERE id = 1) * 4294967296 \
                   + lease_epoch + 1 \
                 ELSE released_fence END \
             WHERE id = $1 AND status = 'pending' AND worker_id IS NULL \
             AND last_worker_id = $2 AND last_lease_fence = $3 AND lease_epoch = $4 \
             RETURNING lease_fence",
        ),
        Some(_) => sqlx::query_scalar(
            "UPDATE work_items SET status = 'claimed', worker_id = $2, \
               claimed_at = statement_timestamp(), \
               lease_expires_at = statement_timestamp() \
                 + make_interval(secs => COALESCE(lease_secs, $5)), \
               lease_epoch = lease_epoch + 1, \
               lease_fence = (SELECT term FROM store_identity WHERE id = 1) * 4294967296 \
                 + lease_epoch + 1, \
               attempt = CASE WHEN attempt > 0 THEN attempt - 1 ELSE 0 END, \
               last_worker_id = NULL, last_lease_fence = NULL, \
               released_fence = CASE WHEN released_fence = $3 \
                 THEN (SELECT term FROM store_identity WHERE id = 1) * 4294967296 \
                   + lease_epoch + 1 \
                 ELSE released_fence END \
             WHERE id = $1 AND status = 'pending' AND worker_id IS NULL \
             AND last_worker_id = $2 AND last_lease_fence = $3 AND lease_epoch = $4 \
             AND tenant_id = $6 \
             RETURNING lease_fence",
        ),
    };
    let mut query = query
        .bind(item)
        .bind(worker_id)
        .bind(lost_fence)
        .bind(epoch_after_expiry_of(lost_fence))
        .bind(lease_secs(DEFAULT_LEASE_SECS));
    if let Some(tenant) = b.scope() {
        query = query.bind(tenant);
    }
    let fence: Option<i64> = query
        .fetch_optional(&b.pool)
        .await
        .map_err(statement_error)?;
    match fence {
        Some(lease_fence) => Ok(LeaseRenewal {
            lease_fence,
            reattached: true,
        }),
        None => Err(StateBackendError::FenceLost(item.to_string())),
    }
}

// ── Reading and settling one item ────────────────────────────────────────────

pub(super) async fn get_work_item(
    b: &PgBackend,
    item: WorkItemId,
) -> BackendResult<Option<WorkItem>> {
    let query = match b.scope() {
        None => sqlx::query("SELECT * FROM work_items WHERE id = $1").bind(item),
        Some(tenant) => sqlx::query("SELECT * FROM work_items WHERE id = $1 AND tenant_id = $2")
            .bind(item)
            .bind(tenant),
    };
    let row = query
        .fetch_optional(&b.pool)
        .await
        .map_err(statement_error)?;
    row.map(|r| row_to_item(&r)).transpose()
}

/// Unfenced: `NotFound` when there is no such item (in this view).
pub(super) async fn complete_work_item(b: &PgBackend, item: WorkItemId) -> BackendResult<()> {
    let query = match b.scope() {
        None => sqlx::query(
            "UPDATE work_items SET status = 'completed', completed_at = statement_timestamp(), \
             lease_expires_at = NULL WHERE id = $1",
        )
        .bind(item),
        Some(tenant) => sqlx::query(
            "UPDATE work_items SET status = 'completed', completed_at = statement_timestamp(), \
             lease_expires_at = NULL WHERE id = $1 AND tenant_id = $2",
        )
        .bind(item)
        .bind(tenant),
    };
    let done = query.execute(&b.pool).await.map_err(statement_error)?;
    if done.rows_affected() == 0 {
        return Err(StateBackendError::NotFound(item.to_string()));
    }
    Ok(())
}

/// Settles only the holder of a still-claimed item under `lease_fence`.
pub(super) async fn complete_work_item_fenced(
    b: &PgBackend,
    item: WorkItemId,
    lease_fence: i64,
) -> BackendResult<bool> {
    let query = match b.scope() {
        None => sqlx::query(
            "UPDATE work_items SET status = 'completed', completed_at = statement_timestamp(), \
             lease_expires_at = NULL WHERE id = $1 AND lease_fence = $2 AND status = 'claimed'",
        )
        .bind(item)
        .bind(lease_fence),
        Some(tenant) => sqlx::query(
            "UPDATE work_items SET status = 'completed', completed_at = statement_timestamp(), \
             lease_expires_at = NULL WHERE id = $1 AND lease_fence = $2 AND status = 'claimed' \
             AND tenant_id = $3",
        )
        .bind(item)
        .bind(lease_fence)
        .bind(tenant),
    };
    let done = query.execute(&b.pool).await.map_err(statement_error)?;
    Ok(done.rows_affected() > 0)
}

/// Unfenced, and records nothing: `NotFound` when there is no such item.
pub(super) async fn fail_work_item(b: &PgBackend, item: WorkItemId) -> BackendResult<()> {
    let query = match b.scope() {
        None => sqlx::query(
            "UPDATE work_items SET status = 'failed', lease_expires_at = NULL, worker_id = NULL \
             WHERE id = $1",
        )
        .bind(item),
        Some(tenant) => sqlx::query(
            "UPDATE work_items SET status = 'failed', lease_expires_at = NULL, worker_id = NULL \
             WHERE id = $1 AND tenant_id = $2",
        )
        .bind(item)
        .bind(tenant),
    };
    let done = query.execute(&b.pool).await.map_err(statement_error)?;
    if done.rows_affected() == 0 {
        return Err(StateBackendError::NotFound(item.to_string()));
    }
    Ok(())
}

/// Settles a claimed item failed with nothing recorded, fenced.
pub(super) async fn withdraw_work_item_fenced(
    b: &PgBackend,
    item: WorkItemId,
    lease_fence: i64,
) -> BackendResult<bool> {
    let query = match b.scope() {
        None => sqlx::query(
            "UPDATE work_items SET status = 'failed', worker_id = NULL, lease_expires_at = NULL \
             WHERE id = $1 AND lease_fence = $2 AND status = 'claimed'",
        )
        .bind(item)
        .bind(lease_fence),
        Some(tenant) => sqlx::query(
            "UPDATE work_items SET status = 'failed', worker_id = NULL, lease_expires_at = NULL \
             WHERE id = $1 AND lease_fence = $2 AND status = 'claimed' AND tenant_id = $3",
        )
        .bind(item)
        .bind(lease_fence)
        .bind(tenant),
    };
    let done = query.execute(&b.pool).await.map_err(statement_error)?;
    Ok(done.rows_affected() > 0)
}

/// How close the two clocks must be for a park to keep the caller's time
/// as it is.
const CLOCKS_AGREE_WITHIN_SECS: f64 = 1.0;

/// Back to pending at `next_attempt`, due at `retry_after`, under a fresh
/// epoch and no fence, in one fenced statement.
///
/// `retry_after` is a time on the caller's clock. The store keeps it as
/// given while this process's clock and the database's agree to within a
/// second, so a park reads back what was written. When they are further
/// apart, it is taken as a delay from this process's clock and applied to
/// the database's: `statement_timestamp() + (retry_after - process_now)`.
/// Either way the item comes due within a second of the delay the caller
/// meant, by the database's clock that the claim reads.
pub(super) async fn park_work_item(
    b: &PgBackend,
    item: WorkItemId,
    lease_fence: i64,
    retry_after: &str,
    next_attempt: u32,
) -> BackendResult<bool> {
    let due = DateTime::parse_from_rfc3339(retry_after)
        .map_err(|e| {
            StateBackendError::Database(format!("invalid retry_after {retry_after:?}: {e}"))
        })?
        .with_timezone(&Utc);
    let query = match b.scope() {
        None => sqlx::query(
            "UPDATE work_items SET status = 'pending', attempt = $3, worker_id = NULL, \
               lease_expires_at = NULL, lease_epoch = lease_epoch + 1, lease_fence = 0, \
               retry_after = CASE \
                 WHEN abs(extract(epoch FROM statement_timestamp() - $5::timestamptz))::float8 < $6 \
                 THEN $4::timestamptz \
                 ELSE statement_timestamp() + ($4::timestamptz - $5::timestamptz) END \
             WHERE id = $1 AND lease_fence = $2 AND status = 'claimed'",
        ),
        Some(_) => sqlx::query(
            "UPDATE work_items SET status = 'pending', attempt = $3, worker_id = NULL, \
               lease_expires_at = NULL, lease_epoch = lease_epoch + 1, lease_fence = 0, \
               retry_after = CASE \
                 WHEN abs(extract(epoch FROM statement_timestamp() - $5::timestamptz))::float8 < $6 \
                 THEN $4::timestamptz \
                 ELSE statement_timestamp() + ($4::timestamptz - $5::timestamptz) END \
             WHERE id = $1 AND lease_fence = $2 AND status = 'claimed' AND tenant_id = $7",
        ),
    };
    let mut query = query
        .bind(item)
        .bind(lease_fence)
        .bind(to_i32("attempt", next_attempt)?)
        .bind(due)
        .bind(crate::clock::process_now())
        .bind(CLOCKS_AGREE_WITHIN_SECS);
    if let Some(tenant) = b.scope() {
        query = query.bind(tenant);
    }
    let done = query.execute(&b.pool).await.map_err(statement_error)?;
    Ok(done.rows_affected() > 0)
}

/// Back to pending now, same attempt, fenced as a park, with the payload
/// (when it is an object) and the item marked claimable only by its run.
/// The payload is read under the item's lock, marked here, and written back
/// in the same transaction.
pub(super) async fn park_for_execution(
    b: &PgBackend,
    item: WorkItemId,
    lease_fence: i64,
) -> BackendResult<bool> {
    let scope = b.scope();
    with_tx(&b.pool, "park_for_execution", |tx| {
        Box::pin(async move {
            let payload: Option<String> =
                match scope {
                    None => {
                        sqlx::query_scalar(
                            "SELECT payload_json FROM work_items \
                         WHERE id = $1 AND lease_fence = $2 AND status = 'claimed' FOR UPDATE",
                        )
                        .bind(item)
                        .bind(lease_fence)
                        .fetch_optional(&mut **tx)
                        .await?
                    }
                    Some(tenant) => sqlx::query_scalar(
                        "SELECT payload_json FROM work_items WHERE id = $1 AND lease_fence = $2 \
                         AND status = 'claimed' AND tenant_id = $3 FOR UPDATE",
                    )
                    .bind(item)
                    .bind(lease_fence)
                    .bind(tenant)
                    .fetch_optional(&mut **tx)
                    .await?,
                };
            let Some(payload) = payload else {
                return Ok(false);
            };
            let mut value: serde_json::Value =
                serde_json::from_str(&payload).map_err(StateBackendError::from)?;
            let (payload, marked) = match value.as_object_mut() {
                Some(object) => {
                    object.insert(
                        "claim".into(),
                        serde_json::Value::from(jamjet_core::node::CLAIM_BY_EXECUTION),
                    );
                    let text = serde_json::to_string(&value).map_err(StateBackendError::from)?;
                    (text, true)
                }
                None => (payload, false),
            };
            sqlx::query(
                "UPDATE work_items SET status = 'pending', retry_after = statement_timestamp(), \
                 worker_id = NULL, lease_expires_at = NULL, lease_epoch = lease_epoch + 1, \
                 lease_fence = 0, payload_json = $2, by_execution = by_execution OR $3 \
                 WHERE id = $1",
            )
            .bind(item)
            .bind(payload)
            .bind(marked)
            .execute(&mut **tx)
            .await?;
            Ok(true)
        })
    })
    .await
}

/// The run's pending and claimed items, oldest first. A view judges by the
/// run's own row, not the item's.
pub(super) async fn open_work_items(
    b: &PgBackend,
    execution_id: &ExecutionId,
) -> BackendResult<Vec<OpenWorkItem>> {
    let query = match b.scope() {
        None => sqlx::query(
            "SELECT id, node_id, queue_type, status, lease_expires_at FROM work_items \
             WHERE execution_id = $1 AND status IN ('pending', 'claimed') \
             ORDER BY created_at, id",
        )
        .bind(execution_id.0),
        Some(tenant) => sqlx::query(
            "SELECT w.id, w.node_id, w.queue_type, w.status, w.lease_expires_at \
             FROM work_items w JOIN workflow_executions e ON e.execution_id = w.execution_id \
             WHERE w.execution_id = $1 AND w.status IN ('pending', 'claimed') \
             AND e.tenant_id = $2 ORDER BY w.created_at, w.id",
        )
        .bind(execution_id.0)
        .bind(tenant),
    };
    let rows = query.fetch_all(&b.pool).await.map_err(statement_error)?;
    rows.iter()
        .map(|row| {
            let status: String = row.try_get("status").map_err(decode)?;
            Ok(OpenWorkItem {
                id: row.try_get("id").map_err(decode)?,
                node_id: row.try_get("node_id").map_err(decode)?,
                queue_type: row.try_get("queue_type").map_err(decode)?,
                claimed: status == "claimed",
                lease_expires_at: row.try_get("lease_expires_at").map_err(decode)?,
            })
        })
        .collect()
}

// ── Failures, rollovers and dead letters ─────────────────────────────────────

/// L1 on the item's run, then L2 on the item under its fence, then the
/// retry or the dead letter, as the sweep would decide it: the run's lock
/// first because the dead letter's row names the run. The run's status is
/// not read, as on SQLite.
pub(super) async fn fail_work_item_fenced(
    b: &PgBackend,
    item_id: WorkItemId,
    lease_fence: i64,
    error: &str,
) -> BackendResult<Option<FailOutcome>> {
    no_nul("error", error)?;
    let Some((run, _)) = item_run(b, item_id).await? else {
        return Ok(None);
    };
    let scope = b.scope();
    with_tx(&b.pool, "fail_work_item_fenced", |tx| {
        Box::pin(async move {
            let claimed = lock_claimed(tx, run, item_id, lease_fence, false, scope).await?;
            fault_point!("pg.fail_work_item_fenced", 1);
            let Some((_, mut item)) = claimed else {
                return Ok(None);
            };
            let attempt = item.attempt + 1;
            let tenant = item.tenant_id.clone();
            let outcome = if attempt >= item.max_attempts {
                settle_dead_lettered(tx, item_id, &tenant, attempt).await?;
                fault_point!("pg.fail_work_item_fenced", 2);
                insert_dead_letter(tx, item_id, &tenant, error).await?;
                fault_point!("pg.fail_work_item_fenced", 3);
                item.attempt = attempt;
                FailOutcome::Exhausted {
                    item: Box::new(item),
                }
            } else {
                let backoff = backoff_secs(attempt);
                requeue(tx, item_id, &tenant, attempt, backoff, Holder::Forget).await?;
                fault_point!("pg.fail_work_item_fenced", 4);
                item.attempt = attempt;
                FailOutcome::Retryable {
                    item: Box::new(item),
                    delay_ms: backoff * 1000,
                }
            };
            fault_point!("pg.fail_work_item_fenced", 5);
            Ok(Some(outcome))
        })
    })
    .await
}

/// [`fail_work_item_fenced`] with its events in the same transaction, under
/// L1. An item whose run has ended is settled failed, nothing is recorded,
/// and the answer is `None`.
pub(super) async fn fail_work_item_fenced_recorded(
    b: &PgBackend,
    item_id: WorkItemId,
    lease_fence: i64,
    error: &str,
) -> BackendResult<Option<FailOutcome>> {
    no_nul("error", error)?;
    let Some((run, _)) = item_run(b, item_id).await? else {
        return Ok(None);
    };
    let scope = b.scope();
    with_tx(&b.pool, "fail_work_item_fenced_recorded", |tx| {
        Box::pin(async move {
            let claimed = lock_claimed(tx, run, item_id, lease_fence, false, scope).await?;
            fault_point!("pg.fail_work_item_fenced_recorded", 1);
            let Some((locked, mut item)) = claimed else {
                return Ok(None);
            };
            let tenant = item.tenant_id.clone();
            if locked.status.is_terminal() {
                settle_failed(tx, item_id, &tenant).await?;
                fault_point!("pg.fail_work_item_fenced_recorded", 2);
                return Ok(None);
            }
            let attempt = item.attempt + 1;
            let exhausted = attempt >= item.max_attempts;
            let backoff = backoff_secs(attempt);
            if exhausted {
                settle_dead_lettered(tx, item_id, &tenant, attempt).await?;
                fault_point!("pg.fail_work_item_fenced_recorded", 3);
            } else {
                requeue(tx, item_id, &tenant, attempt, backoff, Holder::Forget).await?;
                fault_point!("pg.fail_work_item_fenced_recorded", 4);
            }
            item.attempt = attempt;
            if exhausted {
                insert_dead_letter(tx, item_id, &tenant, error).await?;
                fault_point!("pg.fail_work_item_fenced_recorded", 5);
            }
            let outcome = if exhausted {
                FailOutcome::Exhausted {
                    item: Box::new(item),
                }
            } else {
                FailOutcome::Retryable {
                    item: Box::new(item),
                    delay_ms: backoff * 1000,
                }
            };
            insert_events(
                tx,
                run,
                &locked.tenant,
                &made(&fail_events(&outcome, error))?,
            )
            .await?;
            fault_point!("pg.fail_work_item_fenced_recorded", 6);
            Ok(Some(outcome))
        })
    })
    .await
}

/// L1 on the run, then the fenced settle of its item, then the run set
/// `limit_exceeded` unless it has ended. A run that is not there, or not
/// the view's, is `NotFound`; a fence that does not match is `false`.
pub(super) async fn finalize_rollover_fenced(
    b: &PgBackend,
    execution_id: &ExecutionId,
    item: WorkItemId,
    lease_fence: i64,
) -> BackendResult<bool> {
    let run = execution_id.0;
    let scope = b.scope();
    let limit_exceeded = status_str(&WorkflowStatus::LimitExceeded);
    with_tx(&b.pool, "finalize_rollover_fenced", |tx| {
        Box::pin(async move {
            let locked = lock_run(tx, run, scope).await?;
            fault_point!("pg.finalize_rollover_fenced", 1);
            let Some(locked) = locked else {
                return Err(StateBackendError::NotFound(run.to_string()).into());
            };
            let settle = match scope {
                None => sqlx::query(
                    "UPDATE work_items SET status = 'completed', \
                     completed_at = statement_timestamp(), lease_expires_at = NULL \
                     WHERE id = $1 AND lease_fence = $2",
                )
                .bind(item)
                .bind(lease_fence),
                Some(tenant) => sqlx::query(
                    "UPDATE work_items SET status = 'completed', \
                     completed_at = statement_timestamp(), lease_expires_at = NULL \
                     WHERE id = $1 AND lease_fence = $2 AND tenant_id = $3",
                )
                .bind(item)
                .bind(lease_fence)
                .bind(tenant),
            };
            let settled = settle.execute(&mut **tx).await?.rows_affected();
            fault_point!("pg.finalize_rollover_fenced", 2);
            if settled == 0 {
                return Ok(false);
            }
            if !locked.status.is_terminal() {
                sqlx::query(
                    "UPDATE workflow_executions SET status = $2, \
                     updated_at = statement_timestamp(), \
                     completed_at = COALESCE(completed_at, statement_timestamp()) \
                     WHERE execution_id = $1 AND tenant_id = $3",
                )
                .bind(run)
                .bind(limit_exceeded)
                .bind(&locked.tenant)
                .execute(&mut **tx)
                .await?;
            }
            fault_point!("pg.finalize_rollover_fenced", 3);
            Ok(true)
        })
    })
    .await
}

/// L1 on the item's run, then the settle and its dead-letter row in one
/// statement. An existing dead-letter row is replaced, as on SQLite.
pub(super) async fn move_to_dead_letter(
    b: &PgBackend,
    item: WorkItemId,
    last_error: &str,
) -> BackendResult<()> {
    no_nul("error", last_error)?;
    let Some((run, tenant)) = item_run(b, item).await? else {
        return Err(StateBackendError::NotFound(item.to_string()));
    };
    let scope = b.scope();
    let tenant = &tenant;
    with_tx(&b.pool, "move_to_dead_letter", |tx| {
        Box::pin(async move {
            if lock_run(tx, run, scope).await?.is_none() {
                return Err(StateBackendError::NotFound(item.to_string()).into());
            }
            let moved: Option<Uuid> = sqlx::query_scalar(
                "WITH moved AS ( \
                   UPDATE work_items SET status = 'dead_lettered', lease_expires_at = NULL, \
                     worker_id = NULL \
                   WHERE id = $1 AND tenant_id = $3 \
                   RETURNING id, execution_id, tenant_id, node_id, queue_type, payload_json, \
                     attempt, created_at) \
                 INSERT INTO dead_letter_items (id, execution_id, tenant_id, node_id, queue_type, \
                   payload_json, attempt, last_error, created_at, dead_lettered_at) \
                 SELECT id, execution_id, tenant_id, node_id, queue_type, payload_json, attempt, \
                   $2, created_at, statement_timestamp() FROM moved \
                 ON CONFLICT (id) DO UPDATE SET execution_id = EXCLUDED.execution_id, \
                   tenant_id = EXCLUDED.tenant_id, node_id = EXCLUDED.node_id, \
                   queue_type = EXCLUDED.queue_type, payload_json = EXCLUDED.payload_json, \
                   attempt = EXCLUDED.attempt, last_error = EXCLUDED.last_error, \
                   created_at = EXCLUDED.created_at, dead_lettered_at = EXCLUDED.dead_lettered_at \
                 RETURNING id",
            )
            .bind(item)
            .bind(last_error)
            .bind(tenant)
            .fetch_optional(&mut **tx)
            .await?;
            match moved {
                Some(_) => Ok(()),
                None => Err(StateBackendError::NotFound(item.to_string()).into()),
            }
        })
    })
    .await
}

// ── The sweeps ───────────────────────────────────────────────────────────────

/// The claimed items of this view whose lease has lapsed by the database's
/// clock, oldest first.
async fn lapsed_items(b: &PgBackend) -> BackendResult<Vec<WorkItem>> {
    let query = match b.scope() {
        None => sqlx::query(
            "SELECT * FROM work_items WHERE status = 'claimed' \
             AND lease_expires_at < statement_timestamp() ORDER BY created_at, id",
        ),
        Some(tenant) => sqlx::query(
            "SELECT * FROM work_items WHERE status = 'claimed' \
             AND lease_expires_at < statement_timestamp() AND tenant_id = $1 \
             ORDER BY created_at, id",
        )
        .bind(tenant),
    };
    let rows = query.fetch_all(&b.pool).await.map_err(statement_error)?;
    rows.iter().map(row_to_item).collect()
}

/// The sweep without its events, as on SQLite. Each item read lapsed is
/// reclaimed on its own, only while it is still claimed under the fence
/// read and still lapsed by the database's clock: a retry in one statement,
/// a dead letter under L1 with its row in the same statement as the
/// settle.
pub(super) async fn reclaim_expired_leases(b: &PgBackend) -> BackendResult<ReclaimResult> {
    let scope = b.scope();
    let mut result = ReclaimResult::default();
    for mut item in lapsed_items(b).await? {
        let attempt = item.attempt + 1;
        let run = item.execution_id.0;
        let (id, fence, tenant) = (item.id, item.lease_fence, item.tenant_id.clone());
        let to_attempt = to_i32("attempt", attempt)?;
        if attempt >= item.max_attempts {
            let tenant = &tenant;
            let settled = with_tx(&b.pool, "reclaim_expired_leases", |tx| {
                Box::pin(async move {
                    if lock_run(tx, run, scope).await?.is_none() {
                        return Ok(false);
                    }
                    let settled: i64 = sqlx::query_scalar(
                        "WITH settled AS ( \
                           UPDATE work_items SET status = 'dead_lettered', attempt = $4, \
                             lease_expires_at = NULL, worker_id = NULL \
                           WHERE id = $1 AND tenant_id = $2 AND status = 'claimed' \
                             AND lease_fence = $3 AND lease_expires_at < statement_timestamp() \
                           RETURNING id, execution_id, tenant_id, node_id, queue_type, \
                             payload_json, attempt, created_at), \
                         dead AS ( \
                           INSERT INTO dead_letter_items (id, execution_id, tenant_id, node_id, \
                             queue_type, payload_json, attempt, last_error, created_at, \
                             dead_lettered_at) \
                           SELECT id, execution_id, tenant_id, node_id, queue_type, \
                             payload_json, attempt, 'lease expired: worker dead', created_at, \
                             statement_timestamp() FROM settled \
                           ON CONFLICT (id) DO NOTHING) \
                         SELECT count(*) FROM settled",
                    )
                    .bind(id)
                    .bind(tenant)
                    .bind(fence)
                    .bind(to_attempt)
                    .fetch_one(&mut **tx)
                    .await?;
                    Ok(settled > 0)
                })
            })
            .await?;
            if settled {
                item.attempt = attempt;
                result.exhausted.push(item);
            }
        } else {
            let requeued = sqlx::query(
                "UPDATE work_items SET status = 'pending', attempt = $4, \
                   last_worker_id = worker_id, last_lease_fence = lease_fence, worker_id = NULL, \
                   lease_expires_at = NULL, \
                   retry_after = statement_timestamp() + make_interval(secs => $5), \
                   lease_epoch = lease_epoch + 1, lease_fence = 0 \
                 WHERE id = $1 AND tenant_id = $2 AND status = 'claimed' AND lease_fence = $3 \
                   AND lease_expires_at < statement_timestamp()",
            )
            .bind(id)
            .bind(&tenant)
            .bind(fence)
            .bind(to_attempt)
            .bind(backoff_secs(attempt) as i32)
            .execute(&b.pool)
            .await
            .map_err(statement_error)?
            .rows_affected();
            if requeued > 0 {
                item.attempt = attempt;
                result.retryable.push(item);
            }
        }
    }
    Ok(result)
}

/// The scheduler's sweep, each item reclaimed with its events in a
/// transaction of its own: L1 on its run, then L2 on the item, read again
/// and reclaimed only while it is still claimed under the fence the scan
/// read and still lapsed by the database's clock.
pub(super) async fn reclaim_expired_leases_recorded(b: &PgBackend) -> BackendResult<ReclaimResult> {
    let mut result = ReclaimResult::default();
    for candidate in lapsed_items(b).await? {
        match reclaim_one_recorded(b, &candidate).await? {
            Some((SweepOutcome::Retry, item)) => result.retryable.push(item),
            Some((SweepOutcome::Exhausted, item)) => result.exhausted.push(item),
            None => {}
        }
    }
    Ok(result)
}

/// One item of the recorded sweep. An item whose run has ended is settled
/// failed and records nothing (`None`); otherwise it is requeued or
/// dead-lettered and [`sweep_events`] are appended.
async fn reclaim_one_recorded(
    b: &PgBackend,
    candidate: &WorkItem,
) -> BackendResult<Option<(SweepOutcome, WorkItem)>> {
    let scope = b.scope();
    let (run, id, fence) = (
        candidate.execution_id.0,
        candidate.id,
        candidate.lease_fence,
    );
    with_tx(&b.pool, "reclaim_expired_leases_recorded", |tx| {
        Box::pin(async move {
            let claimed = lock_claimed(tx, run, id, fence, true, scope).await?;
            fault_point!("pg.reclaim_expired_leases_recorded", 1);
            let Some((locked, mut item)) = claimed else {
                return Ok(None);
            };
            let tenant = item.tenant_id.clone();
            if locked.status.is_terminal() {
                settle_failed(tx, id, &tenant).await?;
                fault_point!("pg.reclaim_expired_leases_recorded", 2);
                return Ok(None);
            }
            let attempt = item.attempt + 1;
            let outcome = if attempt >= item.max_attempts {
                settle_dead_lettered(tx, id, &tenant, attempt).await?;
                fault_point!("pg.reclaim_expired_leases_recorded", 3);
                SweepOutcome::Exhausted
            } else {
                let backoff = backoff_secs(attempt);
                requeue(tx, id, &tenant, attempt, backoff, Holder::Remember).await?;
                fault_point!("pg.reclaim_expired_leases_recorded", 4);
                SweepOutcome::Retry
            };
            item.attempt = attempt;
            if outcome == SweepOutcome::Exhausted {
                insert_dead_letter(tx, id, &tenant, "lease expired: worker dead").await?;
                fault_point!("pg.reclaim_expired_leases_recorded", 5);
            }
            insert_events(
                tx,
                run,
                &locked.tenant,
                &made(&sweep_events(&item, outcome))?,
            )
            .await?;
            fault_point!("pg.reclaim_expired_leases_recorded", 6);
            Ok(Some((outcome, item)))
        })
    })
    .await
}
