//! JamJet Audit Log (Phase 4 — Tier 1)
//!
//! Provides an immutable, append-only audit trail of all security-relevant
//! events: policy violations, approval actions, budget limits, autonomy
//! escalations, and workflow lifecycle events.
//!
//! The audit log is separate from the event log so it can be retained
//! independently, exported to SIEMs, and queried by compliance tooling
//! without touching the workflow execution tables.

pub mod backend;
pub mod enricher;
pub mod entry;
pub mod noop;
pub mod sqlite;

pub use backend::{AuditBackend, AuditError, AuditQuery};
pub use enricher::{AuditEnricher, RequestContext};
pub use entry::{ActorType, AuditLogEntry};
pub use noop::NoopAuditBackend;
pub use sqlite::SqliteAuditBackend;
