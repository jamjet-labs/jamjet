//! The one policy and approval decision for a work item, for every node kind.
//!
//! Two transports hand a node's work to something that performs it: the
//! in-process worker (`Worker::execute_item`) and the HTTP worker protocol
//! (`POST /work-items/claim`). Until 2026-10 only the worker evaluated policy
//! for ordinary nodes; the claim route evaluated it only for agent tool
//! dispatch nodes and released everything else, so an approval-gated tool node
//! claimed over HTTP ran with no approval, and the run's own initiator could
//! use that to get past separation of duties. Both transports now call
//! [`guard_item`]; there is no second copy of the rules to drift.
//!
//! Like [`crate::dispatch_guard`], this module DECIDES and leaves the audit
//! trail; it never settles the work item. Settling is transport-specific.
//!
//! [`GuardMode::Complete`] answers a different question with the same rules:
//! may a worker's reported result for this node be recorded? It never writes.

use crate::dispatch_guard::{self, DispatchGuardOutcome};
use jamjet_agents::AgentRegistry;
use jamjet_core::node::NodeKind;
use jamjet_core::workflow::ExecutionId;
use jamjet_ir::workflow::{ApprovalDeadline, NodeDef, PolicySetIr};
use jamjet_policy::{EvaluationContext, PolicyDecision, PolicyEvaluator};
use jamjet_state::approvals::ApprovalSealer;
use jamjet_state::approvals::{
    action_basis, answered_request, approval_covers, definition_hash, derive_action_digest,
    node_approval_status, stamp_deadline, NodeApprovalStatus, ACTION_BASIS_KEY,
    ACTION_BASIS_STATE_CAP, ACTION_DEFINITION_KEY, ACTION_DIGEST_KEY, ACTION_STEP_KEY,
};
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::Event;
use serde_json::Value;
use tracing::{info, warn};

/// The decision for one work item. Same shape for every node kind, so a
/// caller settles dispatch and ordinary nodes the same way.
pub type GuardOutcome = DispatchGuardOutcome;

/// Which question the guard answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardMode {
    /// May this node FIRE now? Records a `PolicyViolation` on a block and
    /// appends a `ToolApprovalRequired` when an approval is needed and none
    /// covers the action. Used at claim time by both transports.
    Fire,
    /// May a worker's result for this node be RECORDED? Read-only: nothing is
    /// appended, because a refused completion is not a new policy event and
    /// must not create approval requests a worker could farm.
    ///
    /// An approval counts here if it answered a request for the node's CURRENT
    /// occurrence (the request's recorded `step` equals the node's completion
    /// count), not only if its digest still equals the current one. The action
    /// was authorised at claim time; a sibling that commits between that claim
    /// and this completion moves the digest but not the authorisation, and
    /// refusing then would strand an effect that already happened and re-fire
    /// it after a second approval. What this mode must refuse is a completion
    /// with no approval for this occurrence at all (pending, rejected, never
    /// requested, or an approval of an earlier visit), and it does.
    Complete,
}

/// The agents a node acts for, and where their cards are read.
///
/// A node is attributed to the workflow's `agent_ref` and to its own; it must
/// be allowed by the `allowed_tools` of each. `registry` is `None` when the
/// runtime has none, which refuses every attributed node rather than skipping
/// the check.
#[derive(Clone, Copy, Default)]
pub struct AgentScope<'a> {
    /// The workflow-level `agent_ref` (`WorkflowIr::agent_ref`).
    pub workflow_agent: Option<&'a str>,
    /// The workflow's `remote_agents` (`WorkflowIr::remote_agents`): an
    /// `a2a_task` is checked against `allowed_delegations` by the URL its
    /// `remote_agent` maps to. `None` refuses any `a2a_task` under a list.
    pub remote_agents:
        Option<&'a std::collections::HashMap<String, jamjet_ir::workflow::RemoteAgentConfig>>,
    pub registry: Option<&'a dyn AgentRegistry>,
}

