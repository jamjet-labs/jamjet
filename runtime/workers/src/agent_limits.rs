//! An agent card's autonomy limits, enforced from the run's event log.
//!
//! A node that acts for an agent (the workflow's `agent_ref` or the node's
//! own) is one STEP of that agent. When the shared guard
//! ([`crate::node_guard::guard_item`]) lets such a node fire, it records
//! [`EventKind::AgentStepAdmitted`] naming the agents and the tool calls the
//! step makes. Everything the limits read is folded from the log:
//!
//! - iterations: admitted steps;
//! - tool calls: the admitted steps' `tool_calls` (one for a node that uses
//!   a tool, each call of an agent tool dispatch batch, none for a model or
//!   condition node);
//! - tokens and cost: `input_tokens + output_tokens` and `cost_usd` on the
//!   `NodeCompleted` that ends an admitted step;
//! - consecutive errors: admitted steps ending in `NodeFailed` since the
//!   agent's last completed step;
//! - time: since the agent's first admitted step.
//!
//! The fold covers every segment of the run (`segment_chain`), so a restart
//! or a continue-as-new rollover does not start the counters again, and
//! nothing a node returns (its `state_patch`) can reach them. The step is
//! recorded with `append_event_if`, whose condition re-runs the same check on
//! the log as it stands at the append, so two claims cannot both take the
//! last step.
//!
//! What a step reports about itself (tokens, cost, success or failure) is the
//! executor's or, over HTTP, the worker's word: a worker that under-reports
//! or reports a failed call as completed is believed, as it is everywhere
//! else in the worker protocol.

use chrono::{DateTime, Utc};
use jamjet_agents::card::{AutonomyConstraints, OnLimitExceeded};
use jamjet_core::workflow::ExecutionId;
use jamjet_policy::autonomy::{self, AgentUsage, AutonomyDecision, Step};
use jamjet_state::backend::{ConditionalAppend, StateBackend};
use jamjet_state::event::EventKind;
use jamjet_state::Event;
use serde_json::Value;
use std::collections::HashMap;
use tracing::warn;

use crate::dispatch_guard::DispatchGuardOutcome as GuardOutcome;

/// Every rule recorded for a refusal by an agent's autonomy limits starts
/// with this.
pub const AUTONOMY_RULE: &str = "agent autonomy";

/// The agents a node acts for, each with its card's constraints (`None`: a
/// card with none, where only the default circuit breaker applies).
pub(crate) type Agents = Vec<(String, Option<AutonomyConstraints>)>;

/// What one agent has used, as folded from the log.
#[derive(Debug, Clone, Default)]
struct Spent {
    iterations: u64,
    tool_calls: u64,
    tokens: u64,
    cost_usd: f64,
    consecutive_errors: u32,
    first_step_at: Option<DateTime<Utc>>,
}

/// The fold of a run's log into each agent's usage.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ledger {
    agents: HashMap<String, Spent>,
    /// Admitted steps that have not ended yet, by node: the agents each was
    /// admitted for, oldest first. A `NodeCompleted` or `NodeFailed` of the
    /// node ends the oldest.
    open: HashMap<String, Vec<Vec<String>>>,
}

/// The log holds an event this build cannot read: it may be a step, so the
/// counters cannot be known.
#[derive(Debug)]
pub(crate) struct Unreadable;

impl Ledger {
    /// Fold one segment's events. Steps are matched to their ends within a
    /// segment, so the open steps are dropped when the segment changes.
    fn apply_segment(&mut self, events: &[Event]) -> Result<(), Unreadable> {
        self.open.clear();
        for e in events {
            self.apply(e)?;
        }
        Ok(())
    }

