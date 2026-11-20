// Clippy 1.95+ flags `match X { Variant => { if cond { ... } } }` patterns as
// collapsible. These are intentional in the materializer and memory backend
// where the if-condition is semantically distinct from the variant match
// (status checks, filter predicates). Collapsing into match-guards would
// require explicit catch-all arms and duplicate destructuring.
#![allow(clippy::collapsible_match, clippy::collapsible_if)]

mod api_token;
pub mod approvals;
pub mod artifact;
pub mod backend;
pub mod budget;
pub mod caps;
pub mod clock;
pub mod event;
// Declared before `sqlite` and `tenant_scoped`, so `fault_point!` is in
// scope in them.
#[macro_use]
pub mod faults;
mod features;
pub mod hashing;
pub mod lease_events;
mod legacy;
pub mod materializer;
pub mod memory;
pub mod postgres;
pub mod segment;
pub mod snapshot;
pub mod sqlite;
pub mod tenant;
pub mod tenant_scoped;
pub mod usage;

pub use artifact::{
    artifact_threshold, resolve_value, resolve_value_budgeted, resolve_value_within, spill_bytes,
    ArtifactRef, ARTIFACT_SENTINEL_KEY, DEFAULT_SPILL_THRESHOLD,
};
pub use backend::{
    granted_lease_secs, ApiToken, ApprovalProjectionRow, BackendResult, ClaimedWorkItem,
    ConditionalAppend, ExecutionFinish, FailOutcome, LeaseRenewal, ReclaimResult, ReserveOutcome,
    RunningPage, StateBackend, StateBackendError, TurnCommit, WorkItem, WorkItemId,
    WorkflowDefinition, DEFAULT_LEASE_SECS, MAX_LEASE_SECS, MIN_LEASE_SECS,
};
pub use backend::{ApprovalCursor, OpenWorkItem, PendingApprovalRow};
pub use backend::{ClaimFilter, ExecutionClaim, GateMark, LeasedItem};
pub use backend::{KeyedStart, RunKeyClaim, RunKeyRecord};
pub use backend::{PassCommit, SchedulerPass};
pub use budget::{BudgetState, BudgetTrip};
pub use caps::{CapKind, CapTrip, RunCaps};
pub use event::{Event, EventKind, EventSequence, ProvenanceMetadata};
pub use hashing::{
    canonical_json, content_hash, derive_idempotency_key, idempotency_key, sha256_hex,
};
pub use materializer::{
    apply_events, apply_events_seeded, json_merge_patch, materialize, should_snapshot,
    MaterializedState,
};
pub use memory::InMemoryBackend;
pub use postgres::{PgBackend, PgSettings, SchemaState};
pub use segment::{segment_chain, start_next_segment};
pub use snapshot::Snapshot;
pub use sqlite::{RevokedToken, SqliteBackend};
pub use tenant::{Tenant, TenantId, TenantLimits, TenantStatus, DEFAULT_TENANT};
pub use tenant_scoped::TenantScopedSqliteBackend;
pub use usage::{fold_run_usage, RunUsage};
