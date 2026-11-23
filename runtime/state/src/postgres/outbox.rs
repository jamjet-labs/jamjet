//! The audit outbox on Postgres: a row for each event of a sealed kind
//! ([`SEALED_EVENT_KINDS`](crate::approvals::SEALED_EVENT_KINDS)) that has
//! no audit entry yet.
//!
//! The row is written in the transaction that appends the event
//! ([`insert_events`](super::runs::insert_events)), so no sealed event is
//! committed without one. The audit entry's append deletes it in the
//! entry's own transaction. What is left is an event whose seal was lost:
//! the drain in `jamjet_audit::outbox` seals it from these rows.
//!
//! A row is due from its last failed try, or from its creation before any,
//! so a row that keeps failing queues behind rows written after its last
//! try, and a drain is never held up by the same rows each time.
//!
//! Each method but [`PgBackend::outbox_record_gap`] runs its statements one
//! by one and takes no row lock. Two drains may read one row at once; the
//! audit log keeps one entry per event, so the second seal adds nothing.

use super::runs::{insert_events, lock_known_run, row_to_event, NewEvent};
use super::tx::{statement_error, with_tx};
use super::PgBackend;
use crate::backend::BackendResult;
use crate::event::Event;
use chrono::{DateTime, Utc};
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

/// An outbox row with the event it stands for.
#[derive(Debug, Clone)]
pub struct OutboxRow {
    pub event: Event,
    /// The run's tenant, which the event's seal is made under.
    pub tenant_id: String,
    /// Failed seals of this row so far.
    pub attempts: i32,
    /// Whether the run's log already records the gap (`AuditSealFailed`).
    pub gap_recorded: bool,
}