/// The policy scope recorded on a `PolicyViolation` raised by an agent's
/// `allowed_tools`.
pub const AGENT_SCOPE: &str = "agent";

/// Every rule recorded for an `allowed_tools` refusal starts with this.
pub const ALLOWED_TOOLS_RULE: &str = "agent allowed_tools";

/// Every rule recorded for a refusal by a card's own `blocked_tools` starts
/// with this.
pub const BLOCKED_TOOLS_RULE: &str = "agent blocked_tools";

/// Every rule recorded for a refusal by a card's `allowed_delegations` starts
/// with this.
pub const ALLOWED_DELEGATIONS_RULE: &str = "agent allowed_delegations";

/// Whom a node delegates to, if it delegates.
enum Delegation<'a> {
    /// The target, as the node or its workflow names it.
    To(&'a str),
    /// The target is chosen at run time (`coordinator`, `agent_discovery`, an
    /// `auto` agent tool), so nothing can be checked.
    RunTime,
    /// An `a2a_task` whose `remote_agent` has no entry in the workflow's
    /// `remote_agents`, so the URL it calls is unknown.
    Unresolved(&'a str),
}

/// `Some` for an `agent` node (its `agent_ref`), an `agent_tool` with an
/// explicit target (that URI), an `a2a_task` (the URL its `remote_agent`
/// maps to in the workflow's `remote_agents`: the author writes both the name
/// and the mapping, so the name alone proves nothing), and the run-time
/// kinds. `None` for every other node: a subgraph runs a child workflow, not
/// an agent.
#[allow(deprecated)]
fn delegation_target<'a>(
    kind: &'a NodeKind,
    remote_agents: Option<
        &'a std::collections::HashMap<String, jamjet_ir::workflow::RemoteAgentConfig>,
    >,
) -> Option<Delegation<'a>> {
    use jamjet_core::agent_tool::AgentTarget;
    match kind {
        NodeKind::Agent { agent_ref, .. } => Some(Delegation::To(agent_ref)),
        NodeKind::AgentTool {
            agent: AgentTarget::Explicit(uri),
            ..
        } => Some(Delegation::To(uri)),
        NodeKind::A2aTask { remote_agent, .. } => {
            Some(match remote_agents.and_then(|m| m.get(remote_agent)) {
                Some(config) => Delegation::To(&config.url),
                None => Delegation::Unresolved(remote_agent),
            })
        }
        NodeKind::AgentTool {
            agent: AgentTarget::Auto,
            ..
        }
        | NodeKind::Coordinator { .. }
        | NodeKind::AgentDiscovery { .. } => Some(Delegation::RunTime),
        _ => None,
    }
}

/// What an agent card's `require_approval_for` gates.
#[derive(Debug, Clone, Default)]
struct CardGate {
    /// The node's own tool, gated: approve the node over its full payload.
    node: Option<String>,
    /// Names of an agent tool dispatch's calls that are gated: raised through
    /// the dispatch guard's per-call request, alongside any chain gates.
    calls: Vec<String>,
}