    fn apply(&mut self, e: &Event) -> Result<(), Unreadable> {
        match &e.kind {
            EventKind::AgentStepAdmitted {
                node_id,
                agents,
                tool_calls,
                ..
            } => {
                for a in agents {
                    let s = self.agents.entry(a.clone()).or_default();
                    s.iterations = s.iterations.saturating_add(1);
                    s.tool_calls = s.tool_calls.saturating_add(u64::from(*tool_calls));
                    if s.first_step_at.is_none() {
                        s.first_step_at = Some(e.created_at);
                    }
                }
                self.open
                    .entry(node_id.clone())
                    .or_default()
                    .push(agents.clone());
            }
            EventKind::NodeCompleted {
                node_id,
                input_tokens,
                output_tokens,
                cost_usd,
                ..
            } => {
                let Some(agents) = self.end(node_id) else {
                    return Ok(());
                };
                let tokens = input_tokens
                    .unwrap_or(0)
                    .saturating_add(output_tokens.unwrap_or(0));
                // A cost that is not a finite number cannot be summed into
                // something the budget can trust: count it as spending all
                // of it. A negative cost is refused where it is reported and
                // would refund the budget here, so it counts as nothing.
                let cost = match cost_usd {
                    Some(c) if !c.is_finite() => f64::INFINITY,
                    Some(c) if *c > 0.0 => *c,
                    _ => 0.0,
                };
                for a in agents {
                    let s = self.agents.entry(a).or_default();
                    s.tokens = s.tokens.saturating_add(tokens);
                    s.cost_usd += cost;
                    s.consecutive_errors = 0;
                }
            }
            EventKind::NodeFailed { node_id, .. } => {
                if let Some(agents) = self.end(node_id) {
                    for a in agents {
                        let s = self.agents.entry(a).or_default();
                        s.consecutive_errors = s.consecutive_errors.saturating_add(1);
                    }
                }
            }
            EventKind::Unknown => return Err(Unreadable),
            _ => {}
        }
        Ok(())
    }

    /// End the node's oldest open step, returning its agents.
    fn end(&mut self, node_id: &str) -> Option<Vec<String>> {
        let open = self.open.get_mut(node_id)?;
        if open.is_empty() {
            return None;
        }
        Some(open.remove(0))
    }

    fn usage(&self, agent: &str, now: DateTime<Utc>) -> AgentUsage {
        let s = self.agents.get(agent).cloned().unwrap_or_default();
        AgentUsage {
            iterations: s.iterations,
            tool_calls: s.tool_calls,
            tokens: s.tokens,
            cost_usd: s.cost_usd,
            consecutive_errors: s.consecutive_errors,
            elapsed_ms: s
                .first_step_at
                .map(|t| u64::try_from((now - t).num_milliseconds()).unwrap_or(0)),
        }
    }
}

/// The first agent whose limits stop `step`, with what its card says to do
/// about it. A `fail` card wins over an `escalate_to_human` one: a refusal
/// is the stricter answer.
fn first_stop(
    ledger: &Ledger,
    agents: &Agents,
    step: Step,
    now: DateTime<Utc>,
) -> Option<(String, AutonomyDecision, OnLimitExceeded)> {
    let mut escalation = None;
    for (agent, constraints) in agents {
        let decision = autonomy::check(constraints.as_ref(), &ledger.usage(agent, now), step);
        if decision == AutonomyDecision::Proceed {
            continue;
        }
        let on_limit = constraints
            .as_ref()
            .and_then(|c| c.on_limit_exceeded)
            .unwrap_or_default();
        match on_limit {
            OnLimitExceeded::Fail => return Some((agent.clone(), decision, on_limit)),
            OnLimitExceeded::EscalateToHuman => {
                if escalation.is_none() {
                    escalation = Some((agent.clone(), decision, on_limit));
                }
            }
        }
    }
    escalation
}

/// Whether any agent whose card says `fail` stops `step`. An approved
/// escalation releases a step past an escalating card's limit, never past a
/// failing card's.
fn fail_stop(ledger: &Ledger, agents: &Agents, step: Step, now: DateTime<Utc>) -> bool {
    agents.iter().any(|(agent, constraints)| {
        let on_limit = constraints
            .as_ref()
            .and_then(|c| c.on_limit_exceeded)
            .unwrap_or_default();
        on_limit == OnLimitExceeded::Fail
            && autonomy::check(constraints.as_ref(), &ledger.usage(agent, now), step)
                != AutonomyDecision::Proceed
    })
}

