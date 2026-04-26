// Clippy 1.95+ flags `match X { Variant => { if cond { ... } } }` patterns as
// collapsible. These are intentional in the materializer and memory backend
// where the if-condition is semantically distinct from the variant match
// (status checks, filter predicates). Collapsing into match-guards would
// require explicit catch-all arms and duplicate destructuring.
#![allow(clippy::collapsible_match, clippy::collapsible_if)]

pub mod backend;
pub mod budget;
pub mod event;
pub mod materializer;
pub mod memory;
pub mod snapshot;
pub mod sqlite;
pub mod tenant;
pub mod tenant_scoped;

pub use backend::{
    ApiToken, BackendResult, ReclaimResult, StateBackend, StateBackendError, WorkItem, WorkItemId,
    WorkflowDefinition,
};
pub use budget::BudgetState;
pub use event::{Event, EventKind, EventSequence, ProvenanceMetadata};
pub use materializer::{apply_events, materialize, should_snapshot, MaterializedState};
pub use memory::InMemoryBackend;
pub use snapshot::Snapshot;
pub use sqlite::SqliteBackend;
pub use tenant::{Tenant, TenantId, TenantLimits, TenantStatus, DEFAULT_TENANT};
pub use tenant_scoped::TenantScopedSqliteBackend;
