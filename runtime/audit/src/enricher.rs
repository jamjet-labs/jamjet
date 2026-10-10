//! `AuditEnricher` — derives audit metadata from events and writes to the audit log.

use crate::backend::{AuditBackend, AuditQuery};
use crate::entry::{ActorType, AuditLogEntry};
use crate::signer::AuditSigner;
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

/// In-memory head of the tamper-evident audit chain for this enricher.
#[derive(Default)]
struct ChainHead {
    /// Whether `head` has been seeded from the backend yet.
    seeded: bool,
    /// `entry_hash` of the most recently appended entry (`None` at genesis).
    head: Option<String>,
}

/// Enriches events with audit metadata, seals them into the tamper-evident
/// chain, and appends them to the audit log.
pub struct AuditEnricher {
    backend: Arc<dyn AuditBackend>,
    /// Signs each entry's content hash. Sourced from the environment by
    /// default (`AuditSigner::from_env`). With no `JAMJET_AUDIT_SIGNING_KEY`
    /// and no `JAMJET_AUDIT_ALLOW_INSECURE_KEY` opt-in this signer is
    /// unsigned/refused, so entries are chained but left unsigned (never signed
    /// with a forgeable dev key by default); see [`AuditSigner`].
    signer: AuditSigner,
    /// Serializes the read-head -> seal -> append -> advance-head sequence so
    /// concurrent appends form a single linear chain.
    chain: tokio::sync::Mutex<ChainHead>,
    /// How many entries failed to be written since start: a metric for audit
    /// gaps, which `verify_chain` cannot detect.
    seal_failures: std::sync::atomic::AtomicU64,
}

impl AuditEnricher {
    /// Build an enricher that signs with the key from the environment
    /// (`JAMJET_AUDIT_SIGNING_KEY`; the insecure dev key only behind the
    /// explicit `JAMJET_AUDIT_ALLOW_INSECURE_KEY` opt-in; otherwise unsigned).
    pub fn new(backend: Arc<dyn AuditBackend>) -> Self {
        Self::with_signer(backend, AuditSigner::from_env())
    }

    /// Build an enricher with an explicit signer (tests / callers that load the
    /// signing key from their own secret store).
    pub fn with_signer(backend: Arc<dyn AuditBackend>, signer: AuditSigner) -> Self {
        Self {
            backend,
            signer,
            chain: tokio::sync::Mutex::new(ChainHead::default()),
            seal_failures: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The signer entries are sealed with. In Ed25519 mode its
    /// [`AuditSigner::public_jwk`] is what a third party verifies with.
    pub fn signer(&self) -> &AuditSigner {
        &self.signer
    }

    /// How many audit entries failed to be written since this enricher was
    /// built. Each one is an event with no audit entry: a gap
    /// `verify_chain` cannot see.
    pub fn seal_failures(&self) -> u64 {
        self.seal_failures.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The `entry_hash` of the most recent entry already in the log, used to
    /// resume the chain across process restarts. `None` for an empty log.
    async fn latest_entry_hash(&self) -> Option<String> {
        let q = AuditQuery {
            limit: 1,
            ..Default::default()
        };
        let latest = self.backend.query(&q).await.ok()?.into_iter().next()?;
        latest.entry_hash
    }

    /// Derive audit metadata from an event and append it to the audit log.
    ///
    /// Call this after every `StateBackend::append_event` call, passing
    /// the HTTP request context if available.
    pub async fn enrich_and_append(&self, event: &Event, ctx: Option<&RequestContext>) {
        if let Err(e) = self.try_enrich_and_append(event, ctx).await {
            warn!("Failed to write audit log entry: {e}");
        }
    }

    /// [`enrich_and_append`](Self::enrich_and_append), reporting a failure to
    /// the caller (who can record the gap) instead of only logging it. Every
    /// failure is also counted in [`seal_failures`](Self::seal_failures).
    pub async fn try_enrich_and_append(
        &self,
        event: &Event,
        ctx: Option<&RequestContext>,
    ) -> Result<(), String> {
        let out = self.seal_entry(event, ctx).await;
        if out.is_err() {
            self.seal_failures
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        out
    }

    async fn seal_entry(&self, event: &Event, ctx: Option<&RequestContext>) -> Result<(), String> {
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
            Err(e) => return Err(format!("event does not serialize for the audit log: {e}")),
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

        // Seal the entry into the tamper-evident chain: link it to the
        // previous entry's hash, then sign. Held under the chain lock for the
        // whole read-head -> seal -> append -> advance-head sequence so that
        // concurrent appends produce one linear chain.
        let mut chain = self.chain.lock().await;
        if !chain.seeded {
            chain.head = self.latest_entry_hash().await;
            chain.seeded = true;
        }
        entry.seal(chain.head.clone(), &self.signer);
        let sealed_hash = entry.entry_hash.clone();

        match self.backend.append(entry).await {
            // Advance the head only after a durable append, so a failed write
            // never leaves a gap that the next entry would link straight past.
            Ok(()) => {
                chain.head = sealed_hash;
                Ok(())
            }
            Err(e) => Err(format!("audit log write failed: {e}")),
        }
    }
}

#[async_trait::async_trait]
impl jamjet_state::approvals::ApprovalSealer for AuditEnricher {
    /// Seal an engine-written approval record (request, expiry, escalation)
    /// into the same chain as the decisions, as a system actor of the run's
    /// tenant.
    async fn seal_approval_record(&self, event: &Event, tenant_id: &str) -> Result<(), String> {
        let ctx = RequestContext::system_for_tenant("system", tenant_id);
        self.try_enrich_and_append(event, Some(&ctx)).await
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
        // The authenticated principal when there is one; the display name is
        // operator-chosen and not unique. Dev-mode approvals have only the name.
        EventKind::ApprovalReceived {
            principal: Some(principal),
            ..
        } => principal.clone(),
        EventKind::ApprovalReceived { user_id, .. } => user_id.clone(),
        // An expiry is a rejection by its recorded principal, system:deadline.
        EventKind::ApprovalExpired { principal, .. } => principal.clone(),
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
        | EventKind::ApprovalExpired { .. }
        | EventKind::ApprovalEscalated { .. }
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
