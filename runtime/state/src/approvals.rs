//! Approval-state derivation.
//!
//! Folds an execution's event log into per-node approval status. This is the
//! single source of truth consumed by the worker (skip re-holding an approved
//! node), the API (validate decisions, list pending approvals), and the
//! scheduler tests. Events are the only persistent state: a node is pending
//! iff its latest `ToolApprovalRequired` has no later `ApprovalReceived`.

use crate::event::{ApprovalDecision, EventKind, DEADLINE_PRINCIPAL};
use crate::hashing::content_hash;
use crate::{Event, EventSequence};
use chrono::{DateTime, Utc};
use jamjet_core::workflow::ExecutionId;
use serde::Serialize;

/// A `ToolApprovalRequired` that has not been decided yet.
#[derive(Debug, Clone, Serialize)]
pub struct PendingApproval {
    pub node_id: String,
    pub tool_name: String,
    pub approver: String,
    pub context: serde_json::Value,
    pub sequence: EventSequence,
    /// When the request expires, as recorded in its context (UTC, RFC 3339).
    /// Absent when its policy set no deadline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// When the request escalates, as recorded in its context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalate_at: Option<String>,
}

impl PendingApproval {
    /// The pending view of the request recorded at `sequence`.
    pub fn from_request(
        node_id: &str,
        tool_name: &str,
        approver: &str,
        context: &serde_json::Value,
        sequence: EventSequence,
    ) -> Self {
        let shown = |key: &str| {
            context.get(key).map(|v| match v.as_str() {
                Some(s) => s.to_string(),
                None => v.to_string(),
            })
        };
        Self {
            node_id: node_id.to_string(),
            tool_name: tool_name.to_string(),
            approver: approver.to_string(),
            context: context.clone(),
            sequence,
            expires_at: shown(EXPIRES_AT_KEY),
            escalate_at: shown(ESCALATE_AT_KEY),
        }
    }

    /// The request's deadline, read from its context.
    pub fn deadline(&self) -> RequestDeadline {
        RequestDeadline::of(&self.context)
    }

    /// The `action_digest` the request was made for, if it carries one.
    pub fn action_digest(&self) -> Option<&str> {
        self.context.get(ACTION_DIGEST_KEY).and_then(|d| d.as_str())
    }

    /// What this request binds, for a decider to echo: the `action_digest`
    /// (every request made since dispatch requests were bound to it), or for
    /// an older agent tool dispatch request the `calls_hash` it carried.
    /// `None` for a legacy request that binds nothing.
    pub fn binding(&self) -> Option<&str> {
        self.action_digest()
            .or_else(|| self.context.get(CALLS_HASH_KEY).and_then(|d| d.as_str()))
    }
}

/// Approval status of a single node, derived from the event log.
#[derive(Debug, Clone)]
pub enum NodeApprovalStatus {
    NotRequested,
    Pending(PendingApproval),
    Approved {
        user_id: String,
        sequence: EventSequence,
        /// The action digest the approval authorises; `None` for a legacy
        /// approval or one answering a request with no digest.
        approved_digest: Option<String>,
    },
    Rejected {
        user_id: String,
        comment: Option<String>,
        sequence: EventSequence,
    },
    /// The log holds an event kind this build cannot read
    /// (`EventKind::Unknown`). Its kind and node are unknown, so it could be a
    /// revocation or a denial of this node's approval, whatever its position:
    /// any one in the run's log makes every node's status indeterminate.
    /// Treated as not approved: nothing is released and no decision accepted.
    Indeterminate,
}

/// Why an approval decision cannot be made on a log holding an event kind
/// this build cannot read.
pub const UNREADABLE_LOG_REASON: &str = "the run's event log contains event kinds this build \
     cannot read (written by a newer release); approval state cannot be determined, so \
     nothing is released or decided; upgrade the engine";

