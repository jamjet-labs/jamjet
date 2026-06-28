//! `AuditLogEntry` — the canonical immutable record written to the audit log.

use crate::signer::AuditSigner;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Field separator used when building the canonical content of an entry for
/// hashing. A control character that never appears in our field values, so
/// concatenation is unambiguous (`"a" || "bc"` cannot collide with
/// `"ab" || "c"`).
const FIELD_SEP: u8 = 0x1f;

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
    /// Tenant that this audit entry belongs to.
    #[serde(default = "default_tenant")]
    pub tenant_id: String,
    /// When this entry expires for retention purposes. None = keep indefinitely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether PII redaction was applied to raw_event.
    #[serde(default)]
    pub redacted: bool,
    /// Hash of the *previous* sealed entry in the log (its `entry_hash`).
    ///
    /// `None` for the genesis entry of a chain. Each entry's `entry_hash`
    /// covers this field, so the entries form a tamper-evident chain: altering
    /// any past entry's content (or re-linking the chain) breaks verification
    /// from that point forward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_hash: Option<String>,
    /// SHA-256 hex digest of this entry's canonical content **plus**
    /// `prev_hash`. Set by [`AuditLogEntry::seal`]. `None` until sealed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_hash: Option<String>,
    /// Keyed HMAC-SHA256 (hex) over `entry_hash`. Set by
    /// [`AuditLogEntry::seal`]. A verifier without the key cannot forge it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

fn default_tenant() -> String {
    "default".to_string()
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
            tenant_id: "default".to_string(),
            expires_at: None,
            redacted: false,
            prev_hash: None,
            entry_hash: None,
            signature: None,
        }
    }

    /// Compute this entry's content hash, chained onto `prev_hash`.
    ///
    /// The hash covers every immutable, security-relevant field (everything
    /// except `entry_hash`/`signature`, which are derived) plus the supplied
    /// `prev_hash`. The serialization is deterministic and round-trips through
    /// SQLite storage: scalars are hashed as their stored textual form,
    /// `raw_event` via canonical (sorted-key) JSON, fields separated by
    /// [`FIELD_SEP`].
    pub fn content_hash(&self, prev_hash: Option<&str>) -> String {
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update(bytes);
            hasher.update([FIELD_SEP]);
        };

        field(self.id.as_bytes());
        field(self.event_id.as_bytes());
        field(self.execution_id.as_bytes());
        field(&self.sequence.to_le_bytes());
        field(self.event_type.as_bytes());
        field(self.actor_id.as_bytes());
        field(self.actor_type.as_str().as_bytes());
        field(self.tool_call_hash.as_deref().unwrap_or("").as_bytes());
        field(self.policy_decision.as_deref().unwrap_or("").as_bytes());
        field(self.http_request_id.as_deref().unwrap_or("").as_bytes());
        field(self.http_method.as_deref().unwrap_or("").as_bytes());
        field(self.http_path.as_deref().unwrap_or("").as_bytes());
        field(self.ip_address.as_deref().unwrap_or("").as_bytes());
        field(self.created_at.to_rfc3339().as_bytes());
        field(
            serde_json::to_string(&self.raw_event)
                .unwrap_or_default()
                .as_bytes(),
        );
        field(self.tenant_id.as_bytes());
        field(
            self.expires_at
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default()
                .as_bytes(),
        );
        field(&[u8::from(self.redacted)]);
        field(prev_hash.unwrap_or("").as_bytes());

        hex::encode(hasher.finalize())
    }

    /// Seal this entry into the chain: record `prev_hash`, compute `entry_hash`
    /// over the content + `prev_hash`, then sign `entry_hash` with `signer`.
    ///
    /// Call this once, after all enrichment/redaction is applied, immediately
    /// before persisting the entry. The `prev_hash` is the `entry_hash` of the
    /// previous sealed entry in the log (`None` for the genesis entry).
    pub fn seal(&mut self, prev_hash: Option<String>, signer: &AuditSigner) {
        let hash = self.content_hash(prev_hash.as_deref());
        self.signature = Some(signer.sign(hash.as_bytes()));
        self.entry_hash = Some(hash);
        self.prev_hash = prev_hash;
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

impl ActorType {
    /// The stable lowercase wire/storage form (matches the SQLite encoding).
    pub fn as_str(&self) -> &'static str {
        match self {
            ActorType::Human => "human",
            ActorType::Agent => "agent",
            ActorType::System => "system",
        }
    }
}
