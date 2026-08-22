//! The shared enforcement decision for ADK agent tool-dispatch nodes.
//!
//! An ADK agent compiles a whole turn's model-chosen tool calls into one
//! `python_fn` / `java_fn` node, so the tool names are invisible to the ordinary
//! node-kind policy path and every `blocked_tools` / `require_approval_for` rule
//! silently misses (review finding C1). This module owns the decision that
//! closes that hole, and the audit trail the decision leaves behind.
//!
//! It deliberately does NOT settle the work item. Settling differs by transport:
//! the in-process worker completes a held item so its lease never expires into
//! the retry path, while the HTTP claim route is handing the item OUT and must
//! not complete an item its caller is simultaneously claiming. Keeping the
//! settle at the call site is what lets one decision serve both.
//!
//! Nothing here may reference `Worker`, worker-local state, lease fences or
//! `WorkItemId` — those are worker lifecycle concerns, and depending on them
//! would re-couple the decision to one transport.

use jamjet_core::node::NodeKind;
use jamjet_core::workflow::ExecutionId;
use jamjet_ir::workflow::{NodeDef, PolicySetIr, WorkflowIr};
use jamjet_policy::dispatch::{DispatchDecision, UnreadableCalls};
use jamjet_policy::{EvaluationContext, PolicyDecision, PolicyEvaluator};
use jamjet_state::approvals::NodeApprovalStatus;
use jamjet_state::backend::StateBackend;
use jamjet_state::content_hash;
use jamjet_state::event::EventKind;
use jamjet_state::Event;
use serde_json::Value;
use tracing::{info, warn};

/// The rule string recorded when a marked dispatch node's pending calls cannot
/// be read. Every cause-specific rule starts with this, so the phrase stays a
/// stable needle for anything matching on the family rather than the cause.
const UNREADABLE: &str = "agent dispatch tool calls unreadable";

/// The rule string recorded when a settled approval does not authorise the calls
/// that are actually pending.
const APPROVAL_MISMATCH: &str = "approved tool calls do not match pending calls";

/// What the guard decided about one agent dispatch batch.
///
/// This is deliberately NOT the worker's
/// `Option<Result<(), Box<dyn Error + Send + Sync>>>`, which encodes worker
/// lifecycle ("None = proceed, Some(Err) = fail this node, Some(Ok) = held and
/// already settled"). A caller has to be able to tell a policy denial — audit
/// it, refuse the payload, do not retry — from state it could not read, which
/// is a transient infrastructure failure that should be retried and must NOT
/// pollute the Prove surface with a denial that no policy made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchGuardOutcome {
    /// Policy permits the batch. The caller may run it.
    Allow,
    /// Policy denied the batch. A `PolicyViolation` has already been recorded
    /// (best effort — the denial stands either way). `reason` is the audit rule,
    /// not a user-facing message.
    Blocked { reason: String },
    /// The batch needs human approval and must not run. An outstanding
    /// `ToolApprovalRequired` exists for the node — this call either found one
    /// or appended it. The caller settles the work item however its transport
    /// requires.
    Held { gated: Vec<String> },
    /// The state needed to decide could not be read or the decision could not be
    /// recorded. The caller MUST NOT run the batch. No `PolicyViolation` is
    /// recorded, because no policy denied — this is infrastructure, and the
    /// caller should retry rather than audit a denial that never happened.
    Unavailable { reason: String },
}

/// True for nodes that run a whole agent turn's model-chosen tool calls.
///
/// TWO signals, and the second is not redundant. The marker is declarative and
/// covers both languages, but it is `#[serde(default)]`, so an IR compiled
/// before the marker existed deserializes to `false`.
/// `validate_agent_tool_dispatch` makes that drift loud — on the REGISTRATION
/// route only. Nothing re-validates a workflow already sitting in the backend,
/// so a workflow registered before this change and started from stored IR (a
/// cron/scheduled fleet, or any `start_execution` without a fresh
/// `create_workflow`) would reach the executor unmarked and run every tool
/// unpoliced. That is C1 exactly, still open, for precisely the population that
/// cannot be fixed by validating new registrations.
///
/// So enforcement falls back to the dispatch coordinates, which identify the
/// node with no marker at all — the same rule the validator matches on, shared
/// from one definition. The marker stays the cheap path; it is no longer the
/// only one.
///
/// NARROWNESS is preserved: an ordinary `python_fn` that merely lacks the marker
/// is still not a dispatch node. Only the ADK dispatch coroutine's own
/// coordinates qualify. Java keeps the marker as its sole signal because no Java
/// ADK compiler emits `java_fn` dispatch nodes yet, so there is no pre-marker
/// population to rescue — add the coordinate pair here when that compiler lands.
pub fn is_agent_tool_dispatch(kind: &NodeKind) -> bool {
    match kind {
        NodeKind::PythonFn {
            agent_tool_dispatch: true,
            ..
        }
        | NodeKind::JavaFn {
            agent_tool_dispatch: true,
            ..
        } => true,
        NodeKind::PythonFn {
            module, function, ..
        } => jamjet_ir::is_adk_dispatch_coordinates(module, function),
        _ => false,
    }
}

/// The frozen accumulated state the scheduler enriched onto a PythonFn/JavaFn
/// work item (`runtime/scheduler/src/runner.rs:441-480`). This is the exact
/// input the dispatch will consume, which is what makes policy evaluation over
/// it free of a time-of-check-to-time-of-use window.
///
/// It MUST be `payload["input"]`, never `payload`. The pending calls live one
/// level down; reading the whole payload would leave both call keys absent, so
/// the calls would read as "nothing to run" and every tool would be allowed
/// while the code read like a correct allow. When the key is missing, this
/// yields `Value::Null`, which the reader rejects as unreadable.
pub fn payload_input(payload: &Value) -> Value {
    payload.get("input").cloned().unwrap_or(Value::Null)
}