/// Fold events (must be in sequence order) into the status of `node_id`.
pub fn node_approval_status(events: &[Event], node_id: &str) -> NodeApprovalStatus {
    let mut status = NodeApprovalStatus::NotRequested;
    // Any unreadable event makes the status indeterminate. Its node cannot be
    // read, so it may bear on this one, and a later request does not clear it.
    let mut unreadable = false;
    for event in events {
        match &event.kind {
            EventKind::Unknown => unreadable = true,
            EventKind::ToolApprovalRequired {
                node_id: n,
                tool_name,
                approver,
                context,
            } if n == node_id => {
                status = NodeApprovalStatus::Pending(PendingApproval::from_request(
                    n,
                    tool_name,
                    approver,
                    context,
                    event.sequence,
                ));
            }
            EventKind::ApprovalReceived {
                node_id: n,
                user_id,
                decision,
                comment,
                approved_digest,
                ..
            } if n == node_id => {
                // A decision only resolves an outstanding request.
                if matches!(status, NodeApprovalStatus::Pending(_)) {
                    status = match decision {
                        ApprovalDecision::Approved => NodeApprovalStatus::Approved {
                            user_id: user_id.clone(),
                            sequence: event.sequence,
                            approved_digest: approved_digest.clone(),
                        },
                        ApprovalDecision::Rejected => NodeApprovalStatus::Rejected {
                            user_id: user_id.clone(),
                            comment: comment.clone(),
                            sequence: event.sequence,
                        },
                    };
                }
            }
            // An expiry is a rejection by `system:deadline`, of the request
            // it names. One naming any other request resolves nothing.
            EventKind::ApprovalExpired {
                node_id: n,
                request_sequence,
                expires_at,
                ..
            } if n == node_id => {
                if matches!(&status, NodeApprovalStatus::Pending(p) if p.sequence == *request_sequence)
                {
                    status = NodeApprovalStatus::Rejected {
                        user_id: DEADLINE_PRINCIPAL.to_string(),
                        comment: Some(expiry_comment(expires_at)),
                        sequence: event.sequence,
                    };
                }
            }
            _ => {}
        }
    }
    if unreadable {
        return NodeApprovalStatus::Indeterminate;
    }
    status
}

/// Whether the log holds an event kind this build cannot read.
pub fn has_unreadable_events(events: &[Event]) -> bool {
    events.iter().any(|e| matches!(e.kind, EventKind::Unknown))
}

/// The comment an expiry is recorded with in the approval state.
pub fn expiry_comment(expires_at: &DateTime<Utc>) -> String {
    format!(
        "approval request expired undecided at its deadline {}",
        expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    )
}

/// Context key under which a hold records the digest of the action it parks.
pub const ACTION_DIGEST_KEY: &str = "action_digest";

/// Context key under which an agent tool dispatch hold records the hash of the
/// calls it parks (`workers::dispatch_guard`).
pub const CALLS_HASH_KEY: &str = "calls_hash";

/// Context key under which a hold records what it binds, for a reviewer: the
/// payload the node will be dispatched with (size-capped), the occurrence and
/// the node.
pub const ACTION_BASIS_KEY: &str = "action_basis";

/// Largest serialized payload shown in [`ACTION_BASIS_KEY`]. Above it the
/// preview is dropped (never cut mid-value) and flagged; the digest still
/// covers the whole payload.
pub const ACTION_BASIS_STATE_CAP: usize = 16 * 1024;

/// Context key under which a hold records which occurrence of the node it is
/// for: the node's completion count when the request was made.
pub const ACTION_STEP_KEY: &str = "action_step";

/// The request the node's current decision answered: its latest
/// `ToolApprovalRequired`, if the node has a decision. `None` while the latest
/// request is still pending or there has never been one.
pub fn answered_request(events: &[Event], node_id: &str) -> Option<PendingApproval> {
    match node_approval_status(events, node_id) {
        NodeApprovalStatus::Approved { .. } | NodeApprovalStatus::Rejected { .. } => {}
        _ => return None,
    }
    events.iter().rev().find_map(|e| match &e.kind {
        EventKind::ToolApprovalRequired {
            node_id: n,
            tool_name,
            approver,
            context,
        } if n == node_id => Some(PendingApproval::from_request(
            n, tool_name, approver, context, e.sequence,
        )),
        _ => None,
    })
}

/// The digest an approval of a plain node is bound to.
///
/// Covers the run, the node, which occurrence of the node this is (`step`, the
/// number of times it has already completed), the node's DEFINITION (see
/// [`definition_hash`]) and the work item PAYLOAD the node is dispatched with
/// (see [`action_payload`]): the exact bytes the executor or external worker
/// receives, its `input` included. So an approval stops covering the action
/// when the input changes after the request (a sibling commits, an upstream
/// node writes), when the node is visited again (a loop or any re-enqueue of
/// a completed node): `step` differs, so the old approval cannot be replayed,
/// when the node's definition is not the one that was reviewed, and when an
/// item for the node carries any payload other than the reviewed one.
///
/// The payload, not the materialized state: the scheduler builds
/// `payload.input` with a shallow top-level fold of the run's state, the
/// materializer deep-merges (RFC 7396), and the two disagree on nested
/// objects and null-valued keys. Binding the materialized state showed and
/// approved one input while the node ran on another (review I1).
///
/// Domain-separated from the idempotency key (`hashing::idempotency_key`).
pub fn action_digest(
    run: &str,
    step: u64,
    node: &str,
    definition: &str,
    payload: &serde_json::Value,
) -> String {
    content_hash(&serde_json::json!({
        "domain": "jamjet.approval.action.v3",
        "run": run,
        "step": step,
        "node": node,
        "definition": definition,
        "payload": content_hash(payload),
    }))
}