/// Decide whether `node_def` may fire (or, in `Complete` mode, whether its
/// result may be recorded), evaluating the tenant -> workflow -> node policy
/// chain and, for an approval-gated node, the approval bound to the action.
///
/// `workflow_policy` is the IR's workflow-level policy and `payload` the work
/// item's payload as the engine recorded it.
///
/// The node's OWN names (handler reference and node id, see
/// `EvaluationContext::policy_names`) are evaluated first, for every kind,
/// agent tool dispatch nodes included. A block on them blocks; a gate on them
/// holds the node with an approval bound to the full payload. Only then is a
/// dispatch node's per-call guard applied to the calls in `payload["input"]`.
/// A dispatch node used to skip straight to the per-call guard, so its own
/// names were never checked and an empty call list allowed it outright; the
/// dispatch flag is author-controlled, which made that a bypass of any gate
/// (round-3 review, C1).
///
/// Before any of that, a node attributed to an agent (`agents`) is held to
/// that agent's card: its `allowed_tools`, `blocked_tools` and
/// `require_approval_for`; see [`agent_card`]. These are additional
/// restrictions: the policy chain still has to allow the node, and a card
/// gate holds a node the chain would allow, through the same approval path
/// (same digest binding, the chain's deadline).
#[allow(clippy::too_many_arguments)]
pub async fn guard_item(
    backend: &dyn StateBackend,
    mode: GuardMode,
    execution_id: &ExecutionId,
    node_id: &str,
    tenant_id: &str,
    workflow_policy: Option<&PolicySetIr>,
    node_def: &NodeDef,
    payload: &Value,
    sealer: Option<&dyn ApprovalSealer>,
    agents: AgentScope<'_>,
) -> GuardOutcome {
    // First, so a node the agent may not use never raises an approval request
    // and is refused even when no policy applies.
    let card = match agent_card(
        backend,
        mode,
        execution_id,
        node_id,
        node_def,
        payload,
        agents,
    )
    .await
    {
        Ok(gate) => gate,
        Err(refused) => return refused,
    };
    let card_calls = card.calls.clone();
    let card_gate = card.node;
    let is_dispatch = dispatch_guard::is_agent_tool_dispatch(&node_def.kind);
    // The per-call guard for a dispatch node. `node_approved`: an approval of
    // the node's full payload is on file and covers this item. The calls are
    // part of that payload, so they need no approval of their own; per-call
    // blocks still apply.
    let card_calls = &card_calls;
    let per_call = |node_approved: bool| async move {
        let input = dispatch_guard::payload_input(payload);
        dispatch_guard::guard_dispatch_in(
            backend,
            mode,
            execution_id,
            node_id,
            tenant_id,
            workflow_policy,
            node_def,
            &input,
            node_approved,
            sealer,
            card_calls,
        )
        .await
    };

    // `Ok(None)` (no tenant policy) and `Err` (could not find out) must not
    // collapse: an unreadable tenant would otherwise read as an absent policy
    // and drop that layer's rules.
    let tenant_policy_set = match backend
        .get_tenant(&jamjet_state::TenantId::from(tenant_id))
        .await
    {
        Ok(tenant) => match dispatch_guard::unparseable_tenant_policy(
            backend,
            mode == GuardMode::Fire,
            execution_id,
            node_id,
            tenant_id,
            tenant.map(|t| t.policy_set()),
        )
        .await
        {
            Ok(set) => set,
            Err(blocked) => return blocked,
        },
        Err(e) => {
            warn!(
                execution_id = %execution_id,
                node_id,
                "tenant policy unreadable — refusing to decide on an incomplete policy chain"
            );
            return GuardOutcome::Unavailable {
                reason: format!("tenant policy unavailable; refusing to run unpoliced: {e}"),
            };
        }
    };

    // tenant -> workflow -> node; the evaluator iterates in reverse, so the
    // most specific layer wins.
    let mut sets: Vec<&PolicySetIr> = Vec::new();
    if let Some(ref tp) = tenant_policy_set {
        sets.push(tp);
    }
    if let Some(p) = workflow_policy {
        sets.push(p);
    }
    if let Some(p) = &node_def.policy {
        sets.push(p);
    }
    let ctx = EvaluationContext::from_node_kind(node_id, &node_def.kind);
    let decision = if sets.is_empty() {
        PolicyDecision::Allow
    } else {
        PolicyEvaluator.evaluate(&ctx, &sets)
    };
    // A card gate turns an allow into an approval; a block stays a block, and
    // a chain gate is already one.
    let decision = match (decision, &card_gate) {
        (PolicyDecision::Allow, Some(_)) => PolicyDecision::RequireApproval {
            approver: "human".to_string(),
        },
        (d, _) => d,
    };
    match decision {
        PolicyDecision::Allow if is_dispatch => per_call(false).await,
        PolicyDecision::Allow => GuardOutcome::Allow,

        PolicyDecision::Block { reason } => {
            if mode == GuardMode::Fire {
                let policy_scope = dispatch_guard::identify_policy_scope(
                    &ctx,
                    tenant_policy_set.as_ref(),
                    workflow_policy,
                    node_def.policy.as_ref(),
                );
                warn!(execution_id = %execution_id, node_id, %reason, %policy_scope, "Policy blocked node");
                // Best effort: the block stands whether or not this lands.
                dispatch_guard::record_policy_violation(
                    backend,
                    execution_id,
                    node_id,
                    reason.clone(),
                    policy_scope,
                )
                .await;
            }
            GuardOutcome::Blocked { reason }
        }

        PolicyDecision::RequireApproval { approver } => {
            let tool_name = card_gate
                .clone()
                .or(ctx.tool_name)
                .unwrap_or_else(|| node_id.to_string());
            let outcome = match mode {
                GuardMode::Fire => {
                    // The request's deadline comes from the same chain that
                    // gated the node. A chain whose deadline is invalid (only
                    // reachable past registration and `PUT /tenants`) refuses
                    // the node: raising the request with no deadline would
                    // leave it open forever under a policy that says it
                    // expires.
                    let deadline =
                        match deadline_or_block(backend, execution_id, node_id, &sets).await {
                            Ok(d) => d,
                            Err(blocked) => return blocked,
                        };
                    require_approval(
                        backend,
                        execution_id,
                        node_id,
                        node_def,
                        payload,
                        tool_name,
                        approver,
                        serde_json::json!({ "node_id": node_id }),
                        deadline,
                        tenant_id,
                        sealer,
                    )
                    .await
                }
                GuardMode::Complete => {
                    completion_covered(backend, execution_id, node_id, node_def, payload, tool_name)
                        .await
                }
            };
            match outcome {
                GuardOutcome::Allow if is_dispatch => per_call(true).await,
                other => other,
            }
        }
    }
}

