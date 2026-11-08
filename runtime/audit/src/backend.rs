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
    /// Filter by tenant_id for multi-tenant isolation.
    pub tenant_id: Option<String>,
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

    /// Append `entry` only if the log's newest entry is still the one it was
    /// sealed onto (`entry.prev_hash`; `None` for an empty log). `Ok(false)`:
    /// another writer appended first, so the entry must be sealed onto the
    /// new [`head`](Self::head) and tried again.
    ///
    /// Two processes append to one log (the server, and `jamjet-server token
    /// create` beside it), each holding the head in memory: a plain append
    /// from either would fork the chain. The default appends without the
    /// check, which is right only for a log one process writes.
    async fn append_linked(&self, entry: AuditLogEntry) -> Result<bool, AuditError> {
        self.append(entry).await.map(|()| true)
    }

    /// The `entry_hash` of the newest entry, in the order
    /// [`append_linked`](Self::append_linked) checks: `None` for an empty log.
    async fn head(&self) -> Result<Option<String>, AuditError> {
        let q = AuditQuery {
            limit: 1,
            ..Default::default()
        };
        Ok(self
            .query(&q)
            .await?
            .into_iter()
            .next()
            .and_then(|e| e.entry_hash))
    }

    /// Query audit log entries with optional filters.
    async fn query(&self, q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError>;

    /// Count audit log entries matching the query (for pagination).
    async fn count(&self, q: &AuditQuery) -> Result<u64, AuditError>;
}