/// Context key under which a hold records the [`definition_hash`] of the node
/// it parks, so a reviewer (and the approve route) can see what was bound.
pub const ACTION_DEFINITION_KEY: &str = "action_definition";

/// The hash of one node's definition as registered (the IR's `NodeDef`
/// serialized to JSON, its own policy included), in canonical JSON. Bound
/// into [`action_digest`] so an approval never covers a different node body.
pub fn definition_hash(node_def: &serde_json::Value) -> String {
    content_hash(&serde_json::json!({
        "domain": "jamjet.approval.node.v1",
        "node": node_def,
    }))
}

/// What an approval of one node occurrence binds: read once, shared by every
/// caller that decides or shows it.
#[derive(Debug, Clone)]
pub struct ActionBasis {
    pub run: String,
    pub step: u64,
    pub definition: String,
    /// The dispatched payload, as [`action_payload`] returns it.
    pub payload: serde_json::Value,
}

impl ActionBasis {
    /// The digest of firing `node` on this basis.
    pub fn digest(&self, node: &str) -> String {
        action_digest(&self.run, self.step, node, &self.definition, &self.payload)
    }
}

/// State keys the ENGINE itself writes, excluded from the action digest.
///
/// Every in-process completion writes the execution's budget bookkeeping
/// (`BudgetState::patch_into_snapshot_state`, key `"__budget"`) into state.
/// If the digest covered it, any sibling finishing while a gated node waited
/// would void the node's approval, and a node on a busy parallel branch could
/// re-request forever. These keys describe the engine's accounting, not the
/// action, so they are left out.
///
/// EXACTLY the engine's keys: not every `"__"`-prefixed key, because user state
/// may use that prefix and a change to it must still void an approval. A new
/// engine-written key goes here, and `engine_state_keys_cover_the_budget_writer`
/// fails if the budget writer starts writing one that is not listed.
pub const ENGINE_STATE_KEYS: &[&str] = &["__budget"];

/// `state` without the [`ENGINE_STATE_KEYS`]: what an approval binds.
pub fn action_state(state: &serde_json::Value) -> serde_json::Value {
    let mut state = state.clone();
    if let Some(obj) = state.as_object_mut() {
        for key in ENGINE_STATE_KEYS {
            obj.remove(*key);
        }
    }
    state
}

/// The part of a work item's payload an approval binds: the payload exactly
/// as the engine dispatched it, minus the [`ENGINE_STATE_KEYS`] in the state
/// it carries (`input` for function nodes, `state` for condition nodes).
pub fn action_payload(payload: &serde_json::Value) -> serde_json::Value {
    let mut payload = payload.clone();
    if let Some(obj) = payload.as_object_mut() {
        for key in ["input", "state"] {
            if let Some(v) = obj.get_mut(key) {
                *v = action_state(v);
            }
        }
    }
    payload
}

/// Read the [`ActionBasis`] for the next occurrence of `node_id`, dispatched
/// with `payload` (the work item's payload): the node's completion count and
/// [`action_payload`]. `definition` is the node's [`definition_hash`].
///
/// The ONE derivation of the basis. The in-process worker, the claim route,
/// the completion check and the pending listing all go through it, so the
/// action a reviewer approves, the one the gate compares and the one that runs
/// are the same bytes. Errors propagate: a caller on the enforcement path must
/// treat them as "cannot prove this is approved".
pub async fn action_basis(
    backend: &dyn crate::backend::StateBackend,
    execution_id: &ExecutionId,
    node_id: &str,
    definition: &str,
    payload: &serde_json::Value,
) -> crate::backend::BackendResult<ActionBasis> {
    let step = backend
        .count_node_completions(execution_id, node_id)
        .await?;
    Ok(ActionBasis {
        run: execution_id.to_string(),
        step,
        definition: definition.to_string(),
        payload: action_payload(payload),
    })
}

/// [`action_digest`] for the next occurrence of `node_id` dispatched with
/// `payload`, read from the backend.
pub async fn derive_action_digest(
    backend: &dyn crate::backend::StateBackend,
    execution_id: &ExecutionId,
    node_id: &str,
    definition: &str,
    payload: &serde_json::Value,
) -> crate::backend::BackendResult<String> {
    Ok(
        action_basis(backend, execution_id, node_id, definition, payload)
            .await?
            .digest(node_id),
    )
}