/// The agent card half of the decision. `Ok(None)`: every agent the node
/// acts for allows it outright (or it acts for none). `Ok(Some(name))`: some
/// card's `require_approval_for` names it (as `name`), so the node must be
/// approved. `Err`: the refusal.
///
/// Fails closed. An attributed node is refused (`Blocked`, recorded with scope
/// [`AGENT_SCOPE`] on a firing check) when the runtime has no agent registry
/// or the agent is not registered, and held back (`Unavailable`, nothing
/// recorded) when the registry cannot be read. Empty lists restrict nothing
/// (`jamjet_policy::allowed_tools`).
///
/// Per card, in order: `allowed_tools` (a node not on a non-empty list is
/// refused), `blocked_tools` (a node named by it is refused, even if also on
/// `allowed_tools`), `require_approval_for` (a node named by it needs an
/// approval). All three match the same identities: an agent tool dispatch
/// node's pending calls (name and resolved handler), read from the same
/// payload the dispatch consumes; every other node's own tool name. A node
/// marked as a dispatch but not at the ADK dispatcher's own coordinates is
/// matched on both, because the marker is the author's to set: without that,
/// marking any function node as a dispatch would exempt its own code.
async fn agent_card(
    backend: &dyn StateBackend,
    mode: GuardMode,
    execution_id: &ExecutionId,
    node_id: &str,
    node_def: &NodeDef,
    payload: &Value,
    agents: AgentScope<'_>,
) -> Result<CardGate, GuardOutcome> {
    use jamjet_policy::allowed_tools::{call_allowed, call_named_by, node_named_by, CallRefused};

    let mut refs: Vec<&str> = Vec::new();
    for r in agents
        .workflow_agent
        .into_iter()
        .chain(node_def.agent_ref.as_deref())
    {
        if !refs.contains(&r) {
            refs.push(r);
        }
    }
    let Some(&first) = refs.first() else {
        return Ok(CardGate::default());
    };

    let refuse = |rule: String| async move {
        warn!(execution_id = %execution_id, node_id, %rule, "agent card refused node");
        if mode == GuardMode::Fire {
            dispatch_guard::record_policy_violation(
                backend,
                execution_id,
                node_id,
                rule.clone(),
                AGENT_SCOPE.to_string(),
            )
            .await;
        }
        Err(GuardOutcome::Blocked { reason: rule })
    };

    let Some(registry) = agents.registry else {
        return refuse(format!(
            "{ALLOWED_TOOLS_RULE}: node acts for agent '{first}', but this runtime has no \
             agent registry to read its card"
        ))
        .await;
    };

    // Only the ADK dispatcher itself is exempt from matching its own name.
    let calls_only = match &node_def.kind {
        NodeKind::PythonFn {
            module, function, ..
        } => {
            dispatch_guard::is_agent_tool_dispatch(&node_def.kind)
                && jamjet_ir::is_adk_dispatch_coordinates(module, function)
        }
        _ => false,
    };
    let is_dispatch = dispatch_guard::is_agent_tool_dispatch(&node_def.kind);
    let ctx = EvaluationContext::from_node_kind(node_id, &node_def.kind);
    let mut gate = CardGate::default();

    for agent_ref in refs {
        let card = match registry.get_by_uri(agent_ref).await {
            Ok(Some(agent)) => agent.card,
            Ok(None) => {
                return refuse(format!(
                    "{ALLOWED_TOOLS_RULE}: agent '{agent_ref}' is not registered, so its \
                     card cannot be checked"
                ))
                .await;
            }
            Err(e) => {
                warn!(execution_id = %execution_id, node_id, agent_ref, "agent registry unreadable");
                return Err(GuardOutcome::Unavailable {
                    reason: format!(
                        "agent registry unavailable; refusing to run without checking the \
                         card of agent '{agent_ref}': {e}"
                    ),
                });
            }
        };
        let Some(c) = card.constraints.as_ref() else {
            continue;
        };
        let (allowed, blocked, gated) = (
            c.allowed_tools.as_slice(),
            c.blocked_tools.as_slice(),
            c.require_approval_for.as_slice(),
        );

        // Delegation: the target as the node names it must be on a
        // non-empty `allowed_delegations` (glob patterns, matched as
        // written). A node that picks its target at run time cannot be
        // checked and is refused under a list.
        if !c.allowed_delegations.is_empty() {
            match delegation_target(&node_def.kind, agents.remote_agents) {
                Some(Delegation::To(target)) => {
                    if !c
                        .allowed_delegations
                        .iter()
                        .any(|p| jamjet_policy::glob_matches(p, target))
                    {
                        return refuse(format!(
                            "{ALLOWED_DELEGATIONS_RULE}: agent '{agent_ref}' may not delegate \
                             to '{target}'"
                        ))
                        .await;
                    }
                }
                Some(Delegation::Unresolved(name)) => {
                    return refuse(format!(
                        "{ALLOWED_DELEGATIONS_RULE}: agent '{agent_ref}' has a delegation list, \
                         and remote agent '{name}' is not in the workflow's remote_agents, so \
                         the URL it calls is unknown"
                    ))
                    .await;
                }
                Some(Delegation::RunTime) => {
                    return refuse(format!(
                        "{ALLOWED_DELEGATIONS_RULE}: agent '{agent_ref}' has a delegation list, \
                         and a {} node picks its target at run time",
                        ctx.node_kind_tag
                    ))
                    .await;
                }
                None => {}
            }
        }

        if allowed.is_empty() && blocked.is_empty() && gated.is_empty() {
            continue;
        }

        if !calls_only {
            if let Err(tool) = jamjet_policy::allowed_tools::node_allowed(&ctx, allowed) {
                return refuse(format!(
                    "{ALLOWED_TOOLS_RULE}: tool '{tool}' is not allowed for agent '{agent_ref}'"
                ))
                .await;
            }
            if let Some(tool) = node_named_by(&ctx, blocked) {
                return refuse(format!(
                    "{BLOCKED_TOOLS_RULE}: tool '{tool}' is blocked for agent '{agent_ref}'"
                ))
                .await;
            }
            if gate.node.is_none() {
                gate.node = node_named_by(&ctx, gated);
            }
        }
        if is_dispatch {
            let input = dispatch_guard::payload_input(payload);
            let calls = match jamjet_policy::dispatch::read_pending_tool_calls(&input) {
                Ok(calls) => calls,
                Err(cause) => return refuse(dispatch_guard::unreadable_rule(cause)).await,
            };
            for call in &calls {
                match call_allowed(call, allowed) {
                    Ok(()) => {}
                    Err(CallRefused::Name(tool)) => {
                        return refuse(format!(
                            "{ALLOWED_TOOLS_RULE}: tool '{tool}' is not allowed for agent \
                             '{agent_ref}'"
                        ))
                        .await;
                    }
                    Err(CallRefused::Handler { name, handler }) => {
                        return refuse(format!(
                            "{ALLOWED_TOOLS_RULE}: tool '{name}' resolves to '{handler}', which \
                             is not allowed for agent '{agent_ref}'"
                        ))
                        .await;
                    }
                }
                if let Some(tool) = call_named_by(call, blocked) {
                    return refuse(format!(
                        "{BLOCKED_TOOLS_RULE}: tool '{tool}' is blocked for agent '{agent_ref}'"
                    ))
                    .await;
                }
                if call_named_by(call, gated).is_some() && !gate.calls.contains(&call.name) {
                    gate.calls.push(call.name.clone());
                }
            }
        }
    }
    Ok(gate)
}

