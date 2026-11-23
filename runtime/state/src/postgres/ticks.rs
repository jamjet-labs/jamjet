//! The tick lease on Postgres (`StateBackend::begin_tick`, `finish_tick`,
//! `tick_status`): one row, `name = 'tick'`, shared by every engine on the
//! database and by every tenant's view, on the database's clock.
//!
//! A begin is one upsert whose update runs only while the lease is free or
//! lapsed. A begin that meets a row another begin has locked waits for it
//! and checks the row as that one left it, so two begins never both take
//! the lease. When the update does not run, the row stays locked by this
//! transaction, so the read that follows names the holder that kept it. A
//! finish is one update that names its holder: a holder whose lease was
//! taken over changes nothing.

use super::runs::decode;
use super::tx::{no_nul, statement_error, with_tx};
use super::PgBackend;
use crate::backend::{BackendResult, StateBackendError, TickBegin, TickOutcome, TickStatus};
use sqlx::Row;
use std::time::Duration;

pub(super) async fn begin_tick(
    b: &PgBackend,
    holder: &str,
    lease: Duration,
) -> BackendResult<TickBegin> {
    no_nul("tick holder", holder)?;
    crate::backend::tick_lease(lease)?;
    let secs = lease.as_secs_f64();
    with_tx(&b.pool, "begin_tick", |tx| {
        Box::pin(async move {
            let took = sqlx::query(
                "INSERT INTO engine_ticks (name, holder, lease_until, last_started_at) \
                 VALUES ('tick', $1, statement_timestamp() + make_interval(secs => $2), \
                         statement_timestamp()) \
                 ON CONFLICT (name) DO UPDATE SET holder = EXCLUDED.holder, \
                   lease_until = EXCLUDED.lease_until, last_started_at = EXCLUDED.last_started_at \
                 WHERE engine_ticks.holder IS NULL \
                    OR engine_ticks.lease_until <= statement_timestamp() \
                 RETURNING cursor, last_finished_at",
            )
            .bind(holder)
            .bind(secs)
            .fetch_optional(&mut **tx)
            .await?;
            fault_point!("pg.begin_tick", 1);
            if let Some(row) = took {
                return Ok(TickBegin::Began {
                    cursor: row.try_get("cursor").map_err(decode)?,
                    last_finished_at: row.try_get("last_finished_at").map_err(decode)?,
                });
            }
            let row =
                sqlx::query("SELECT holder, lease_until FROM engine_ticks WHERE name = 'tick'")
                    .fetch_one(&mut **tx)
                    .await?;
            let held: Option<String> = row.try_get("holder").map_err(decode)?;
            let until = row.try_get("lease_until").map_err(decode)?;
            match (held, until) {
                (Some(holder), Some(lease_until)) => Ok(TickBegin::Busy {
                    holder,
                    lease_until,
                }),
                // The upsert left the row locked, so no finish ran between.
                _ => Err(StateBackendError::Database(
                    "the tick lease was held and then found free".into(),
                )
                .into()),
            }
        })
    })
    .await
}

pub(super) async fn finish_tick(
    b: &PgBackend,
    holder: &str,
    outcome: &TickOutcome,
) -> BackendResult<bool> {
    // A holder with a NUL cannot hold the lease.
    if holder.contains('\0') {
        return Ok(false);
    }
    if let Some(cursor) = &outcome.cursor {
        no_nul("tick cursor", cursor)?;
    }
    let report = serde_json::to_string(&outcome.report)?;
    let (complete, cursor, report) = (outcome.complete, outcome.cursor.as_deref(), &report);
    with_tx(&b.pool, "finish_tick", |tx| {
        Box::pin(async move {
            let done = sqlx::query(
                "UPDATE engine_ticks SET holder = NULL, lease_until = NULL, \
                   last_finished_at = statement_timestamp(), last_complete = $2, cursor = $3, \
                   last_report = $4 \
                 WHERE name = 'tick' AND holder = $1",
            )
            .bind(holder)
            .bind(complete)
            .bind(cursor)
            .bind(report)
            .execute(&mut **tx)
            .await?
            .rows_affected();
            fault_point!("pg.finish_tick", 1);
            Ok(done == 1)
        })
    })
    .await
}

pub(super) async fn tick_status(b: &PgBackend) -> BackendResult<Option<TickStatus>> {
    let row = sqlx::query(
        "SELECT holder, lease_until, last_started_at, last_finished_at, last_complete, \
           cursor, last_report \
         FROM engine_ticks WHERE name = 'tick'",
    )
    .fetch_optional(&b.pool)
    .await
    .map_err(statement_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let report: Option<String> = row.try_get("last_report").map_err(decode)?;
    Ok(Some(TickStatus {
        holder: row.try_get("holder").map_err(decode)?,
        lease_until: row.try_get("lease_until").map_err(decode)?,
        last_started_at: row.try_get("last_started_at").map_err(decode)?,
        last_finished_at: row.try_get("last_finished_at").map_err(decode)?,
        last_complete: row.try_get("last_complete").map_err(decode)?,
        last_report: report.as_deref().map(serde_json::from_str).transpose()?,
        cursor: row.try_get("cursor").map_err(decode)?,
    }))
}