/// Whether an approval status authorises firing an action whose digest is
/// `current`. Only an `Approved` whose `approved_digest` equals `current`
/// does; a missing digest (legacy approval) never does.
pub fn approval_covers(status: &NodeApprovalStatus, current: &str) -> bool {
    matches!(
        status,
        NodeApprovalStatus::Approved { approved_digest: Some(d), .. } if d == current
    )
}

/// The principal that started this run: the FIRST `WorkflowStarted`'s
/// `initiated_by`. `None` for a dev-mode run, a run logged before initiators
/// were recorded, or a log with no `WorkflowStarted`.
pub fn run_initiator(events: &[Event]) -> Option<String> {
    events.iter().find_map(|e| match &e.kind {
        EventKind::WorkflowStarted { initiated_by, .. } => Some(initiated_by.clone()),
        _ => None,
    })?
}

/// All nodes whose latest `ToolApprovalRequired` is undecided.
pub fn pending_approvals(events: &[Event]) -> Vec<PendingApproval> {
    use std::collections::BTreeMap;
    let mut by_node: BTreeMap<String, Option<PendingApproval>> = BTreeMap::new();
    for event in events {
        match &event.kind {
            EventKind::ToolApprovalRequired {
                node_id,
                tool_name,
                approver,
                context,
            } => {
                by_node.insert(
                    node_id.clone(),
                    Some(PendingApproval::from_request(
                        node_id,
                        tool_name,
                        approver,
                        context,
                        event.sequence,
                    )),
                );
            }
            // Same rule as `node_approval_status`: an expiry resolves only the
            // request it names.
            EventKind::ApprovalExpired {
                node_id,
                request_sequence,
                ..
            } => {
                if let Some(slot) = by_node.get_mut(node_id) {
                    if slot.as_ref().map(|p| p.sequence) == Some(*request_sequence) {
                        *slot = None;
                    }
                }
            }
            EventKind::ApprovalReceived { node_id, .. } => {
                // Intentional asymmetry: node_approval_status returns NotRequested for a
                // decision with no prior request; here we simply leave the event with no
                // effect. Both paths ignore orphan decisions by design.
                if let Some(slot) = by_node.get_mut(node_id) {
                    *slot = None;
                }
            }
            _ => {}
        }
    }
    by_node.into_values().flatten().collect()
}

/// Seals an approval record (a request, an expiry, an escalation) that has
/// been durably appended into the signed, hash-chained audit log, beside the
/// decisions the approve routes seal. Implemented by `jamjet_audit`'s
/// `AuditEnricher`; declared here so the worker and the scheduler, which
/// append those records, need no dependency on the audit crate.
///
/// Sealing is after the durable append and best effort, like the decision
/// path: a failed audit write is logged by the implementation and never
/// undoes or blocks the record it describes.
#[async_trait::async_trait]
pub trait ApprovalSealer: Send + Sync {
    /// Seal `event` (with its assigned sequence) for `tenant_id`. `Err` when
    /// no audit entry was written; the caller records the gap.
    async fn seal_approval_record(&self, event: &Event, tenant_id: &str) -> Result<(), String>;
}

/// Seal an approval record the engine has just appended: a request, an
/// expiry or an escalation. Every place the engine appends one calls this,
/// with the sequence the backend assigned, which the sealed entry carries.
/// `None` seals nothing (a component built without an audit log). A failed
/// seal is recorded in the run's log with [`record_seal_gap`].
pub async fn seal_appended(
    backend: &dyn crate::backend::StateBackend,
    sealer: Option<&dyn ApprovalSealer>,
    mut event: Event,
    appended: EventSequence,
    tenant_id: &str,
) {
    if let Some(sealer) = sealer {
        event.sequence = appended;
        if let Err(error) = sealer.seal_approval_record(&event, tenant_id).await {
            record_seal_gap(backend, &event, error).await;
        }
    }
}

