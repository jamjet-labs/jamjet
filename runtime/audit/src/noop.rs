//! No-op audit backend — silently discards all entries.
//! Used when running with in-memory storage (no persistence needed).

use crate::backend::{AuditBackend, AuditError, AuditQuery};
use crate::entry::AuditLogEntry;
use async_trait::async_trait;

pub struct NoopAuditBackend;

#[async_trait]
impl AuditBackend for NoopAuditBackend {
    async fn append(&self, _entry: AuditLogEntry) -> Result<(), AuditError> {
        Ok(())
    }

    async fn query(&self, _q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError> {
        Ok(vec![])
    }

    async fn count(&self, _q: &AuditQuery) -> Result<u64, AuditError> {
        Ok(0)
    }
}