/// The answered request of `node_id` that can release one step past an
/// agent's limit, by sequence: approved, raised for that limit (its context
/// carries `autonomy_limit`, so the reviewer saw it), and not used by an
/// earlier step. `None` when there is none.
pub(crate) fn unused_escalation(events: &[Event], node_id: &str) -> Option<i64> {
    use jamjet_state::approvals::{answered_request, node_approval_status, NodeApprovalStatus};
    if !matches!(
        node_approval_status(events, node_id),
        NodeApprovalStatus::Approved { .. }
    ) {
        return None;
    }
    let request = answered_request(events, node_id)?;
    request.context.get(ESCALATION_KEY)?;
    let used = events.iter().any(|e| {
        matches!(&e.kind, EventKind::AgentStepAdmitted { approval_sequence: Some(s), .. } if *s == request.sequence)
    });
    (!used).then_some(request.sequence)
}

/// The key under which an escalation request's context shows the limit.
pub(crate) const ESCALATION_KEY: &str = "autonomy_limit";

/// An over-limit step whose card escalates: it may fire only under an
/// approval of that step.
#[derive(Debug, Clone)]
pub(crate) struct Escalation {
    pub agent: String,
    pub decision: AutonomyDecision,
}

impl Escalation {
    /// What the reviewer is asked about, in the request's context.
    pub(crate) fn context(&self) -> Value {
        let mut v = serde_json::json!({ "agent_ref": self.agent });
        match &self.decision {
            AutonomyDecision::LimitReached {
                limit_type,
                limit_value,
                actual_value,
            } => {
                v["limit_type"] = Value::from(*limit_type);
                v["limit_value"] = limit_value.clone();
                v["actual_value"] = actual_value.clone();
            }
            AutonomyDecision::CircuitBreakerTripped {
                consecutive_errors,
                threshold,
            } => {
                v["limit_type"] = Value::from("circuit_breaker");
                v["limit_value"] = Value::from(*threshold);
                v["actual_value"] = Value::from(*consecutive_errors);
            }
            AutonomyDecision::Proceed => {}
        }
        v
    }
}

/// A step the limits allow (or escalate), ready to be recorded once the
/// rest of the guard allows it too.
pub(crate) struct Plan {
    agents: Agents,
    step: Step,
    /// The fold of the run's earlier segments. They are closed, so only the
    /// current segment's log is re-read at the append.
    earlier: Ledger,
    now: DateTime<Utc>,
    pub escalation: Option<Escalation>,
}

/// Decide whether a step of `agents` making `tool_calls` tool calls may
/// fire. `Ok(plan)`: it may, possibly only under an approval
/// (`plan.escalation`); record it with [`admit`] if the rest of the guard
/// allows the node. `Err`: refused (a limit with `on_limit_exceeded: fail`,
/// recorded here) or undecidable (`Unavailable`).
pub(crate) async fn plan(
    backend: &dyn StateBackend,
    execution_id: &ExecutionId,
    node_id: &str,
    agents: Agents,
    tool_calls: u32,
) -> Result<Plan, GuardOutcome> {
    let unavailable = |what: String| GuardOutcome::Unavailable {
        reason: format!(
            "agent autonomy counters unavailable; refusing to run without checking them: {what}"
        ),
    };
    let chain = jamjet_state::segment_chain(backend, execution_id)
        .await
        .map_err(|e| unavailable(e.to_string()))?;
    let mut earlier = Ledger::default();
    for id in chain.iter().filter(|id| *id != execution_id) {
        let events = backend
            .get_events(id)
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        earlier
            .apply_segment(&events)
            .map_err(|_| unavailable(format!("segment {id} holds an unreadable event")))?;
    }
    let events = backend
        .get_events(execution_id)
        .await
        .map_err(|e| unavailable(e.to_string()))?;
    let mut ledger = earlier.clone();
    ledger
        .apply_segment(&events)
        .map_err(|_| unavailable("the run's log holds an unreadable event".to_string()))?;

    let now = Utc::now();
    let step = Step { tool_calls };
    let escalation = match first_stop(&ledger, &agents, step, now) {
        None => None,
        Some((agent, decision, OnLimitExceeded::Fail)) => {
            let reason = describe(&agent, &decision);
            warn!(execution_id = %execution_id, node_id, %reason, "agent autonomy limit refused node");
            record_stop(backend, execution_id, node_id, &agent, &decision, None).await;
            return Err(GuardOutcome::Blocked { reason });
        }
        Some((agent, decision, OnLimitExceeded::EscalateToHuman)) => {
            Some(Escalation { agent, decision })
        }
    };
    Ok(Plan {
        agents,
        step,
        earlier,
        now,
        escalation,
    })
}