/// Record in the run's own log that `event` has no audit entry
/// (`AuditSealFailed`). The signed chain cannot show a missing entry:
/// `verify_chain` checks the entries it is given, so an event never sealed
/// is invisible to it. This record is what makes the gap findable (compare
/// the log with the audit entries). Best effort: if this append fails too,
/// the gap is only in the process log.
pub async fn record_seal_gap(
    backend: &dyn crate::backend::StateBackend,
    event: &Event,
    error: String,
) {
    let event_type = serde_json::to_value(&event.kind)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_string());
    tracing::error!(
        execution_id = %event.execution_id,
        sequence = event.sequence,
        %event_type,
        %error,
        "event not sealed into the audit chain; recording the gap"
    );
    let gap = Event::new(
        event.execution_id.clone(),
        0,
        EventKind::AuditSealFailed {
            event_id: event.id,
            sequence: event.sequence,
            event_type,
            error,
        },
    );
    if let Err(e) = backend.append_event(gap).await {
        tracing::error!(execution_id = %event.execution_id, "could not record the audit gap either: {e}");
    }
}

/// Context key under which a request records when it expires: UTC, RFC 3339.
pub const EXPIRES_AT_KEY: &str = "expires_at";

/// Context key under which a request records when it escalates.
pub const ESCALATE_AT_KEY: &str = "escalate_at";

/// How a deadline is written into a request's context: UTC, millisecond
/// precision, `Z`.
pub fn format_deadline(at: &DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Record `deadline`, counted from `now`, in a request's `context`:
/// [`EXPIRES_AT_KEY`], and [`ESCALATE_AT_KEY`] when it escalates. `Err` if a
/// time cannot be represented, which a validated deadline never hits; the
/// caller must then refuse to raise the request rather than raise it with no
/// deadline.
pub fn stamp_deadline(
    context: &mut serde_json::Map<String, serde_json::Value>,
    deadline: &jamjet_ir::workflow::ApprovalDeadline,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let after = |secs: u64| {
        i64::try_from(secs)
            .ok()
            .and_then(chrono::Duration::try_seconds)
            .and_then(|d| now.checked_add_signed(d))
            .ok_or_else(|| format!("approval deadline of {secs}s cannot be represented"))
    };
    let expires_at = after(deadline.timeout_secs)?;
    context.insert(
        EXPIRES_AT_KEY.to_string(),
        serde_json::Value::String(format_deadline(&expires_at)),
    );
    if let Some(secs) = deadline.escalate_after_secs {
        let escalate_at = after(secs)?;
        context.insert(
            ESCALATE_AT_KEY.to_string(),
            serde_json::Value::String(format_deadline(&escalate_at)),
        );
    }
    Ok(())
}

/// One recorded time of a request's deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadlineTime {
    /// The request records none.
    Unset,
    /// The request records this time.
    At(DateTime<Utc>),
    /// The request records something that is not a time. Only the engine
    /// writes these, so this is corruption or tampering, and it is read as
    /// already passed: a deadline nobody can read must not keep a request open.
    Unreadable,
}

impl DeadlineTime {
    fn read(context: &serde_json::Value, key: &str) -> Self {
        match context.get(key) {
            None => DeadlineTime::Unset,
            Some(v) => match v
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            {
                Some(t) => DeadlineTime::At(t.with_timezone(&Utc)),
                None => DeadlineTime::Unreadable,
            },
        }
    }

    /// Whether this time has come at `now`. A time is reached AT its instant
    /// (`now >= at`), so a decision and a sweep at exactly the deadline agree
    /// that it has passed. `Unset` never passes; `Unreadable` always has.
    pub fn passed(&self, now: DateTime<Utc>) -> bool {
        match self {
            DeadlineTime::Unset => false,
            DeadlineTime::At(at) => now >= *at,
            DeadlineTime::Unreadable => true,
        }
    }
}

/// A request's deadline as its context records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestDeadline {
    pub expires_at: DeadlineTime,
    pub escalate_at: DeadlineTime,
}

impl RequestDeadline {
    /// Read the deadline a request's `context` records.
    pub fn of(context: &serde_json::Value) -> Self {
        Self {
            expires_at: DeadlineTime::read(context, EXPIRES_AT_KEY),
            escalate_at: DeadlineTime::read(context, ESCALATE_AT_KEY),
        }
    }

    /// Whether the request has expired at `now`. A decision is refused then,
    /// whether or not a sweep has recorded the expiry yet.
    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.passed(now)
    }
}

/// What a deadline sweep should record for one pending request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadlineAction {
    /// Record `ApprovalExpired` for the request at `request_sequence`.
    /// `expires_at` is the recorded deadline, or the sweep's own time when the
    /// recorded one is unreadable.
    Expire {
        node_id: String,
        request_sequence: EventSequence,
        expires_at: DateTime<Utc>,
    },
    /// Record `ApprovalEscalated` for the request at `request_sequence`.
    Escalate {
        node_id: String,
        request_sequence: EventSequence,
        escalate_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    },
}