/// The approval half of the decision, for a node that needs one: `Allow` if an
/// approval covers the action this item would perform (this node, this
/// occurrence, dispatched with `payload`), otherwise `Held`
/// with an outstanding request (an existing one, or one appended here bound to
/// the current action digest). Reads that the decision depends on fail CLOSED
/// as `Unavailable`.
///
/// Shared by the policy path above and the worker's autonomy path, so both
/// request and match approvals identically.
///
/// `deadline`: the request, if one is raised here, records when it expires
/// and escalates (see `approvals::stamp_deadline`); `None` raises it with no
/// deadline. A request raised here is sealed into the audit chain by
/// `sealer`, for `tenant_id`.
#[allow(clippy::too_many_arguments)]
pub async fn require_approval(
    backend: &dyn StateBackend,
    execution_id: &ExecutionId,
    node_id: &str,
    node_def: &NodeDef,
    payload: &Value,
    tool_name: String,
    approver: String,
    context: Value,
    deadline: Option<ApprovalDeadline>,
    tenant_id: &str,
    sealer: Option<&dyn ApprovalSealer>,
) -> GuardOutcome {
    let gated = vec![tool_name.clone()];
    let events = match backend.get_events(execution_id).await {
        Ok(events) => events,
        Err(e) => {
            return GuardOutcome::Unavailable {
                reason: format!("approval state unavailable; refusing to run unapproved: {e}"),
            }
        }
    };

    let status = node_approval_status(&events, node_id);
    if matches!(status, NodeApprovalStatus::Indeterminate) {
        // Fail closed: never release, never raise a request over a log this
        // build cannot fully read.
        return GuardOutcome::Unavailable {
            reason: jamjet_state::approvals::UNREADABLE_LOG_REASON.to_string(),
        };
    }
    if matches!(
        status,
        NodeApprovalStatus::Pending(_) | NodeApprovalStatus::Rejected { .. }
    ) {
        // Already requested, or decided rejected (the scheduler owns the
        // failure). Hold; emit nothing.
        return GuardOutcome::Held { gated };
    }

    // Approved or NotRequested: what would this item do right now?
    let definition = match node_definition(node_def) {
        Ok(d) => d,
        Err(reason) => return GuardOutcome::Unavailable { reason },
    };
    let basis = match action_basis(backend, execution_id, node_id, &definition, payload).await {
        Ok(b) => b,
        Err(e) => {
            return GuardOutcome::Unavailable {
                reason: format!("approval binding unavailable; refusing to run unapproved: {e}"),
            }
        }
    };
    let current = basis.digest(node_id);

    if approval_covers(&status, &current) {
        info!(execution_id = %execution_id, node_id, "Approval satisfied — proceeding");
        return GuardOutcome::Allow;
    }

    if let NodeApprovalStatus::Approved {
        approved_digest, ..
    } = &status
    {
        warn!(
            execution_id = %execution_id,
            node_id,
            approved_digest = approved_digest.as_deref().unwrap_or("<none: legacy approval>"),
            current_digest = %current,
            "approval does not cover this action (state changed, a later visit of the \
             node, or a legacy approval without a digest) — requesting a fresh approval"
        );
    } else {
        info!(execution_id = %execution_id, node_id, %approver, "Node requires approval");
    }

    let mut context = context;
    if !context.is_object() {
        context = serde_json::json!({ "context": context });
    }
    if let Some(obj) = context.as_object_mut() {
        obj.insert(ACTION_DIGEST_KEY.to_string(), Value::String(current));
        obj.insert(ACTION_DEFINITION_KEY.to_string(), Value::String(definition));
        // The occurrence this request is for. `GuardMode::Complete` reads it.
        obj.insert(ACTION_STEP_KEY.to_string(), Value::from(basis.step));
        // What the reviewer is approving: the payload the digest covers (the
        // input the node is dispatched with) and the node it would run. Shown
        // in the pending listing.
        obj.insert(
            ACTION_BASIS_KEY.to_string(),
            basis_preview(&basis.payload, basis.step, node_def),
        );
        if let Some(deadline) = &deadline {
            if let Err(e) = stamp_deadline(obj, deadline, chrono::Utc::now()) {
                // Never raise a request without the deadline its policy set.
                return GuardOutcome::Unavailable {
                    reason: format!("approval required but its deadline could not be set: {e}"),
                };
            }
        }
    }

    // KNOWN LIMIT, unchanged: read-then-append with no compare-and-set, so two
    // concurrent claims for two items of one node can both append. Fail closed
    // either way: both return Held. See `dispatch_guard::guard_dispatch`.
    let request = Event::new(
        execution_id.clone(),
        0,
        EventKind::ToolApprovalRequired {
            node_id: node_id.to_string(),
            tool_name,
            approver,
            context,
        },
    );
    if let Err(reason) = record_request(backend, sealer, request, tenant_id).await {
        // An unrecorded request is an invisible hold: fail closed.
        return GuardOutcome::Unavailable { reason };
    }
    GuardOutcome::Held { gated }
}

