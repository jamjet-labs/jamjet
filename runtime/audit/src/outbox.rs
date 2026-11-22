//! The outbox drain: seals the sealed events whose seal was lost.
//!
//! On Postgres every event of a sealed kind is appended with an
//! `audit_outbox` row, and its audit entry deletes that row. The engine
//! still seals each event right after appending it, with the HTTP request's
//! context. A row left behind is an event that seal never reached: the
//! engine stopped between the append and the seal, or the audit write
//! failed. [`drain`] seals those, as a system actor of the run's tenant.
//!
//! A row stays until its event is sealed. After [`GAP_AFTER_ATTEMPTS`]
//! failed seals the run's log records the gap (`AuditSealFailed`) once, so
//! it is visible while the drain keeps trying. A later seal does not take
//! the gap record back; a reader takes a gap whose event has an entry as
//! closed.
//!
//! Two engines can drain at once. Both may seal one event; the audit log
//! keeps one entry per event, so the second seal adds nothing.

use crate::enricher::{AuditEnricher, RequestContext};
use jamjet_state::approvals::try_record_seal_gap;
use jamjet_state::PgBackend;
use std::time::Duration;
use tracing::{error, warn};

/// How old a row is before the drain seals it. The engine seals each event
/// right after its append; this leaves that seal, which carries the HTTP
/// context, time to land first.
pub const DUE_AFTER: Duration = Duration::from_secs(5);

/// Failed seals of one event before its gap is recorded in the run's log.
pub const GAP_AFTER_ATTEMPTS: i32 = 3;

/// What one [`drain`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Rows that were due.
    pub due: usize,
    /// Of those, the events now in the audit log: sealed by this drain, or
    /// found sealed by another.
    pub sealed: usize,
    /// Of those, the events whose seal failed.
    pub failed: usize,
    /// Gaps this drain recorded in run logs.
    pub gaps_recorded: usize,
    /// Why the outbox could not be read, when it could not.
    pub error: Option<String>,
}

/// Seal up to `limit` outbox rows written at least `older_than` ago. Never
/// fails: what went wrong is logged and counted in the report.
pub async fn drain(
    pg: &PgBackend,
    enricher: &AuditEnricher,
    older_than: Duration,
    limit: u32,
) -> DrainReport {
    let mut report = DrainReport::default();
    let rows = match pg.outbox_due(older_than, limit).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("the audit outbox could not be read: {e}");
            report.error = Some(e.to_string());
            return report;
        }
    };
    report.due = rows.len();
    for row in rows {
        let ctx = RequestContext::system_for_tenant("system", row.tenant_id.as_str());
        let seal_error = match enricher.try_enrich_and_append(&row.event, Some(&ctx)).await {
            Ok(()) => {
                report.sealed += 1;
                continue;
            }
            Err(e) => e,
        };
        report.failed += 1;
        let event_id = row.event.id;
        let attempts = match pg.outbox_note_failure(event_id, &seal_error).await {
            Ok(attempts) => attempts,
            Err(e) => {
                warn!(%event_id, "a failed seal could not be counted in the outbox: {e}");
                continue;
            }
        };
        if attempts < GAP_AFTER_ATTEMPTS || row.gap_recorded {
            continue;
        }
        let why = format!("the outbox could not seal it in {attempts} attempts: {seal_error}");
        match try_record_seal_gap(pg, &row.event, why).await {
            Ok(()) => {
                report.gaps_recorded += 1;
                if let Err(e) = pg.outbox_mark_gap(event_id).await {
                    // The next drain records the gap again: a second record
                    // of one gap, never none.
                    warn!(%event_id, "a recorded gap could not be marked in the outbox: {e}");
                }
            }
            Err(e) => error!(%event_id, "an audit gap could not be recorded: {e}"),
        }
    }
    report
}