/// Decide whether a marked agent dispatch node may run, and emit the audit
/// trail. **The CALLER settles the work item.**
///
/// The frozen `payload_input` is the same bytes the dispatch will consume, so
/// there is no time-of-check-to-time-of-use window: this deliberately does NOT
/// re-read the node's input from the backend or from a snapshot.
#[allow(clippy::too_many_arguments)]
pub async fn guard_dispatch(
    backend: &dyn StateBackend,
    execution_id: &ExecutionId,
    node_id: &str,
    tenant_id: &str,
    ir: &WorkflowIr,
    node_def: &NodeDef,
    payload_input: &Value,
) -> DispatchGuardOutcome {
    // Load tenant policy (sits between global and workflow in the chain).
    //
    // `Ok(None)` — this tenant has no policy — and `Err` — we could not find out
    // — must NOT collapse together. Swallowing the error makes an unreadable
    // tenant record look like an absent policy: when the tenant layer is the
    // whole chain, `sets` empties and the guard returns `Allow` before it even
    // reads the pending calls, so a tenant-level `blocked_tools` rule goes
    // unenforced on a transient backend blip. That contradicts this module's own
    // contract, which is that unreadable state yields `Unavailable` and the
    // caller retries — never a decision no policy made.
    let tenant_policy_set = match backend
        .get_tenant(&jamjet_state::TenantId::from(tenant_id))
        .await
    {
        Ok(tenant) => tenant.and_then(|t| t.policy_set()),
        Err(e) => {
            warn!(
                execution_id = %execution_id,
                node_id,
                "tenant policy unreadable — refusing to decide"
            );
            return DispatchGuardOutcome::Unavailable {
                reason: format!("tenant policy unavailable; refusing to run unpoliced: {e}"),
            };
        }
    };

    // Build policy chain: tenant -> workflow -> node (least-specific to
    // most-specific). The evaluator iterates in reverse, so node rules win.
    let mut sets: Vec<&PolicySetIr> = Vec::new();
    if let Some(ref tp) = tenant_policy_set {
        sets.push(tp);
    }
    if let Some(p) = &ir.policy {
        sets.push(p);
    }
    if let Some(p) = &node_def.policy {
        sets.push(p);
    }
    if sets.is_empty() {
        return DispatchGuardOutcome::Allow;
    }

    let calls = match jamjet_policy::dispatch::read_pending_tool_calls(payload_input) {
        Ok(calls) => calls,
        Err(cause) => {
            // Fail closed: never run a node whose policy-relevant state we
            // cannot read. `Ok(vec![])` (genuinely nothing to run) is distinct
            // and falls through to Allow below — do not collapse the two.
            let rule = unreadable_rule(cause);
            warn!(
                execution_id = %execution_id,
                node_id,
                rule,
                "agent dispatch tool calls unreadable — refusing to run"
            );
            // A fail-closed refusal is a policy denial and must reach the audit
            // surface like any other. Scope is "unknown" because the engine's
            // fail-closed rule denied here, not a policy layer.
            record_policy_violation(
                backend,
                execution_id,
                node_id,
                rule.clone(),
                "unknown".to_string(),
            )
            .await;
            return DispatchGuardOutcome::Blocked { reason: rule };
        }
    };

    match jamjet_policy::dispatch::evaluate_dispatch(node_id, &calls, &sets) {
        DispatchDecision::Allow => DispatchGuardOutcome::Allow,

        DispatchDecision::Block { tool_name, reason } => {
            // Attribute the scope using the same per-call context
            // `evaluate_dispatch` matched on; a node-kind context carries no
            // tool_name, so reusing one would record every dispatch block as
            // "unknown".
            let policy_scope = identify_policy_scope(
                &tool_ctx(node_id, &tool_name),
                tenant_policy_set.as_ref(),
                ir.policy.as_ref(),
                node_def.policy.as_ref(),
            );
            warn!(execution_id = %execution_id, node_id, %tool_name, %reason, %policy_scope, "Policy blocked agent tool call");
            record_policy_violation(backend, execution_id, node_id, reason.clone(), policy_scope)
                .await;
            DispatchGuardOutcome::Blocked { reason }
        }

        DispatchDecision::RequireApproval { approver, gated } => {
            // Bind the approval to the exact calls it authorises, so a settled
            // approval cannot be replayed against a different payload
            // (approval-loop hardening: decision not bound to resolved params).
            //
            // COVERAGE: the bound material is `name` + `arguments` per call, in
            // order, and nothing else. Every other field a producer may carry on
            // a raw call — `id` above all, plus any provider-specific extras —
            // is deliberately UNBOUND, so a re-fire that differs only in call id
            // still matches the approval. Widening this to the raw call objects
            // would bind the approval to fields that carry no authority and
            // would make benign re-issues fail the check.
            let calls_json = serde_json::json!(calls
                .iter()
                .map(|c| serde_json::json!({"name": c.name, "arguments": c.arguments}))
                .collect::<Vec<_>>());
            let current_hash = content_hash(&calls_json);

            // ONE read of the event log, for the whole approval decision.
            //
            // The binding check and the "is there already a request?" check both
            // need approval state. Reading twice is safe under a lease, which
            // serializes one work item to one worker — but this unit also runs
            // on the HTTP claim route, where two concurrent claims for the same
            // node could both pass the binding check in the window between two
            // reads and then diverge on the second. One read cannot straddle a
            // write, so everything below decides from the same snapshot.
            let events = match backend.get_events(execution_id).await {
                Ok(events) => events,
                Err(e) => {
                    // Fail closed: never run an approval-gated node blind. This
                    // is infrastructure, not a denial, so nothing is audited.
                    return DispatchGuardOutcome::Unavailable {
                        reason: format!(
                            "approval state unavailable; refusing to run unapproved: {e}"
                        ),
                    };
                }
            };

            match jamjet_state::approvals::node_approval_status(&events, node_id) {
                NodeApprovalStatus::Approved { .. } => {
                    // An existing approval authorised one specific call set. If
                    // the pending calls differ, the approval must not carry over
                    // — otherwise a settled approval authorises whatever happens
                    // to be pending when the node re-fires.
                    let approved_hash = latest_request_calls_hash(&events, node_id);
                    if approved_hash.as_deref() != Some(current_hash.as_str()) {
                        warn!(
                            execution_id = %execution_id,
                            node_id,
                            "approved call set does not match pending calls — refusing"
                        );
                        // The highest-value audit record here: an approval was on
                        // file but did not authorise these calls. The approval
                        // requirement came from an identifiable layer, so name it.
                        let policy_scope = gated
                            .first()
                            .map(|tool| {
                                identify_policy_scope(
                                    &tool_ctx(node_id, tool),
                                    tenant_policy_set.as_ref(),
                                    ir.policy.as_ref(),
                                    node_def.policy.as_ref(),
                                )
                            })
                            .unwrap_or_else(|| "unknown".to_string());
                        record_policy_violation(
                            backend,
                            execution_id,
                            node_id,
                            APPROVAL_MISMATCH.to_string(),
                            policy_scope,
                        )
                        .await;
                        return DispatchGuardOutcome::Blocked {
                            reason: APPROVAL_MISMATCH.to_string(),
                        };
                    }
                    info!(execution_id = %execution_id, node_id, "Approval satisfied — proceeding");
                    DispatchGuardOutcome::Allow
                }

                // Already requested (or decided rejected — the scheduler owns
                // the failure). Hold; emit nothing.
                NodeApprovalStatus::Pending(_) | NodeApprovalStatus::Rejected { .. } => {
                    DispatchGuardOutcome::Held { gated }
                }

                NodeApprovalStatus::NotRequested => {
                    // KNOWN LIMIT: reading `NotRequested` and then appending the
                    // request is a read-modify-write with no compare-and-set,
                    // the same shape as `record_policy_violation` below — but
                    // NOT the same consequence, so do not carry that note's
                    // "cosmetic" reading over to here.
                    //
                    // Under a lease exactly one worker holds a work item, so the
                    // worker path is safe. On the HTTP claim route, two
                    // concurrent claims for the same node can each read
                    // `NotRequested` from their own snapshot and each append a
                    // `ToolApprovalRequired`. The damage is a DUPLICATE
                    // OUTSTANDING APPROVAL REQUEST, not a colliding sequence
                    // number: `node_approval_status` resets to `Pending` on every
                    // new request, so a human's settled decision would refer to a
                    // request that has already been superseded and could never
                    // stick. That is precisely the invariant
                    // `an_already_pending_node_is_held_without_a_duplicate_request`
                    // exists to protect, and today it holds only because a lease
                    // keeps the writer single.
                    //
                    // The backend trait offers no conditional/CAS append, so this
                    // cannot be closed here — it needs a primitive that makes
                    // "append iff no open request for this node" atomic, and the
                    // route path must supply it. Fail-closed is preserved either
                    // way: both racers return `Held`, so nothing runs unapproved.
                    info!(execution_id = %execution_id, node_id, %approver, "Node requires approval");
                    // `gated` carries tool NAMES only: two calls to the same
                    // gated tool render as ["send_wire", "send_wire"] with no
                    // discriminator. `calls` is what identifies them.
                    let context = serde_json::json!({
                        "node_id": node_id,
                        "gated_tools": gated,
                        "calls": calls_json,
                        "calls_hash": current_hash,
                    });
                    let seq = match backend.latest_sequence(execution_id).await {
                        Ok(s) => s + 1,
                        Err(e) => {
                            return DispatchGuardOutcome::Unavailable {
                                reason: format!("approval required but could not be recorded: {e}"),
                            }
                        }
                    };
                    if let Err(e) = backend
                        .append_event(Event::new(
                            execution_id.clone(),
                            seq,
                            EventKind::ToolApprovalRequired {
                                node_id: node_id.to_string(),
                                tool_name: gated.join(", "),
                                approver,
                                context,
                            },
                        ))
                        .await
                    {
                        // Fail closed: an unrecorded request is an invisible hold.
                        return DispatchGuardOutcome::Unavailable {
                            reason: format!("approval required but could not be recorded: {e}"),
                        };
                    }
                    DispatchGuardOutcome::Held { gated }
                }
            }
        }
    }
}