/// What a sweep at `now` should record for this log: an expiry for every
/// pending request whose deadline has passed, and otherwise an escalation for
/// one whose escalation time has passed and that has not escalated yet. A
/// request past both is expired, not escalated: there is nothing left to
/// escalate. Never an approval.
///
/// This decides from one read; the sweep appends each action with
/// [`still_pending`] (and, for an escalation, [`not_escalated`]) as the
/// append's condition, so an action this read found due cannot land beside a
/// decision or a newer request recorded since.
pub fn due_deadline_actions(events: &[Event], now: DateTime<Utc>) -> Vec<DeadlineAction> {
    let mut actions = Vec::new();
    for p in pending_approvals(events) {
        let d = p.deadline();
        if d.expired(now) {
            let expires_at = match d.expires_at {
                DeadlineTime::At(at) => at,
                _ => now,
            };
            actions.push(DeadlineAction::Expire {
                node_id: p.node_id.clone(),
                request_sequence: p.sequence,
                expires_at,
            });
        } else if let (DeadlineTime::At(expires_at), true) =
            (d.expires_at, d.escalate_at.passed(now))
        {
            if not_escalated(events, &p.node_id, p.sequence) {
                let escalate_at = match d.escalate_at {
                    DeadlineTime::At(at) => at,
                    _ => now,
                };
                actions.push(DeadlineAction::Escalate {
                    node_id: p.node_id.clone(),
                    request_sequence: p.sequence,
                    escalate_at,
                    expires_at,
                });
            }
        }
    }
    actions
}

/// Whether `node_id`'s approval is pending on exactly the request recorded at
/// `request_sequence`: no decision, no expiry and no newer request since. The
/// condition every append that resolves or escalates a request is made under.
pub fn still_pending(events: &[Event], node_id: &str, request_sequence: EventSequence) -> bool {
    matches!(
        node_approval_status(events, node_id),
        NodeApprovalStatus::Pending(p) if p.sequence == request_sequence
    )
}

/// [`still_pending`], and the request's deadline has not passed at `now`.
/// The condition a human decision is appended under: evaluated inside the
/// append, under the backend's write lock, so a decision whose append is
/// delayed past the deadline (a lock wait, a stall) is refused instead of
/// landing late.
pub fn still_open(
    events: &[Event],
    node_id: &str,
    request_sequence: EventSequence,
    now: DateTime<Utc>,
) -> bool {
    matches!(
        node_approval_status(events, node_id),
        NodeApprovalStatus::Pending(p)
            if p.sequence == request_sequence && !p.deadline().expired(now)
    )
}

/// Whether `node_id`'s pending request is past its deadline at `now`: pending
/// but no longer decidable.
pub fn pending_but_expired(events: &[Event], node_id: &str, now: DateTime<Utc>) -> bool {
    matches!(
        node_approval_status(events, node_id),
        NodeApprovalStatus::Pending(p) if p.deadline().expired(now)
    )
}

/// Whether the request at `request_sequence` has not been escalated yet.
pub fn not_escalated(events: &[Event], node_id: &str, request_sequence: EventSequence) -> bool {
    !events.iter().any(|e| {
        matches!(&e.kind, EventKind::ApprovalEscalated { node_id: n, request_sequence: r, .. }
            if n == node_id && *r == request_sequence)
    })
}

