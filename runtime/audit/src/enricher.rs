//! `AuditEnricher` — derives audit metadata from events and writes to the audit log.

use crate::backend::AuditBackend;
use crate::entry::{ActorType, AuditLogEntry};
use chrono::Duration;
use jamjet_ir::workflow::DataPolicyIr;
use jamjet_policy::redaction::PiiRedactor;
use jamjet_state::event::{Event, EventKind};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tracing::warn;

/// HTTP request context injected by the API middleware.
///
/// `None` values are expected for events emitted by the scheduler / worker
/// (no HTTP request is involved).
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub request_id: Option<String>,
    pub method: Option<String>,
    pub path: Option<String>,
    pub ip: Option<String>,
    /// `actor_id` comes from the API token's name field, or "system" for internal events.
    pub actor_id: String,
    pub actor_type: ActorType,
    /// Tenant context for this request.
    pub tenant_id: String,
    /// Data handling policy for PII redaction and retention.
    pub data_policy: Option<DataPolicyIr>,
}

impl RequestContext {
    pub fn system(worker_id: impl Into<String>) -> Self {
        Self {
            actor_id: worker_id.into(),
            actor_type: ActorType::System,
            tenant_id: "default".to_string(),
            ..Default::default()
        }
    }

    pub fn system_for_tenant(worker_id: impl Into<String>, tenant_id: impl Into<String>) -> Self {
        Self {
            actor_id: worker_id.into(),
            actor_type: ActorType::System,
            tenant_id: tenant_id.into(),
            ..Default::default()
        }
    }
}

/// Enriches events with audit metadata and appends them to the audit log.
pub struct AuditEnricher {
    backend: Arc<dyn AuditBackend>,
}

impl AuditEnricher {
    pub fn new(backend: Arc<dyn AuditBackend>) -> Self {
        Self { backend }
    }

    /// Derive audit metadata from an event and append it to the audit log.
    ///
    /// Call this after every `StateBackend::append_event` call, passing
    /// the HTTP request context if available.
    pub async fn enrich_and_append(&self, event: &Event, ctx: Option<&RequestContext>) {
        let default_ctx = RequestContext {
            actor_id: "system".to_string(),
            actor_type: ActorType::System,
            tenant_id: "default".to_string(),
            ..Default::default()
        };
        let ctx = ctx.unwrap_or(&default_ctx);

        let event_type = extract_event_type(&event.kind);
        let raw_event = match serde_json::to_value(&event.kind) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to serialize event for audit log: {e}");
                return;
            }
        };

        let actor_id = derive_actor_id(&event.kind, &ctx.actor_id);
        let actor_type = derive_actor_type(&event.kind, &ctx.actor_type);

        let mut entry = AuditLogEntry::new(
            event.id,
            event.execution_id.to_string(),
            event.sequence,
            event_type,
            actor_id,
            actor_type,
            raw_event,
        );

        // Enrich with HTTP context.
        entry.http_request_id = ctx.request_id.clone();
        entry.http_method = ctx.method.clone();
        entry.http_path = ctx.path.clone();
        entry.ip_address = ctx.ip.clone();

        // Derive tool_call_hash for tool-invocation events.
        entry.tool_call_hash = extract_tool_call_hash(&event.kind);

        // Derive policy_decision for policy events.
        entry.policy_decision = extract_policy_decision(&event.kind);

        // Set tenant_id from request context.
        entry.tenant_id = ctx.tenant_id.clone();

        // Apply data policy: PII redaction + retention.
        if let Some(data_policy) = &ctx.data_policy {
            let redactor = PiiRedactor::from_policy(data_policy);
            if redactor.is_active() {
                entry.raw_event = redactor.redact_json(&entry.raw_event);
                entry.redacted = true;
            }

            // Retention: set expires_at based on policy.
            if let Some(days) = data_policy.retention_days {
                entry.expires_at = Some(entry.created_at + Duration::days(i64::from(days)));
            }

            // Prompt/output retention controls.
            if !data_policy.retain_prompts {
                strip_prompts(&mut entry.raw_event);
            }
            if !data_policy.retain_outputs {
                strip_outputs(&mut entry.raw_event);
            }
        }

        if let Err(e) = self.backend.append(entry).await {
            warn!("Failed to write audit log entry: {e}");
        }
    }
}

/// Remove prompt data from the raw event JSON.
fn strip_prompts(raw: &mut serde_json::Value) {
    if let Some(obj) = raw.as_object_mut() {
        obj.remove("prompt");
        obj.remove("system_prompt");
        obj.remove("input_messages");
    }
}

/// Remove output/completion data from the raw event JSON.
fn strip_outputs(raw: &mut serde_json::Value) {
    if let Some(obj) = raw.as_object_mut() {
        obj.remove("output");
        obj.remove("completion");
        obj.remove("response");
    }
}

// ── Derivation helpers ────────────────────────────────────────────────────

fn extract_event_type(kind: &EventKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| {
            v.get("type")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn derive_actor_id(kind: &EventKind, default: &str) -> String {
    match kind {
        EventKind::ApprovalReceived { user_id, .. } => user_id.clone(),
        EventKind::NodeStarted { worker_id, .. } => worker_id.clone(),
        _ => default.to_string(),
    }
}

fn derive_actor_type(kind: &EventKind, default: &ActorType) -> ActorType {
    match kind {
        EventKind::ApprovalReceived { .. } => ActorType::Human,
        EventKind::NodeStarted { .. }
        | EventKind::NodeCompleted { .. }
        | EventKind::NodeFailed { .. }
        | EventKind::NodeScheduled { .. } => ActorType::System,
        EventKind::PolicyViolation { .. }
        | EventKind::ToolApprovalRequired { .. }
        | EventKind::AutonomyLimitReached { .. }
        | EventKind::CircuitBreakerTripped { .. }
        | EventKind::EscalationRequired { .. }
        | EventKind::TokenBudgetExceeded { .. }
        | EventKind::CostBudgetExceeded { .. } => ActorType::System,
        _ => default.clone(),
    }
}

fn extract_tool_call_hash(kind: &EventKind) -> Option<String> {
    let (tool_name, input) = match kind {
        EventKind::ToolCalled { tool, .. } => (tool.as_str(), "{}"),
        EventKind::ToolApprovalRequired {
            tool_name, context, ..
        } => {
            let input = context.to_string();
            // We can't return a reference to a local, so hash inline.
            let mut hasher = Sha256::new();
            hasher.update(tool_name.as_bytes());
            hasher.update(b":");
            hasher.update(input.as_bytes());
            let result = hasher.finalize();
            return Some(hex::encode(result));
        }
        _ => return None,
    };

    let mut hasher = Sha256::new();
    hasher.update(tool_name.as_bytes());
    hasher.update(b":");
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    Some(hex::encode(result))
}

fn extract_policy_decision(kind: &EventKind) -> Option<String> {
    match kind {
        EventKind::PolicyViolation { decision, .. } => Some(decision.clone()),
        EventKind::ToolApprovalRequired { .. } => Some("require_approval".to_string()),
        _ => None,
    }
}

// hex encoding helper (avoids pulling in the `hex` crate)
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
}