impl PgBackend {
    /// Up to `limit` readable rows due at least `older_than` ago by the
    /// database's clock, earliest due first, each with its event. A row is
    /// due from its last failed try, or from its creation. A tenant's view
    /// sees only its own tenant's rows.
    ///
    /// A row whose event this build cannot read is logged, counted as a
    /// failed try and left out; it stays in the outbox. The read goes on
    /// past such rows, a page at a time, until it has `limit` readable rows
    /// or the due rows run out, so no number of them holds up the rest.
    pub async fn outbox_due(
        &self,
        older_than: Duration,
        limit: u32,
    ) -> BackendResult<Vec<OutboxRow>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        // One cut-off for every page, so a row tried during this read is
        // not due again within it.
        let cutoff: DateTime<Utc> =
            sqlx::query_scalar("SELECT statement_timestamp() - make_interval(secs => $1)")
                .bind(older_than.as_secs_f64())
                .fetch_one(&self.pool)
                .await
                .map_err(statement_error)?;
        let mut due = Vec::new();
        let mut unreadable: Vec<Uuid> = Vec::new();
        let mut after: Option<(DateTime<Utc>, Uuid)> = None;
        while due.len() < limit as usize {
            let page = sqlx::query(
                "SELECT e.id, e.execution_id, e.sequence, e.kind_json, e.created_at, \
                        o.tenant_id, o.attempts, o.gap_recorded, \
                        COALESCE(o.last_tried_at, o.created_at) AS due_at \
                 FROM audit_outbox o JOIN events e ON e.id = o.event_id \
                 WHERE COALESCE(o.last_tried_at, o.created_at) <= $1 \
                   AND ($2::text IS NULL OR o.tenant_id = $2) \
                   AND ($3::timestamptz IS NULL \
                        OR (COALESCE(o.last_tried_at, o.created_at), o.event_id) > ($3, $4::uuid)) \
                 ORDER BY COALESCE(o.last_tried_at, o.created_at), o.event_id \
                 LIMIT $5",
            )
            .bind(cutoff)
            .bind(self.scope())
            .bind(after.map(|(at, _)| at))
            .bind(after.map(|(_, id)| id))
            .bind(i64::from(limit))
            .fetch_all(&self.pool)
            .await
            .map_err(statement_error)?;
            let full = page.len() == limit as usize;
            for row in &page {
                let event_id: Uuid = row.try_get("id").map_err(statement_error)?;
                after = Some((row.try_get("due_at").map_err(statement_error)?, event_id));
                if due.len() == limit as usize {
                    break;
                }
                let event = match row_to_event(row) {
                    Ok(event) => event,
                    Err(e) => {
                        tracing::error!(
                            %event_id,
                            "an outbox row's event cannot be read, so it is not sealed: {e}"
                        );
                        unreadable.push(event_id);
                        continue;
                    }
                };
                due.push(OutboxRow {
                    event,
                    tenant_id: row.try_get("tenant_id").map_err(statement_error)?,
                    attempts: row.try_get("attempts").map_err(statement_error)?,
                    gap_recorded: row.try_get("gap_recorded").map_err(statement_error)?,
                });
            }
            if !full {
                break;
            }
        }
        if !unreadable.is_empty() {
            sqlx::query(
                "UPDATE audit_outbox \
                 SET attempts = attempts + 1, last_tried_at = statement_timestamp(), \
                     last_error = 'its event cannot be read' \
                 WHERE event_id = ANY($1) AND ($2::text IS NULL OR tenant_id = $2)",
            )
            .bind(&unreadable)
            .bind(self.scope())
            .execute(&self.pool)
            .await
            .map_err(statement_error)?;
        }
        Ok(due)
    }

    /// Count a failed seal of the event's row, keep its error and date the
    /// try, so the row is due again after rows written since. Answers
    /// the attempts after this one, or 0 when the row is gone because the
    /// event was sealed meanwhile.
    pub async fn outbox_note_failure(&self, event_id: Uuid, error: &str) -> BackendResult<i32> {
        // Postgres text holds no NUL, and an error text is not worth
        // refusing the count over.
        let error = error.replace('\0', "\u{fffd}");
        let attempts: Option<i32> = sqlx::query_scalar(
            "UPDATE audit_outbox \
             SET attempts = attempts + 1, last_error = $2, last_tried_at = statement_timestamp() \
             WHERE event_id = $1 AND ($3::text IS NULL OR tenant_id = $3) \
             RETURNING attempts",
        )
        .bind(event_id)
        .bind(error)
        .bind(self.scope())
        .fetch_optional(&self.pool)
        .await
        .map_err(statement_error)?;
        Ok(attempts.unwrap_or(0))
    }

    /// Record `event`'s gap in its run's log (`AuditSealFailed`), unless it
    /// is recorded already. The row's mark is claimed and the record
    /// appended in one transaction, under the run's lock, so of two drains
    /// at once one records the gap and the other finds it claimed. Answers
    /// whether this call recorded it; `false` also when the row is gone.
    pub async fn outbox_record_gap(&self, event: &Event, error: String) -> BackendResult<bool> {
        let gap = NewEvent::of(&crate::approvals::seal_gap_event(event, error))?;
        let run = event.execution_id.0;
        let event_id = event.id;
        let scope = self.scope();
        with_tx(&self.pool, "outbox_record_gap", |tx| {
            let gap = &gap;
            Box::pin(async move {
                let locked = lock_known_run(tx, run, scope).await?;
                let claimed: Option<i32> = sqlx::query_scalar(
                    "UPDATE audit_outbox SET gap_recorded = true \
                     WHERE event_id = $1 AND NOT gap_recorded \
                       AND ($2::text IS NULL OR tenant_id = $2) \
                     RETURNING 1",
                )
                .bind(event_id)
                .bind(scope)
                .fetch_optional(&mut **tx)
                .await?;
                if claimed.is_none() {
                    return Ok(false);
                }
                insert_events(tx, run, &locked.tenant, std::slice::from_ref(gap)).await?;
                Ok(true)
            })
        })
        .await
    }

    /// Note that the run's log now records the event's gap, so it is
    /// recorded once. The row stays until the event is sealed.
    pub async fn outbox_mark_gap(&self, event_id: Uuid) -> BackendResult<()> {
        sqlx::query(
            "UPDATE audit_outbox SET gap_recorded = true \
             WHERE event_id = $1 AND ($2::text IS NULL OR tenant_id = $2)",
        )
        .bind(event_id)
        .bind(self.scope())
        .execute(&self.pool)
        .await
        .map_err(statement_error)?;
        Ok(())
    }
}
