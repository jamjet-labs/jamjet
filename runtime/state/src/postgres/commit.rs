//! The fenced commit of a node's turn, on Postgres: `commit_turn` and
//! `commit_turn_unless_terminal`.
//!
//! One transaction. The run's row is locked first, then the item's, and the
//! fence is checked here against the locked row; then the item settles, the
//! terminal event takes the next sequence, the tool effect is recorded when
//! the event carries an idempotency key, and, when asked, the run's snapshot
//! is folded from its latest snapshot and every event after it. The fold
//! reads the log under the run's lock, so two siblings that commit at once
//! take turns there and the second folds the first's event: its snapshot
//! carries both patches.
//!
//! Through `commit_turn_unless_terminal`, the turn of a run that has ended
//! withdraws its item and records nothing. The status is read with the run's
//! lock, so a cancel lands wholly before the commit, which then sees it, or
//! wholly after. The fence is checked before the status: a stale or forged
//! fence answers `FenceLost` and touches nothing, ended run or not.

use super::queue::settle_failed;
use super::runs::{
    decode, insert_events, lock_run, row_to_event, row_to_snapshot, status_str, LockedRun, NewEvent,
};
use super::tx::{no_nul, with_tx, TxError};
use super::PgBackend;
use crate::backend::{BackendResult, StateBackendError, TurnCommit, WorkItemId};
use crate::event::{Event, EventKind};
use crate::materializer::{apply_events_seeded, MaterializedState};
use crate::snapshot::Snapshot;
use jamjet_core::workflow::WorkflowStatus;
use sqlx::{PgConnection, Row};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// How the turn's terminal event settles its item.
#[derive(Clone, Copy)]
enum Settle {
    /// `NodeCompleted`: completed, with the time it completed.
    Completed,
    /// `NodeFailed`: failed, with no worker.
    Failed,
}

/// The tool effect a `NodeCompleted` with an idempotency key records.
struct Effect {
    key: String,
    node_id: String,
    result_json: String,
}

/// The effect `kind` records, if it is a `NodeCompleted` with a key: the
/// result a later attempt of the node replays instead of running again.
fn effect_of(kind: &EventKind) -> BackendResult<Option<Effect>> {
    let EventKind::NodeCompleted {
        node_id,
        output,
        state_patch,
        duration_ms,
        gen_ai_system,
        gen_ai_model,
        input_tokens,
        output_tokens,
        finish_reason,
        cost_usd,
        idempotency_key: Some(key),
        ..
    } = kind
    else {
        return Ok(None);
    };
    no_nul("idempotency key", key)?;
    let result = serde_json::json!({
        "output": output,
        "state_patch": state_patch,
        "duration_ms": duration_ms,
        "gen_ai_system": gen_ai_system,
        "gen_ai_model": gen_ai_model,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "finish_reason": finish_reason,
        "cost_usd": cost_usd,
    });
    Ok(Some(Effect {
        key: key.clone(),
        node_id: node_id.clone(),
        result_json: serde_json::to_string(&result)?,
    }))
}

/// The item's row as the commit locked it.
struct LockedItem {
    status: String,
    lease_fence: i64,
    tenant: String,
}

/// Lock the run's row (L1), then the item's (L2), in that order. A run that
/// is not there, or not the view's, is `NotFound`; an item that is not
/// there, or not the view's, is `None`.
async fn lock_run_and_item(
    conn: &mut PgConnection,
    run: Uuid,
    item: WorkItemId,
    scope: Option<&str>,
) -> Result<(LockedRun, Option<LockedItem>), TxError> {
    let Some(locked) = lock_run(&mut *conn, run, scope).await? else {
        return Err(StateBackendError::NotFound(run.to_string()).into());
    };
    let row =
        match scope {
            None => sqlx::query(
                "SELECT status, lease_fence, tenant_id FROM work_items WHERE id = $1 FOR UPDATE",
            )
            .bind(item)
            .fetch_optional(&mut *conn)
            .await?,
            Some(tenant) => {
                sqlx::query(
                    "SELECT status, lease_fence, tenant_id FROM work_items \
                 WHERE id = $1 AND tenant_id = $2 FOR UPDATE",
                )
                .bind(item)
                .bind(tenant)
                .fetch_optional(&mut *conn)
                .await?
            }
        };
    let item = match row {
        Some(row) => Some(LockedItem {
            status: row.try_get("status").map_err(decode)?,
            lease_fence: row.try_get("lease_fence").map_err(decode)?,
            tenant: row.try_get("tenant_id").map_err(decode)?,
        }),
        None => None,
    };
    Ok((locked, item))
}