/// Append an approval request and seal it into the audit chain: the one
/// place a request is recorded, for both guards and both transports. The
/// seal follows the durable append and cannot undo it; a failed append is
/// `Err` and the caller fails closed.
pub(crate) async fn record_request(
    backend: &dyn StateBackend,
    sealer: Option<&dyn ApprovalSealer>,
    request: Event,
    tenant_id: &str,
) -> Result<(), String> {
    let appended = backend
        .append_event(request.clone())
        .await
        .map_err(|e| format!("approval required but could not be recorded: {e}"))?;
    jamjet_state::approvals::seal_appended(backend, sealer, request, appended, tenant_id).await;
    Ok(())
}

/// The deadline a request raised under `sets` carries, or the refusal when
/// the chain's deadline settings are invalid (recorded as a violation, like an
/// unparseable tenant policy). Shared with the agent tool dispatch guard.
pub(crate) async fn deadline_or_block(
    backend: &dyn StateBackend,
    execution_id: &ExecutionId,
    node_id: &str,
    sets: &[&PolicySetIr],
) -> Result<Option<ApprovalDeadline>, GuardOutcome> {
    match ApprovalDeadline::resolve(sets) {
        Ok(r) => {
            if let Some(dropped) = r.dropped_escalation_secs {
                warn!(
                    execution_id = %execution_id,
                    node_id,
                    escalate_after_secs = dropped,
                    timeout_secs = r.deadline.map(|d| d.timeout_secs),
                    "approval escalation dropped: it would not come before the effective \
                     deadline (the timeout is never extended to fit it)"
                );
            }
            Ok(r.deadline)
        }
        Err(cause) => {
            let reason = format!(
                "approval deadline policy is invalid ({cause}); the node is refused until \
                 the policy is fixed"
            );
            warn!(execution_id = %execution_id, node_id, %reason, "refusing node");
            dispatch_guard::record_policy_violation(
                backend,
                execution_id,
                node_id,
                reason.clone(),
                "unknown".to_string(),
            )
            .await;
            Err(GuardOutcome::Blocked { reason })
        }
    }
}