/// The refusal text, recorded on the node's `NodeFailed`.
fn describe(agent: &str, decision: &AutonomyDecision) -> String {
    match decision {
        AutonomyDecision::LimitReached {
            limit_type,
            limit_value,
            actual_value,
        } => format!(
            "{AUTONOMY_RULE}: agent '{agent}' reached {limit_type} (limit {limit_value}, \
             at {actual_value})"
        ),
        AutonomyDecision::CircuitBreakerTripped {
            consecutive_errors,
            threshold,
        } => format!(
            "{AUTONOMY_RULE}: agent '{agent}' circuit breaker tripped ({consecutive_errors} \
             consecutive errors, threshold {threshold})"
        ),
        AutonomyDecision::Proceed => format!("{AUTONOMY_RULE}: agent '{agent}'"),
    }
}

/// Record the stop (`AutonomyLimitReached` or `CircuitBreakerTripped`) and,
/// for an escalation, `EscalationRequired`. Best effort: the refusal or the
/// hold stands whether or not these land.
pub(crate) async fn record_stop(
    backend: &dyn StateBackend,
    execution_id: &ExecutionId,
    node_id: &str,
    agent: &str,
    decision: &AutonomyDecision,
    escalated_to: Option<&str>,
) {
    let stop = match decision {
        AutonomyDecision::LimitReached {
            limit_type,
            limit_value,
            actual_value,
        } => EventKind::AutonomyLimitReached {
            node_id: node_id.to_string(),
            agent_ref: agent.to_string(),
            limit_type: limit_type.to_string(),
            limit_value: limit_value.clone(),
            actual_value: actual_value.clone(),
        },
        AutonomyDecision::CircuitBreakerTripped {
            consecutive_errors,
            threshold,
        } => EventKind::CircuitBreakerTripped {
            node_id: node_id.to_string(),
            agent_ref: agent.to_string(),
            consecutive_errors: *consecutive_errors,
            threshold: *threshold,
        },
        AutonomyDecision::Proceed => return,
    };
    let reason = match decision {
        AutonomyDecision::CircuitBreakerTripped { .. } => "circuit_breaker".to_string(),
        AutonomyDecision::LimitReached { limit_type, .. } => format!("autonomy_limit:{limit_type}"),
        AutonomyDecision::Proceed => return,
    };
    let mut kinds = vec![stop];
    if let Some(target) = escalated_to {
        kinds.push(EventKind::EscalationRequired {
            node_id: node_id.to_string(),
            agent_ref: agent.to_string(),
            reason,
            escalation_target: target.to_string(),
        });
    }
    for kind in kinds {
        if let Err(e) = backend
            .append_event(Event::new(execution_id.clone(), 0, kind))
            .await
        {
            warn!(execution_id = %execution_id, node_id, "could not record an autonomy stop: {e}");
        }
    }
}

