//! `AuditLogEntry` — the canonical immutable record written to the audit log.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A single immutable audit log entry.
///
/// Wraps an underlying `jamjet_state::Event` with HTTP request context,
/// actor identification, and derived security metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogEntry {
    /// Unique ID for this audit record.
    pub id: Uuid,
    /// The underlying event's UUID (`Event.id`).
    pub event_id: Uuid,
    /// The execution this event belongs to.
    pub execution_id: String,
    /// Monotonic sequence within the execution.
    pub sequence: i64,
    /// The serde tag of the `EventKind` (e.g. `"node_completed"`, `"policy_violation"`).
    pub event_type: String,
    /// Who triggered this action.
    pub actor_id: String,
    pub actor_type: ActorType,
    /// SHA-256 hex digest of `tool_name || ":" || input_json` for tool-invocation events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_hash: Option<String>,
    /// The policy decision that was made, if this event is policy-related.
    /// Values: `"allow"` | `"block"` | `"require_approval"`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_decision: Option<String>,
    /// X-Request-ID or equivalent tracing header from the originating HTTP request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip_address: Option<String>,
    pub created_at: DateTime<Utc>,
    /// The full serialized `EventKind` JSON for immutable archival.
    pub raw_event: serde_json::Value,
}

impl AuditLogEntry {
    pub fn new(
        event_id: Uuid,
        execution_id: String,
        sequence: i64,
        event_type: String,
        actor_id: String,
        actor_type: ActorType,
        raw_event: serde_json::Value,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            event_id,
            execution_id,
            sequence,
            event_type,
            actor_id,
            actor_type,
            tool_call_hash: None,
            policy_decision: None,
            http_request_id: None,
            http_method: None,
            http_path: None,
            ip_address: None,
            created_at: Utc::now(),
            raw_event,
        }
    }
}

/// Who initiated the action that produced this event.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActorType {
    #[default]
    /// A human user via the REST API.
    Human,
    /// An agent acting autonomously.
    Agent,
    /// The JamJet runtime itself (scheduler, worker, heartbeat).
    System,
}