/// The per-call evaluation context `evaluate_dispatch` matched on.
///
/// `"tool"`, not `"python_fn"`: only the model-allowlist branch keys on the tag
/// (`runtime/policy/src/lib.rs:62`), and `"tool"` is what makes `blocked_tools` /
/// `require_approval_for` read naturally here. This MUST stay identical to the
/// context built in `jamjet_policy::dispatch::evaluate_dispatch`, or scope
/// attribution names a layer that did not decide.
fn tool_ctx(node_id: &str, tool_name: &str) -> EvaluationContext {
    EvaluationContext {
        node_id: node_id.to_string(),
        node_kind_tag: "tool".to_string(),
        tool_name: Some(tool_name.to_string()),
        model_ref: None,
    }
}

/// The audit rule for each way a dispatch payload can be unreadable.
///
/// The prefix is applied here rather than written into each arm, so the family
/// phrase cannot drift on one cause and quietly stop matching: the decision is
/// identical for all of them, so anything keyed on the family must keep
/// matching, while the Prove surface gains the cause.
fn unreadable_rule(cause: UnreadableCalls) -> String {
    let detail = match cause {
        UnreadableCalls::NotAnObject => "input is not an object",
        UnreadableCalls::NotAList => "the call list is not an array",
        UnreadableCalls::UnnamedCall => "a call has no readable name",
        UnreadableCalls::NameTooLong => "a call name exceeds the length bound",
        UnreadableCalls::TooManyCalls => "the call list exceeds the batch bound",
    };
    format!("{UNREADABLE}: {detail}")
}