/// Record the step, if the limits still allow it on the log as it stands
/// (`approved`: an approval covers this over-limit step, so only the record
/// is needed). `Allow` once recorded; anything else withholds the node
/// (`Unavailable`: the next attempt decides again).
pub(crate) async fn admit(
    backend: &dyn StateBackend,
    execution_id: &ExecutionId,
    node_id: &str,
    work_item_id: Option<uuid::Uuid>,
    plan: Plan,
) -> GuardOutcome {
    let Plan {
        agents,
        step,
        earlier,
        now,
        escalation,
    } = plan;
    // An escalated step fires only on the approval that released it, read
    // now, and re-checked in the append below.
    let approval_sequence = match &escalation {
        None => None,
        Some(_) => match backend.get_events(execution_id).await {
            Ok(events) => match unused_escalation(&events, node_id) {
                Some(seq) => Some(seq),
                None => {
                    return GuardOutcome::Unavailable {
                        reason: "the approval releasing this step past the agent's limit was \
                                 used or withdrawn; deciding again on the next attempt"
                            .to_string(),
                    }
                }
            },
            Err(e) => {
                return GuardOutcome::Unavailable {
                    reason: format!("agent steps unreadable: {e}"),
                }
            }
        },
    };
    let names = agents.iter().map(|(a, _)| a.clone()).collect();
    let node = node_id.to_string();
    let event = Event::new(
        execution_id.clone(),
        0,
        EventKind::AgentStepAdmitted {
            node_id: node_id.to_string(),
            work_item_id,
            agents: names,
            tool_calls: step.tool_calls,
            approval_sequence,
        },
    );
    let still_allowed = move |log: &[Event]| -> bool {
        let mut ledger = earlier.clone();
        if ledger.apply_segment(log).is_err() {
            return false;
        }
        match approval_sequence {
            // Every limit still allows it.
            None => first_stop(&ledger, &agents, step, now).is_none(),
            // Past an escalating card's limit on that one approval, still
            // unused, and no failing card's limit reached meanwhile.
            Some(seq) => {
                unused_escalation(log, &node) == Some(seq)
                    && !fail_stop(&ledger, &agents, step, now)
            }
        }
    };
    match backend.append_event_if(event, &still_allowed).await {
        Ok(ConditionalAppend::Appended(_)) => GuardOutcome::Allow,
        Ok(ConditionalAppend::Refused) => GuardOutcome::Unavailable {
            reason: "another step of the agent was recorded while this one was decided; \
                     deciding again on the next attempt"
                .to_string(),
        },
        Ok(ConditionalAppend::ExecutionTerminal(status)) => GuardOutcome::Unavailable {
            reason: format!("execution is {status:?}"),
        },
        Err(e) => GuardOutcome::Unavailable {
            reason: format!("agent step could not be recorded; refusing to run it unrecorded: {e}"),
        },
    }
}

/// `GuardMode::Complete`: a result for a node acting for an agent is
/// recorded only for a work item the guard admitted. Without this a result
/// reported for an item that never went through the claim gate would be a
/// step no limit saw.
pub(crate) async fn completion_admitted(
    backend: &dyn StateBackend,
    execution_id: &ExecutionId,
    work_item_id: Option<uuid::Uuid>,
) -> Result<(), GuardOutcome> {
    let Some(item) = work_item_id else {
        return Err(GuardOutcome::Blocked {
            reason: format!(
                "{AUTONOMY_RULE}: a result for a node acting for an agent needs the work item \
                 it was admitted as"
            ),
        });
    };
    let events = backend
        .get_events(execution_id)
        .await
        .map_err(|e| GuardOutcome::Unavailable {
            reason: format!("agent steps unreadable; refusing to record the result: {e}"),
        })?;
    // The item's step must still be open: each admission of the node from
    // the item's latest one on is ended by one result, so a step whose
    // result is already recorded backs no second one.
    let admitted = match events.iter().rposition(|e| {
        matches!(&e.kind, EventKind::AgentStepAdmitted { work_item_id: Some(id), .. } if *id == item)
    }) {
        None => false,
        Some(at) => {
            let node = events[at].kind.node_id().unwrap_or_default();
            let (mut opened, mut ended) = (0usize, 0usize);
            for e in &events[at..] {
                match &e.kind {
                    EventKind::AgentStepAdmitted { node_id, .. } if node_id == node => opened += 1,
                    EventKind::NodeCompleted { node_id, .. } | EventKind::NodeFailed { node_id, .. }
                        if node_id == node =>
                    {
                        ended += 1
                    }
                    _ => {}
                }
            }
            ended < opened
        }
    };
    if admitted {
        Ok(())
    } else {
        Err(GuardOutcome::Blocked {
            reason: format!(
                "{AUTONOMY_RULE}: work item {item} acts for an agent and was never admitted by \
                 the claim gate, so its result cannot be recorded"
            ),
        })
    }
}