/// Settle the locked item as its terminal event says.
async fn settle_item(
    conn: &mut PgConnection,
    item: WorkItemId,
    tenant: &str,
    settle: Settle,
) -> Result<(), TxError> {
    let update = match settle {
        Settle::Completed => {
            "UPDATE work_items SET status = 'completed', completed_at = statement_timestamp(), \
             lease_expires_at = NULL WHERE id = $1 AND tenant_id = $2"
        }
        Settle::Failed => {
            "UPDATE work_items SET status = 'failed', completed_at = NULL, \
             lease_expires_at = NULL, worker_id = NULL WHERE id = $1 AND tenant_id = $2"
        }
    };
    sqlx::query(update)
        .bind(item)
        .bind(tenant)
        .execute(conn)
        .await?;
    Ok(())
}

/// Write `snapshot` under `tenant`, the run's, replacing one the run already
/// has at its sequence, as `write_snapshot` does. Only a row of `tenant` is
/// replaced; the run's lock already holds it to the run's tenant.
async fn insert_snapshot(
    conn: &mut PgConnection,
    snapshot: &Snapshot,
    tenant: &str,
) -> Result<(), TxError> {
    let state = serde_json::to_string(&snapshot.state).map_err(StateBackendError::from)?;
    let completed =
        serde_json::to_string(&snapshot.completed_nodes).map_err(StateBackendError::from)?;
    let active = serde_json::to_string(&snapshot.active_nodes).map_err(StateBackendError::from)?;
    sqlx::query(
        "INSERT INTO snapshots \
           (id, execution_id, tenant_id, at_sequence, state_json, created_at, status, \
            completed_nodes_json, active_nodes_json, last_sequence) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         ON CONFLICT (execution_id, at_sequence) DO UPDATE SET \
           id = EXCLUDED.id, tenant_id = EXCLUDED.tenant_id, state_json = EXCLUDED.state_json, \
           created_at = EXCLUDED.created_at, status = EXCLUDED.status, \
           completed_nodes_json = EXCLUDED.completed_nodes_json, \
           active_nodes_json = EXCLUDED.active_nodes_json, \
           last_sequence = EXCLUDED.last_sequence \
         WHERE snapshots.tenant_id = $3",
    )
    .bind(snapshot.id)
    .bind(snapshot.execution_id.0)
    .bind(tenant)
    .bind(snapshot.at_sequence)
    .bind(state)
    .bind(snapshot.created_at)
    .bind(status_str(&snapshot.status))
    .bind(completed)
    .bind(active)
    .bind(snapshot.last_sequence)
    .execute(conn)
    .await?;
    Ok(())
}