/// The `calls_hash` bound to the LATEST approval request for a node.
///
/// `node_approval_status` resets to Pending on each new request, so an
/// `Approved` status always refers to the latest request. Find that event FIRST,
/// then read its hash — a `find_map` over the hash would skip a hash-less latest
/// request and silently inherit an older, superseded request's hash, which is a
/// fail-open. Selecting the event first makes an absent hash yield `None`, which
/// never equals `Some(current)` and blocks.
fn latest_request_calls_hash(events: &[Event], node_id: &str) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|e| {
            matches!(
                &e.kind,
                EventKind::ToolApprovalRequired { node_id: n, .. } if n == node_id
            )
        })
        .and_then(|e| match &e.kind {
            EventKind::ToolApprovalRequired { context, .. } => context
                .get("calls_hash")
                .and_then(|h| h.as_str())
                .map(|s| s.to_string()),
            _ => None,
        })
}

/// Record a policy denial in the event log, best effort.
///
/// The denial is enforced by the caller regardless of whether this record lands:
/// fail closed, never open. Used by every deny path in the guard — the
/// policy-rule block AND the two fail-closed refusals (unreadable calls,
/// approved-set mismatch) — so that no refusal is invisible to audit.
///
/// KNOWN LIMIT: `latest_sequence() + 1` is a read-modify-write with no
/// compare-and-set. Under a lease exactly one worker holds a work item, so the
/// sequence cannot be raced. On the HTTP claim route, two concurrent claims for
/// the same execution can compute the same next sequence. The backend trait
/// offers no CAS append, so this is not fixable here; it is a known limit of the
/// route path, and it affects only the audit record's sequence — never whether
/// the denial is enforced.
///
/// That last sentence is about THIS append and no other. The `NotRequested` arm
/// of `guard_dispatch` performs a read-modify-write of the same shape whose
/// consequence is materially worse — duplicate outstanding approval requests,
/// which can stop a human decision from ever settling. See the KNOWN LIMIT note
/// there before concluding the route path is only cosmetically affected.
async fn record_policy_violation(
    backend: &dyn StateBackend,
    execution_id: &ExecutionId,
    node_id: &str,
    rule: String,
    policy_scope: String,
) {
    if let Ok(latest) = backend.latest_sequence(execution_id).await {
        let _ = backend
            .append_event(Event::new(
                execution_id.clone(),
                latest + 1,
                EventKind::PolicyViolation {
                    node_id: node_id.to_string(),
                    rule,
                    decision: "blocked".to_string(),
                    policy_scope,
                },
            ))
            .await;
    }
}

