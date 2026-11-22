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
//! Each method is one statement and takes no row lock. Two drains may read
//! one row at once; the audit log keeps one entry per event, so the second
//! seal adds nothing.

use super::runs::row_to_event;
use super::tx::statement_error;
use super::PgBackend;
use crate::backend::BackendResult;
use crate::event::Event;
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
    /// Up to `limit` rows written at least `older_than` ago by the
    /// database's clock, oldest first, each with its event. A tenant's view
    /// sees only its own tenant's rows.
    pub async fn outbox_due(
        &self,
        older_than: Duration,
        limit: u32,
    ) -> BackendResult<Vec<OutboxRow>> {
        let rows = sqlx::query(
            "SELECT e.id, e.execution_id, e.sequence, e.kind_json, e.created_at, \
                    o.tenant_id, o.attempts, o.gap_recorded \
             FROM audit_outbox o JOIN events e ON e.id = o.event_id \
             WHERE o.created_at <= statement_timestamp() - make_interval(secs => $1) \
               AND ($3::text IS NULL OR o.tenant_id = $3) \
             ORDER BY o.created_at, o.event_id \
             LIMIT $2",
        )
        .bind(older_than.as_secs_f64())
        .bind(i64::from(limit))
        .bind(self.scope())
        .fetch_all(&self.pool)
        .await
        .map_err(statement_error)?;
        rows.iter()
            .map(|row| {
                Ok(OutboxRow {
                    event: row_to_event(row)?,
                    tenant_id: row.try_get("tenant_id").map_err(statement_error)?,
                    attempts: row.try_get("attempts").map_err(statement_error)?,
                    gap_recorded: row.try_get("gap_recorded").map_err(statement_error)?,
                })
            })
            .collect()
    }

    /// Count a failed seal of the event's row and keep its error. Answers
    /// the attempts after this one, or 0 when the row is gone because the
    /// event was sealed meanwhile.
    pub async fn outbox_note_failure(&self, event_id: Uuid, error: &str) -> BackendResult<i32> {
        // Postgres text holds no NUL, and an error text is not worth
        // refusing the count over.
        let error = error.replace('\0', "\u{fffd}");
        let attempts: Option<i32> = sqlx::query_scalar(
            "UPDATE audit_outbox SET attempts = attempts + 1, last_error = $2 \
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
