//! `AuditBackend` trait — the append-only storage interface for audit records.

use crate::entry::AuditLogEntry;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("database error: {0}")]
    Database(String),
    #[error("serialization error: {0}")]
    Serialization(String),
}

/// Query parameters for `AuditBackend::query`.
#[derive(Debug, Default)]
pub struct AuditQuery {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// Filter by actor_id (exact match).
    pub actor_id: Option<String>,
    /// Filter by event_type tag (exact match, e.g. `"policy_violation"`).
    pub event_type: Option<String>,
    /// Filter by execution_id.
    pub execution_id: Option<String>,
    pub limit: u32,
    pub offset: u32,
}

impl AuditQuery {
    pub fn new() -> Self {
        Self {
            limit: 50,
            ..Default::default()
        }
    }
}

/// Append-only audit log storage.
///
/// Implementations MUST NEVER issue UPDATE or DELETE SQL against the
/// audit_log table. The audit log is the system of record for compliance.
#[async_trait]
pub trait AuditBackend: Send + Sync {
    /// Append a single audit log entry. Idempotent on `entry.id`.
    async fn append(&self, entry: AuditLogEntry) -> Result<(), AuditError>;

    /// Query audit log entries with optional filters.
    async fn query(&self, q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError>;

    /// Count audit log entries matching the query (for pagination).
    async fn count(&self, q: &AuditQuery) -> Result<u64, AuditError>;
}