/// Determine which policy scope triggered a non-Allow decision.
///
/// Checks each scope individually from most-specific (node) to least-specific
/// (tenant), returning the name of the first scope that produces a non-Allow
/// decision.
pub fn identify_policy_scope(
    ctx: &EvaluationContext,
    tenant_policy: Option<&PolicySetIr>,
    workflow_policy: Option<&PolicySetIr>,
    node_policy: Option<&PolicySetIr>,
) -> String {
    // Check most-specific first (same order as evaluator's reverse iteration).
    if let Some(p) = node_policy {
        if !matches!(PolicyEvaluator.evaluate(ctx, &[p]), PolicyDecision::Allow) {
            return "node".to_string();
        }
    }
    if let Some(p) = workflow_policy {
        if !matches!(PolicyEvaluator.evaluate(ctx, &[p]), PolicyDecision::Allow) {
            return "workflow".to_string();
        }
    }
    if let Some(p) = tenant_policy {
        if !matches!(PolicyEvaluator.evaluate(ctx, &[p]), PolicyDecision::Allow) {
            return "tenant".to_string();
        }
    }
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FailingGetEvents;
    use jamjet_state::tenant::{Tenant, TenantStatus};
    use jamjet_state::InMemoryBackend;
    use serde_json::json;
    use std::sync::Arc;

    // ── Fixtures ──────────────────────────────────────────────────────────────

    /// A single marked agent-dispatch node, optionally under a workflow policy.
    fn ir_with(policy: Option<serde_json::Value>) -> WorkflowIr {
        let mut doc = json!({
            "workflow_id": "adk-wf",
            "version": "1.0.0",
            "state_schema": "{}",
            "start_node": "n1",
            "nodes": { "n1": { "id": "n1", "kind": {
                "type": "python_fn",
                "module": "jamjet.agents.tool_runtime",
                "function": "dispatch_tool_calls",
                "output_schema": "",
                "agent_tool_dispatch": true
            }}},
            "edges": [],
            "retry_policies": {},
            "models": {},
            "tools": {},
            "mcp_servers": {},
            "remote_agents": {}
        });
        if let Some(p) = policy {
            doc["policy"] = p;
        }
        serde_json::from_value(doc).expect("dispatch IR must deserialize")
    }

    fn policy_json(blocked: &[&str], approval: &[&str]) -> serde_json::Value {
        json!({
            "blocked_tools": blocked,
            "require_approval_for": approval,
            "model_allowlist": []
        })
    }

    /// Workflow-level policy — the ordinary shape.
    fn ir(blocked: &[&str], approval: &[&str]) -> WorkflowIr {
        ir_with(Some(policy_json(blocked, approval)))
    }

    /// No policy anywhere: the empty-chain case.
    fn unpoliced_ir() -> WorkflowIr {
        ir_with(None)
    }

    fn one_call(name: &str) -> serde_json::Value {
        json!({"id": "c1", "name": name, "arguments": {}})
    }

    fn input_for(names: &[&str]) -> serde_json::Value {
        json!({"tool_calls": names.iter().map(|n| one_call(n)).collect::<Vec<_>>()})
    }

    /// The exact `calls_json` shape the guard hashes. Both the recorded approval
    /// context and the re-fire check must hash this same shape, or the binding
    /// never matches and an approved node can never proceed.
    fn calls_json_for(names: &[&str]) -> serde_json::Value {
        json!(names
            .iter()
            .map(|n| json!({"name": n, "arguments": {}}))
            .collect::<Vec<_>>())
    }

    struct Fixture {
        backend: Arc<InMemoryBackend>,
        execution_id: ExecutionId,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                backend: Arc::new(InMemoryBackend::new()),
                execution_id: ExecutionId::new(),
            }
        }

        async fn seed(&self, kinds: Vec<EventKind>) {
            for kind in kinds {
                // InMemoryBackend assigns the sequence itself; 0 is a placeholder.
                self.backend
                    .append_event(Event::new(self.execution_id.clone(), 0, kind))
                    .await
                    .unwrap();
            }
        }

        async fn with_tenant_policy(&self, blocked: &[&str], approval: &[&str]) {
            let now = chrono::Utc::now();
            self.backend
                .create_tenant(Tenant {
                    id: "default".into(),
                    name: "default".into(),
                    status: TenantStatus::Active,
                    policy: Some(policy_json(blocked, approval)),
                    limits: None,
                    created_at: now,
                    updated_at: now,
                })
                .await
                .unwrap();
        }

        async fn guard(&self, ir: &WorkflowIr, input: &serde_json::Value) -> DispatchGuardOutcome {
            let node_def = ir.node("n1").expect("node n1 must exist");
            guard_dispatch(
                self.backend.as_ref(),
                &self.execution_id,
                "n1",
                "default",
                ir,
                node_def,
                input,
            )
            .await
        }

        async fn events(&self) -> Vec<Event> {
            self.backend.get_events(&self.execution_id).await.unwrap()
        }

        /// Every `PolicyViolation` as `(rule, policy_scope)`.
        async fn violations(&self) -> Vec<(String, String)> {
            self.events()
                .await
                .iter()
                .filter_map(|e| match &e.kind {
                    EventKind::PolicyViolation {
                        rule,
                        decision,
                        policy_scope,
                        ..
                    } => {
                        assert_eq!(decision, "blocked", "a violation is always a block");
                        Some((rule.clone(), policy_scope.clone()))
                    }
                    _ => None,
                })
                .collect()
        }

        /// Every `ToolApprovalRequired` as `(tool_name, approver, context)`.
        async fn requests(&self) -> Vec<(String, String, serde_json::Value)> {
            self.events()
                .await
                .iter()
                .filter_map(|e| match &e.kind {
                    EventKind::ToolApprovalRequired {
                        tool_name,
                        approver,
                        context,
                        ..
                    } => Some((tool_name.clone(), approver.clone(), context.clone())),
                    _ => None,
                })
                .collect()
        }
    }

    // ── A backend that cannot read its own event log ──────────────────────────

    /// An unreadable TENANT record is infrastructure too, and collapsing it into
    /// "this tenant has no policy" is a fail-open.
    ///
    /// The tenant layer is the only policy here, so treating a backend error as
    /// an absent policy empties the chain, and the guard returns `Allow` without
    /// even reading the pending calls — a tenant-level `blocked_tools` rule
    /// silently not enforced, on the transport ADK nodes actually take. The
    /// module contract is that state the guard cannot read yields `Unavailable`,
    /// never a decision.
    #[tokio::test]
    async fn an_unreadable_tenant_record_is_unavailable_not_allow() {
        let backend = FailingGetEvents::failing_tenant();
        let execution_id = ExecutionId::new();
        // No workflow and no node policy: the tenant layer is the whole chain,
        // which is what makes a swallowed error decide the outcome.
        let ir = ir(&[], &[]);
        let node_def = ir.node("n1").expect("node n1 must exist");

        let outcome = guard_dispatch(
            &backend,
            &execution_id,
            "n1",
            "default",
            &ir,
            node_def,
            &input_for(&["send_wire"]),
        )
        .await;

        match &outcome {
            DispatchGuardOutcome::Unavailable { reason } => assert!(
                reason.contains("injected: tenant record unreadable"),
                "the underlying failure must survive into the reason; got {reason:?}"
            ),
            other => panic!("an unreadable tenant record must be Unavailable, got {other:?}"),
        }
        assert!(
            backend.recorded(&execution_id).await.is_empty(),
            "no policy denied anything, so nothing may be audited"
        );
    }

    /// An unreadable event log is infrastructure, not a decision.
    ///
    /// The outcome assertion is half the point; the audit assertion is the other
    /// half and the more important one. Returning `Blocked` here would record a
    /// `PolicyViolation` claiming a policy denied the batch, when in truth no
    /// policy was ever consulted — a false denial on the Prove surface, and a
    /// permanent one, since the caller would stop retrying something that only
    /// needed a working backend.
    #[tokio::test]
    async fn an_unreadable_event_log_is_unavailable_and_records_no_denial() {
        let backend = FailingGetEvents::new();
        let execution_id = ExecutionId::new();
        let ir = ir(&[], &["send_wire"]);
        let node_def = ir.node("n1").expect("node n1 must exist");

        let outcome = guard_dispatch(
            &backend,
            &execution_id,
            "n1",
            "default",
            &ir,
            node_def,
            &input_for(&["send_wire"]),
        )
        .await;

        match &outcome {
            DispatchGuardOutcome::Unavailable { reason } => {
                assert!(
                    reason.contains("injected: event log unreadable"),
                    "the underlying failure must survive into the reason; got {reason:?}"
                );
            }
            other => {
                panic!("a backend failure is infrastructure, not a policy denial; got {other:?}")
            }
        }

        let recorded = backend.recorded(&execution_id).await;
        assert!(
            !recorded
                .iter()
                .any(|e| matches!(e.kind, EventKind::PolicyViolation { .. })),
            "no policy denied — a PolicyViolation here is a false denial in the \
             audit log; got {recorded:?}"
        );
        assert!(
            recorded.is_empty(),
            "an undecidable dispatch writes nothing at all; got {recorded:?}"
        );
    }

    fn approved(node_id: &str) -> EventKind {
        EventKind::ApprovalReceived {
            node_id: node_id.into(),
            user_id: "approver".into(),
            decision: jamjet_state::event::ApprovalDecision::Approved,
            comment: None,
            state_patch: None,
        }
    }

    fn rejected(node_id: &str) -> EventKind {
        EventKind::ApprovalReceived {
            node_id: node_id.into(),
            user_id: "approver".into(),
            decision: jamjet_state::event::ApprovalDecision::Rejected,
            comment: None,
            state_patch: None,
        }
    }

    fn request_for(names: &[&str], hash: Option<String>) -> EventKind {
        let mut context = json!({
            "node_id": "n1",
            "gated_tools": names,
            "calls": calls_json_for(names),
        });
        if let Some(h) = hash {
            context["calls_hash"] = json!(h);
        }
        EventKind::ToolApprovalRequired {
            node_id: "n1".into(),
            tool_name: names.join(", "),
            approver: "human".into(),
            context,
        }
    }

    // ── Allow ─────────────────────────────────────────────────────────────────

    /// The availability dual of the whole guard, and the most common production
    /// shape: an agent calling a permitted tool while a real policy is in force.
    /// An implementation that denied on Allow would pass every deny test here.
    #[tokio::test]
    async fn a_permitted_tool_is_allowed_and_audits_nothing() {
        let f = Fixture::new();
        let outcome = f
            .guard(
                &ir(&["send_wire"], &["wire_batch"]),
                &input_for(&["read_file"]),
            )
            .await;
        assert_eq!(outcome, DispatchGuardOutcome::Allow);
        assert!(
            f.events().await.is_empty(),
            "an allowed dispatch must write no events"
        );
    }

    /// `Ok(vec![])` (genuinely nothing to run) must stay distinct from an
    /// unreadable payload at THIS seam, not only inside the reader.
    #[tokio::test]
    async fn an_empty_call_list_is_allowed_not_unreadable() {
        let f = Fixture::new();
        let outcome = f
            .guard(&ir(&["send_wire"], &[]), &json!({"tool_calls": []}))
            .await;
        assert_eq!(outcome, DispatchGuardOutcome::Allow);
        assert!(f.violations().await.is_empty());
    }

    /// An empty policy chain has nothing to enforce, so the guard steps aside
    /// before it ever reads the payload. This mirrors the worker's long-standing
    /// short-circuit; changing it would make an unpoliced workflow start failing
    /// on payload shape.
    #[tokio::test]
    async fn an_empty_policy_chain_allows() {
        let f = Fixture::new();
        assert_eq!(
            f.guard(&unpoliced_ir(), &input_for(&["send_wire"])).await,
            DispatchGuardOutcome::Allow
        );
        // Even an unreadable payload: with no policy there is no decision to make.
        assert_eq!(
            f.guard(&unpoliced_ir(), &json!({"tool_calls": "nope"}))
                .await,
            DispatchGuardOutcome::Allow
        );
        assert!(f.events().await.is_empty());
    }

    // ── Block ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_blocked_tool_is_blocked_and_audited_exactly_once() {
        let f = Fixture::new();
        let outcome = f
            .guard(
                &ir(&["send_wire"], &[]),
                &input_for(&["read_file", "send_wire"]),
            )
            .await;
        match outcome {
            DispatchGuardOutcome::Blocked { reason } => {
                assert!(
                    reason.contains("matches blocked pattern 'send_wire'"),
                    "the audit rule must name the rule that matched; got {reason:?}"
                );
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
        let violations = f.violations().await;
        assert_eq!(
            violations.len(),
            1,
            "exactly one PolicyViolation, got {violations:?}"
        );
        assert_eq!(violations[0].1, "workflow", "the blocking layer is named");
    }

    /// Fail closed: a batch holding both a blocked and a gated call must block,
    /// never merely hold.
    #[tokio::test]
    async fn a_block_beats_an_approval_gate() {
        let f = Fixture::new();
        let outcome = f
            .guard(
                &ir(&["drop_table"], &["send_wire"]),
                &input_for(&["send_wire", "drop_table"]),
            )
            .await;
        assert!(matches!(outcome, DispatchGuardOutcome::Blocked { .. }));
        assert!(f.requests().await.is_empty(), "a blocked batch is not held");
    }

    /// The tenant layer is part of the chain the guard builds. A guard that
    /// only read the workflow and node policies would allow this.
    #[tokio::test]
    async fn a_tenant_policy_blocks_and_is_named_as_the_scope() {
        let f = Fixture::new();
        f.with_tenant_policy(&["send_wire"], &[]).await;
        let outcome = f.guard(&unpoliced_ir(), &input_for(&["send_wire"])).await;
        assert!(matches!(outcome, DispatchGuardOutcome::Blocked { .. }));
        assert_eq!(f.violations().await[0].1, "tenant");
    }

    // ── Unreadable ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_non_array_call_list_is_blocked() {
        let f = Fixture::new();
        let outcome = f
            .guard(
                &ir(&["send_wire"], &[]),
                &json!({"tool_calls": "send_wire"}),
            )
            .await;
        assert!(matches!(outcome, DispatchGuardOutcome::Blocked { .. }));
        assert_eq!(f.violations().await.len(), 1);
    }

    #[tokio::test]
    async fn a_nameless_call_is_blocked() {
        let f = Fixture::new();
        let outcome = f
            .guard(
                &ir(&["send_wire"], &[]),
                &json!({"tool_calls": [{"id": "c1", "arguments": {}}]}),
            )
            .await;
        assert!(matches!(outcome, DispatchGuardOutcome::Blocked { .. }));
        assert_eq!(f.violations().await.len(), 1);
    }

    /// A non-object input is unreadable, not empty — the live shape of a missing
    /// `input` key, which `payload_input` renders as `Value::Null`.
    #[tokio::test]
    async fn a_non_object_input_is_blocked() {
        let f = Fixture::new();
        for input in [Value::Null, json!("send_wire"), json!([one_call("x")])] {
            let outcome = f.guard(&ir(&["send_wire"], &[]), &input).await;
            assert!(
                matches!(outcome, DispatchGuardOutcome::Blocked { .. }),
                "{input} must be unreadable, got {outcome:?}"
            );
        }
    }

    /// The end-to-end shape a route or worker actually hits: a payload with no
    /// `input` key at all, plus a top-level decoy. Reading the whole payload
    /// instead of `payload["input"]` would make this readable and allow it.
    #[tokio::test]
    async fn an_absent_input_key_is_blocked() {
        let f = Fixture::new();
        let payload = json!({"workflow_id": "adk-wf", "tool_calls": []});
        let outcome = f
            .guard(&ir(&["send_wire"], &[]), &payload_input(&payload))
            .await;
        assert!(matches!(outcome, DispatchGuardOutcome::Blocked { .. }));
        assert_eq!(f.violations().await.len(), 1);
    }

    /// Audit fidelity: a production denial has to be attributable. Three
    /// different malformations previously recorded one indistinguishable rule.
    /// Each must still carry the family phrase, because that is the needle
    /// existing consumers match on.
    #[tokio::test]
    async fn each_unreadable_cause_records_a_distinct_rule() {
        let cases = [
            Value::Null,
            json!({"tool_calls": "send_wire"}),
            json!({"tool_calls": [{"id": "c1"}]}),
        ];
        let mut rules = Vec::new();
        for input in cases {
            let f = Fixture::new();
            let outcome = f.guard(&ir(&["send_wire"], &[]), &input).await;
            let DispatchGuardOutcome::Blocked { reason } = outcome else {
                panic!("{input} must block");
            };
            let recorded = f.violations().await;
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].0, reason, "the audit rule is the denial reason");
            assert!(
                reason.starts_with(UNREADABLE),
                "every cause keeps the family phrase; got {reason:?}"
            );
            rules.push(reason);
        }
        rules.sort();
        rules.dedup();
        assert_eq!(rules.len(), 3, "the three causes must be distinguishable");
    }

    // ── Approval ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_gated_tool_is_held_and_requests_approval_exactly_once() {
        let f = Fixture::new();
        let outcome = f
            .guard(&ir(&[], &["send_wire"]), &input_for(&["send_wire"]))
            .await;
        assert_eq!(
            outcome,
            DispatchGuardOutcome::Held {
                gated: vec!["send_wire".into()]
            }
        );
        let requests = f.requests().await;
        assert_eq!(requests.len(), 1, "exactly one request, got {requests:?}");
        assert!(f.violations().await.is_empty(), "a hold is not a violation");
    }

    /// `gated` is what a human approver is shown as the tools they are
    /// authorising, so membership is load-bearing: naming the whole batch would
    /// over-report, naming none would under-report.
    #[tokio::test]
    async fn one_gated_call_in_a_batch_of_three_holds_and_names_only_that_call() {
        let f = Fixture::new();
        let outcome = f
            .guard(
                &ir(&[], &["send_wire"]),
                &input_for(&["read_file", "send_wire", "log"]),
            )
            .await;
        assert_eq!(
            outcome,
            DispatchGuardOutcome::Held {
                gated: vec!["send_wire".into()]
            }
        );
    }

    /// The approval is bound to the exact calls it authorises, so a settled
    /// approval cannot be replayed against a different payload.
    #[tokio::test]
    async fn the_request_context_carries_the_gated_tools_calls_and_a_hash() {
        let f = Fixture::new();
        f.guard(
            &ir(&[], &["send_wire"]),
            &input_for(&["read_file", "send_wire"]),
        )
        .await;
        let requests = f.requests().await;
        let (tool_name, approver, context) = &requests[0];
        assert_eq!(tool_name, "send_wire", "tool_name lists the gated calls");
        assert_eq!(approver, "human");
        assert_eq!(context["node_id"], "n1");
        assert_eq!(context["gated_tools"], json!(["send_wire"]));
        assert_eq!(
            context["calls"],
            calls_json_for(&["read_file", "send_wire"]),
            "the WHOLE batch is bound, not just the gated call"
        );
        assert_eq!(
            context["calls_hash"],
            json!(content_hash(&calls_json_for(&["read_file", "send_wire"]))),
            "the hash must cover the same shape the re-fire check recomputes"
        );
    }

    /// Re-entering an already-pending node must not stack up requests: an
    /// approver would see duplicates and `node_approval_status` would keep
    /// resetting to Pending, so a settled decision could never stick.
    #[tokio::test]
    async fn an_already_pending_node_is_held_without_a_duplicate_request() {
        let f = Fixture::new();
        let first = f
            .guard(&ir(&[], &["send_wire"]), &input_for(&["send_wire"]))
            .await;
        let second = f
            .guard(&ir(&[], &["send_wire"]), &input_for(&["send_wire"]))
            .await;
        assert!(matches!(first, DispatchGuardOutcome::Held { .. }));
        assert!(matches!(second, DispatchGuardOutcome::Held { .. }));
        assert_eq!(
            f.requests().await.len(),
            1,
            "the second pass must not append a second request"
        );
    }

    /// A rejected node holds too: the scheduler owns turning a rejection into a
    /// failure. Re-requesting here would reopen a decision a human already made.
    #[tokio::test]
    async fn a_rejected_node_is_held_without_a_new_request() {
        let f = Fixture::new();
        f.seed(vec![request_for(&["send_wire"], None), rejected("n1")])
            .await;
        let outcome = f
            .guard(&ir(&[], &["send_wire"]), &input_for(&["send_wire"]))
            .await;
        assert!(matches!(outcome, DispatchGuardOutcome::Held { .. }));
        assert_eq!(
            f.requests().await.len(),
            1,
            "no new request for a rejection"
        );
    }

    /// The dual of the binding check: an approval DOES release the exact call
    /// set it authorised. Without this, blocking always would satisfy the check.
    #[tokio::test]
    async fn an_approval_releases_the_call_set_it_authorised() {
        let f = Fixture::new();
        let hash = content_hash(&calls_json_for(&["send_wire"]));
        f.seed(vec![
            request_for(&["send_wire"], Some(hash)),
            approved("n1"),
        ])
        .await;
        let outcome = f
            .guard(&ir(&[], &["send_wire"]), &input_for(&["send_wire"]))
            .await;
        assert_eq!(outcome, DispatchGuardOutcome::Allow);
        assert_eq!(
            f.requests().await.len(),
            1,
            "an approved node is not re-requested"
        );
        assert!(f.violations().await.is_empty());
    }

    /// An approval authorises a specific set of calls, not the node forever.
    #[tokio::test]
    async fn an_approval_does_not_authorise_a_different_call_set() {
        let f = Fixture::new();
        let hash = content_hash(&calls_json_for(&["read_file"]));
        f.seed(vec![
            request_for(&["read_file"], Some(hash)),
            approved("n1"),
        ])
        .await;
        let outcome = f
            .guard(
                &ir(&[], &["send_wire", "read_file"]),
                &input_for(&["send_wire"]),
            )
            .await;
        assert_eq!(
            outcome,
            DispatchGuardOutcome::Blocked {
                reason: APPROVAL_MISMATCH.into()
            }
        );
        let violations = f.violations().await;
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].0, APPROVAL_MISMATCH);
        assert_eq!(
            violations[0].1, "workflow",
            "the approval requirement came from an identifiable layer, so the \
             mismatch record must name it rather than shrug with \"unknown\""
        );
    }

    /// The approved hash must come from the LATEST request for the node, and an
    /// absent hash on that request must block.
    ///
    /// `node_approval_status` resets to Pending on every new request, so an
    /// `Approved` status always refers to the latest one. A lookup that scans
    /// backwards for the first event *yielding a hash* walks straight past a
    /// hash-less latest request and inherits an older, superseded request's
    /// hash — a fail-open. The pending calls below deliberately MATCH the older
    /// request, so that lookup would allow and only the correct one blocks.
    #[tokio::test]
    async fn a_hashless_latest_request_does_not_inherit_an_older_hash() {
        let f = Fixture::new();
        let old = content_hash(&calls_json_for(&["read_file"]));
        f.seed(vec![
            request_for(&["read_file"], Some(old)),
            request_for(&["read_file"], None),
            approved("n1"),
        ])
        .await;
        let outcome = f
            .guard(&ir(&[], &["read_file"]), &input_for(&["read_file"]))
            .await;
        assert_eq!(
            outcome,
            DispatchGuardOutcome::Blocked {
                reason: APPROVAL_MISMATCH.into()
            }
        );
        assert_eq!(f.violations().await.len(), 1);
    }

    // ── Narrowness ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn only_marked_nodes_are_agent_tool_dispatch() {
        let marked = ir(&[], &[]);
        assert!(is_agent_tool_dispatch(&marked.node("n1").unwrap().kind));

        let unmarked: WorkflowIr = serde_json::from_value(json!({
            "workflow_id": "wf", "version": "1.0.0", "state_schema": "{}",
            "start_node": "n1",
            "nodes": { "n1": { "id": "n1", "kind": {
                "type": "python_fn", "module": "m", "function": "f",
                "output_schema": "", "agent_tool_dispatch": false }}},
            "edges": [], "retry_policies": {}, "models": {}, "tools": {},
            "mcp_servers": {}, "remote_agents": {}
        }))
        .unwrap();
        assert!(!is_agent_tool_dispatch(&unmarked.node("n1").unwrap().kind));
    }

    /// An IR stored before the marker existed deserializes to
    /// `agent_tool_dispatch: false`, and nothing re-validates a workflow that is
    /// already in the backend — `validate_agent_tool_dispatch` runs on the
    /// REGISTRATION route only. Keying enforcement solely on the marker would
    /// therefore leave C1 fully open for every ADK workflow registered before
    /// this change and started from stored IR (a cron/scheduled fleet, or any
    /// `start_execution` without a fresh `create_workflow`).
    ///
    /// The dispatch coordinates identify the node without the marker — that is
    /// exactly what the registration validator matches on — so enforcement keys
    /// on them too. The marker stays as the cheap path, not the only signal.
    #[tokio::test]
    async fn unmarked_adk_dispatch_coordinates_are_still_guarded() {
        let unmarked: WorkflowIr = serde_json::from_value(json!({
            "workflow_id": "wf", "version": "1.0.0", "state_schema": "{}",
            "start_node": "n1",
            "nodes": { "n1": { "id": "n1", "kind": {
                "type": "python_fn",
                "module": "jamjet.agents.tool_runtime",
                "function": "dispatch_tool_calls",
                "output_schema": "", "agent_tool_dispatch": false }}},
            "edges": [], "retry_policies": {}, "models": {}, "tools": {},
            "mcp_servers": {}, "remote_agents": {}
        }))
        .unwrap();
        assert!(
            is_agent_tool_dispatch(&unmarked.node("n1").unwrap().kind),
            "an unmarked node carrying the ADK dispatch coordinates must still be guarded"
        );
    }

    #[test]
    fn payload_input_reads_one_level_down() {
        assert_eq!(payload_input(&json!({"input": {"a": 1}})), json!({"a": 1}));
        assert_eq!(payload_input(&json!({"tool_calls": []})), Value::Null);
        assert_eq!(payload_input(&Value::Null), Value::Null);
    }
}