/// The commit both trait methods run. With `refuse_terminal`, a run that has
/// ended withdraws the item instead and the answer is
/// [`TurnCommit::ExecutionTerminal`].
pub(super) async fn commit_turn(
    b: &PgBackend,
    item_id: WorkItemId,
    lease_fence: i64,
    event: Event,
    write_snapshot: bool,
    refuse_terminal: bool,
) -> BackendResult<TurnCommit> {
    // A miswired caller fails before anything is written, rather than
    // settling an item as done for an event that does not end its node.
    let settle = match &event.kind {
        EventKind::NodeCompleted { .. } => Settle::Completed,
        EventKind::NodeFailed { .. } => Settle::Failed,
        _ => {
            return Err(StateBackendError::Database(
                "commit_turn requires a terminal event (NodeCompleted/NodeFailed)".into(),
            ))
        }
    };
    let row = NewEvent::of(&event)?;
    let effect = effect_of(&event.kind)?;
    let run = event.execution_id.0;
    let execution_id = &event.execution_id;
    let scope = b.scope();
    let (row, effect) = (std::slice::from_ref(&row), &effect);
    with_tx(&b.pool, "commit_turn", |tx| {
        Box::pin(async move {
            let (locked, item) = lock_run_and_item(tx, run, item_id, scope).await?;
            fault_point!("pg.commit_turn", 1);
            let Some(item) = item.filter(|i| i.status == "claimed" && i.lease_fence == lease_fence)
            else {
                return Err(StateBackendError::FenceLost(item_id.to_string()).into());
            };
            if refuse_terminal && locked.status.is_terminal() {
                settle_failed(tx, item_id, &item.tenant).await?;
                fault_point!("pg.commit_turn", 2);
                return Ok(TurnCommit::ExecutionTerminal(locked.status));
            }
            settle_item(tx, item_id, &item.tenant, settle).await?;
            fault_point!("pg.commit_turn", 3);
            let sequence = insert_events(tx, run, &locked.tenant, row).await?;
            fault_point!("pg.commit_turn", 4);
            if let Some(effect) = effect {
                // A row already there is the first result for the key, and
                // stays: a later commit of the same key replays it.
                sqlx::query(
                    "INSERT INTO tool_effects (idempotency_key, execution_id, node_id, \
                       result_json, tenant_id, recorded_at) \
                     VALUES ($1, $2, $3, $4, $5, statement_timestamp()) \
                     ON CONFLICT (idempotency_key) DO NOTHING",
                )
                .bind(&effect.key)
                .bind(run)
                .bind(&effect.node_id)
                .bind(&effect.result_json)
                .bind(&locked.tenant)
                .execute(&mut **tx)
                .await?;
                fault_point!("pg.commit_turn", 5);
            }
            if write_snapshot {
                let latest =
                    match scope {
                        None => {
                            sqlx::query(
                                "SELECT * FROM snapshots WHERE execution_id = $1 \
                             ORDER BY at_sequence DESC LIMIT 1",
                            )
                            .bind(run)
                            .fetch_optional(&mut **tx)
                            .await?
                        }
                        Some(tenant) => sqlx::query(
                            "SELECT * FROM snapshots WHERE execution_id = $1 AND tenant_id = $2 \
                             ORDER BY at_sequence DESC LIMIT 1",
                        )
                        .bind(run)
                        .bind(tenant)
                        .fetch_optional(&mut **tx)
                        .await?,
                    };
                fault_point!("pg.commit_turn", 6);
                let (base, after) = match latest {
                    Some(row) => {
                        let snapshot = row_to_snapshot(&row)?;
                        let after = snapshot.at_sequence;
                        let base = MaterializedState {
                            current_state: snapshot.state,
                            status: snapshot.status,
                            completed_nodes: snapshot.completed_nodes,
                            active_nodes: snapshot.active_nodes,
                            last_sequence: snapshot.last_sequence,
                        };
                        (base, after)
                    }
                    None => {
                        // No snapshot yet: the fold starts from the run's
                        // input.
                        let input: String = match scope {
                            None => {
                                sqlx::query_scalar(
                                    "SELECT initial_input FROM workflow_executions \
                                     WHERE execution_id = $1",
                                )
                                .bind(run)
                                .fetch_one(&mut **tx)
                                .await?
                            }
                            Some(tenant) => {
                                sqlx::query_scalar(
                                    "SELECT initial_input FROM workflow_executions \
                                     WHERE execution_id = $1 AND tenant_id = $2",
                                )
                                .bind(run)
                                .bind(tenant)
                                .fetch_one(&mut **tx)
                                .await?
                            }
                        };
                        fault_point!("pg.commit_turn", 7);
                        let base = MaterializedState {
                            current_state: serde_json::from_str(&input)
                                .map_err(StateBackendError::from)?,
                            status: WorkflowStatus::Pending,
                            completed_nodes: HashMap::new(),
                            active_nodes: HashSet::new(),
                            last_sequence: 0,
                        };
                        (base, 0)
                    }
                };
                let rows =
                    match scope {
                        None => sqlx::query(
                            "SELECT id, execution_id, sequence, kind_json, created_at FROM events \
                             WHERE execution_id = $1 AND sequence > $2 ORDER BY sequence",
                        )
                        .bind(run)
                        .bind(after)
                        .fetch_all(&mut **tx)
                        .await?,
                        Some(tenant) => sqlx::query(
                            "SELECT id, execution_id, sequence, kind_json, created_at FROM events \
                             WHERE execution_id = $1 AND sequence > $2 AND tenant_id = $3 \
                             ORDER BY sequence",
                        )
                        .bind(run)
                        .bind(after)
                        .bind(tenant)
                        .fetch_all(&mut **tx)
                        .await?,
                    };
                fault_point!("pg.commit_turn", 8);
                let tail = rows
                    .iter()
                    .map(row_to_event)
                    .collect::<BackendResult<Vec<_>>>()?;
                let folded = apply_events_seeded(base, &tail);
                let snapshot = Snapshot::from_materialized(execution_id.clone(), &folded);
                insert_snapshot(tx, &snapshot, &locked.tenant).await?;
                fault_point!("pg.commit_turn", 9);
            }
            fault_point!("pg.commit_turn", 10);
            Ok(TurnCommit::Committed(sequence))
        })
    })
    .await
}