/// `GuardMode::Complete` for an approval-gated node: `Allow` only if the
/// node's approval answered a request for its current occurrence.
async fn completion_covered(
    backend: &dyn StateBackend,
    execution_id: &ExecutionId,
    node_id: &str,
    node_def: &NodeDef,
    payload: &Value,
    tool_name: String,
) -> GuardOutcome {
    let gated = vec![tool_name];
    let unavailable = |e: jamjet_state::backend::StateBackendError| GuardOutcome::Unavailable {
        reason: format!("approval state unavailable; refusing to record an unapproved result: {e}"),
    };
    let events = match backend.get_events(execution_id).await {
        Ok(events) => events,
        Err(e) => return unavailable(e),
    };
    let status = node_approval_status(&events, node_id);
    if !matches!(status, NodeApprovalStatus::Approved { .. }) {
        return GuardOutcome::Held { gated };
    }
    let Some(request) = answered_request(&events, node_id) else {
        return GuardOutcome::Held { gated };
    };
    let step = match backend.count_node_completions(execution_id, node_id).await {
        Ok(s) => s,
        Err(e) => return unavailable(e),
    };
    match request.context.get(ACTION_STEP_KEY).and_then(Value::as_u64) {
        Some(requested_step) if requested_step == step => GuardOutcome::Allow,
        Some(_) => GuardOutcome::Held { gated },
        // A request recorded before the step was: only an exact digest match
        // proves it is this occurrence.
        None => match match node_definition(node_def) {
            Ok(definition) => {
                derive_action_digest(backend, execution_id, node_id, &definition, payload).await
            }
            Err(reason) => return GuardOutcome::Unavailable { reason },
        } {
            Ok(current) if approval_covers(&status, &current) => GuardOutcome::Allow,
            Ok(_) => GuardOutcome::Held { gated },
            Err(e) => unavailable(e),
        },
    }
}