/// Whether `node_id`'s latest request was resolved by its deadline: the
/// request a late decision is refused for.
pub fn expired_by_deadline(events: &[Event], node_id: &str) -> bool {
    let Some(request) = answered_request(events, node_id) else {
        return false;
    };
    events.iter().any(|e| {
        matches!(&e.kind, EventKind::ApprovalExpired { node_id: n, request_sequence, .. }
            if n == node_id && *request_sequence == request.sequence)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ApprovalDecision, EventKind};
    use crate::Event;
    use jamjet_core::workflow::ExecutionId;

    fn ev(seq: u64, kind: EventKind) -> Event {
        Event::new(ExecutionId::new(), seq as EventSequence, kind)
    }
    fn required(seq: u64, node: &str) -> Event {
        ev(
            seq,
            EventKind::ToolApprovalRequired {
                node_id: node.into(),
                tool_name: format!("tool_{node}"),
                approver: "human".into(),
                context: serde_json::json!({}),
            },
        )
    }
    fn received(seq: u64, node: &str, decision: ApprovalDecision) -> Event {
        ev(
            seq,
            EventKind::ApprovalReceived {
                node_id: node.into(),
                user_id: "tester".into(),
                decision,
                comment: Some("note".into()),
                state_patch: None,
                principal: None,
                approved_digest: None,
                request_sequence: None,
            },
        )
    }

    #[test]
    fn no_events_means_not_requested() {
        assert!(matches!(
            node_approval_status(&[], "a"),
            NodeApprovalStatus::NotRequested
        ));
        assert!(pending_approvals(&[]).is_empty());
    }

    #[test]
    fn required_without_decision_is_pending() {
        let events = vec![required(1, "a")];
        match node_approval_status(&events, "a") {
            NodeApprovalStatus::Pending(p) => {
                assert_eq!(p.node_id, "a");
                assert_eq!(p.tool_name, "tool_a");
                assert_eq!(p.sequence, 1);
            }
            other => panic!("expected Pending, got {other:?}"),
        }
        assert_eq!(pending_approvals(&events).len(), 1);
    }

    #[test]
    fn approved_resolves_pending() {
        let events = vec![
            required(1, "a"),
            received(2, "a", ApprovalDecision::Approved),
        ];
        assert!(matches!(
            node_approval_status(&events, "a"),
            NodeApprovalStatus::Approved { .. }
        ));
        assert!(pending_approvals(&events).is_empty());
    }

    #[test]
    fn rejected_resolves_pending_with_comment() {
        let events = vec![
            required(1, "a"),
            received(2, "a", ApprovalDecision::Rejected),
        ];
        match node_approval_status(&events, "a") {
            NodeApprovalStatus::Rejected {
                user_id, comment, ..
            } => {
                assert_eq!(user_id, "tester");
                assert_eq!(comment.as_deref(), Some("note"));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn new_request_after_approval_is_pending_again() {
        let events = vec![
            required(1, "a"),
            received(2, "a", ApprovalDecision::Approved),
            required(3, "a"),
        ];
        assert!(matches!(
            node_approval_status(&events, "a"),
            NodeApprovalStatus::Pending(_)
        ));
    }

    #[test]
    fn decision_without_request_is_ignored() {
        let events = vec![received(1, "a", ApprovalDecision::Approved)];
        assert!(matches!(
            node_approval_status(&events, "a"),
            NodeApprovalStatus::NotRequested
        ));
    }

    #[test]
    fn pending_approvals_order_is_deterministic() {
        // Insert "b" before "a" to verify BTreeMap lexicographic ordering, not insertion order.
        let events = vec![required(1, "b"), required(2, "a")];
        let pending = pending_approvals(&events);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].node_id, "a");
        assert_eq!(pending[1].node_id, "b");
    }

    #[test]
    fn per_node_isolation() {
        let events = vec![
            required(1, "a"),
            required(2, "b"),
            received(3, "a", ApprovalDecision::Approved),
        ];
        assert!(matches!(
            node_approval_status(&events, "a"),
            NodeApprovalStatus::Approved { .. }
        ));
        assert!(matches!(
            node_approval_status(&events, "b"),
            NodeApprovalStatus::Pending(_)
        ));
        let pending = pending_approvals(&events);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].node_id, "b");
    }

    fn request_with(seq: u64, node: &str, ctx: serde_json::Value) -> Event {
        ev(
            seq,
            EventKind::ToolApprovalRequired {
                node_id: node.into(),
                tool_name: format!("tool_{node}"),
                approver: "human".into(),
                context: ctx,
            },
        )
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn expiry(seq: u64, node: &str, request_sequence: i64) -> Event {
        ev(
            seq,
            EventKind::ApprovalExpired {
                node_id: node.into(),
                request_sequence,
                expires_at: at("2026-01-01T00:00:00Z"),
                principal: DEADLINE_PRINCIPAL.into(),
            },
        )
    }

    #[test]
    fn a_deadline_is_reached_at_its_instant() {
        let t = at("2026-01-01T00:00:00Z");
        let d = DeadlineTime::At(t);
        assert!(!d.passed(t - chrono::Duration::milliseconds(1)));
        assert!(d.passed(t), "at the deadline it has passed");
        assert!(d.passed(t + chrono::Duration::milliseconds(1)));
        assert!(!DeadlineTime::Unset.passed(t + chrono::Duration::days(10_000)));
        assert!(DeadlineTime::Unreadable.passed(t - chrono::Duration::days(10_000)));
    }

    #[test]
    fn an_unreadable_deadline_reads_as_passed() {
        for bad in [
            serde_json::json!({ "expires_at": "tomorrow" }),
            serde_json::json!({ "expires_at": 1767225600 }),
            serde_json::json!({ "expires_at": null }),
        ] {
            assert_eq!(
                RequestDeadline::of(&bad).expires_at,
                DeadlineTime::Unreadable,
                "{bad}"
            );
        }
        assert_eq!(
            RequestDeadline::of(&serde_json::json!({})).expires_at,
            DeadlineTime::Unset
        );
    }

    #[test]
    fn a_stamped_deadline_reads_back() {
        let now = at("2026-01-01T00:00:00Z");
        let mut ctx = serde_json::Map::new();
        stamp_deadline(
            &mut ctx,
            &jamjet_ir::workflow::ApprovalDeadline {
                timeout_secs: 600,
                escalate_after_secs: Some(60),
            },
            now,
        )
        .unwrap();
        let d = RequestDeadline::of(&serde_json::Value::Object(ctx.clone()));
        assert_eq!(
            d.expires_at,
            DeadlineTime::At(now + chrono::Duration::seconds(600))
        );
        assert_eq!(
            d.escalate_at,
            DeadlineTime::At(now + chrono::Duration::seconds(60))
        );
        assert_eq!(ctx["expires_at"], "2026-01-01T00:10:00.000Z");
    }

    #[test]
    fn an_expiry_rejects_the_request_it_names() {
        let events = vec![required(1, "a"), expiry(2, "a", 1)];
        match node_approval_status(&events, "a") {
            NodeApprovalStatus::Rejected { user_id, .. } => assert_eq!(user_id, DEADLINE_PRINCIPAL),
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert!(pending_approvals(&events).is_empty());
        assert!(expired_by_deadline(&events, "a"));
        assert!(!still_pending(&events, "a", 1));
    }

    /// An expiry naming an older request resolves nothing: the newer request
    /// stays pending, in both folds.
    #[test]
    fn an_expiry_of_a_superseded_request_resolves_nothing() {
        let events = vec![required(1, "a"), required(2, "a"), expiry(3, "a", 1)];
        assert!(matches!(
            node_approval_status(&events, "a"),
            NodeApprovalStatus::Pending(p) if p.sequence == 2
        ));
        assert_eq!(pending_approvals(&events).len(), 1);
        assert!(still_pending(&events, "a", 2));
        assert!(!expired_by_deadline(&events, "a"));
    }

    #[test]
    fn a_human_rejection_is_not_an_expiry() {
        let events = vec![
            required(1, "a"),
            received(2, "a", ApprovalDecision::Rejected),
        ];
        assert!(!expired_by_deadline(&events, "a"));
    }

    #[test]
    fn due_actions_expire_before_they_escalate_and_escalate_once() {
        let ctx = serde_json::json!({
            "expires_at": "2026-01-01T00:10:00.000Z",
            "escalate_at": "2026-01-01T00:01:00.000Z",
        });
        let events = vec![request_with(1, "a", ctx)];
        assert!(due_deadline_actions(&events, at("2026-01-01T00:00:59Z")).is_empty());
        let esc = due_deadline_actions(&events, at("2026-01-01T00:01:00Z"));
        assert!(matches!(
            esc.as_slice(),
            [DeadlineAction::Escalate {
                request_sequence: 1,
                ..
            }]
        ));
        let mut escalated = events.clone();
        escalated.push(ev(
            2,
            EventKind::ApprovalEscalated {
                node_id: "a".into(),
                request_sequence: 1,
                escalate_at: at("2026-01-01T00:01:00Z"),
                expires_at: at("2026-01-01T00:10:00Z"),
            },
        ));
        assert!(due_deadline_actions(&escalated, at("2026-01-01T00:05:00Z")).is_empty());
        assert!(!not_escalated(&escalated, "a", 1));
        // Past both: expire only.
        let both = due_deadline_actions(&events, at("2026-01-01T00:10:00Z"));
        assert!(matches!(
            both.as_slice(),
            [DeadlineAction::Expire {
                request_sequence: 1,
                ..
            }]
        ));
    }

    #[test]
    fn an_unreadable_event_makes_approval_state_indeterminate() {
        let events = vec![
            required(1, "a"),
            received(2, "a", ApprovalDecision::Approved),
            ev(3, EventKind::Unknown),
        ];
        assert!(matches!(
            node_approval_status(&events, "a"),
            NodeApprovalStatus::Indeterminate
        ));
        assert!(!approval_covers(
            &node_approval_status(&events, "a"),
            "anything"
        ));
        // Before a request too: a later request does not clear it.
        let events = vec![ev(1, EventKind::Unknown), required(2, "a")];
        assert!(matches!(
            node_approval_status(&events, "a"),
            NodeApprovalStatus::Indeterminate
        ));
        // The listing still reads the log.
        assert_eq!(pending_approvals(&events).len(), 1);
    }
}