/// The [`definition_hash`] of `node_def` as registered. Serializing a
/// `NodeDef` does not fail in practice; if it ever did, the decision fails
/// closed rather than binding an approval to nothing.
pub(crate) fn node_definition(node_def: &NodeDef) -> Result<String, String> {
    serde_json::to_value(node_def)
        .map(|v| definition_hash(&v))
        .map_err(|e| format!("node definition unreadable; refusing to run unapproved: {e}"))
}

/// The reviewer's view of what a request binds: the dispatched payload, its
/// `input` included (dropped and flagged above [`ACTION_BASIS_STATE_CAP`]
/// serialized bytes), the occurrence, and a summary of the node definition
/// (id, kind with its module, function or tool, description and own policy).
pub(crate) fn basis_preview(payload: &Value, step: u64, node_def: &NodeDef) -> Value {
    let payload_bytes = serde_json::to_string(payload).map(|s| s.len()).unwrap_or(0);
    let truncated = payload_bytes > ACTION_BASIS_STATE_CAP;
    serde_json::json!({
        "payload": if truncated { Value::Null } else { payload.clone() },
        "payload_truncated": truncated,
        "payload_bytes": payload_bytes,
        "step": step,
        "node": {
            "id": node_def.id,
            "kind": node_def.kind,
            "description": node_def.description,
            "policy": node_def.policy,
        },
    })
}
