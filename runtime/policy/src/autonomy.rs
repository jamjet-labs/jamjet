//! An agent card's autonomy limits, decided from what the agent has used.
//!
//! The limits are the card's `max_iterations`, `max_tool_calls`,
//! `token_budget`, `cost_budget_usd`, `time_budget_secs` and the circuit
//! breaker (`max_consecutive_errors`, default
//! [`DEFAULT_MAX_CONSECUTIVE_ERRORS`]). They apply at every autonomy level.
//!
//! This module only decides. What the agent has used ([`AgentUsage`]) is
//! folded from the run's durable event log by the shared node guard
//! (`jamjet_worker::node_guard`), which also records the refusal and settles
//! the node. Keeping the decision a pure function of the usage means the
//! guard can run the same check inside the compare-and-set that records the
//! step, so two concurrent claims cannot both take the last step.
//!
//! Every check is made before a step fires, against what earlier steps used:
//!
//! - `max_iterations`: a step is refused once the agent has taken that many.
//! - `max_tool_calls`: a step is refused if its own calls would take the
//!   agent past the limit (a step that makes no tool call is not limited).
//! - `token_budget`, `cost_budget_usd`: a step is refused once the recorded
//!   usage is at or over the budget. The step that crosses it is not known
//!   to cross it until it reports, so the total can end above the budget by
//!   at most that one step's usage.
//! - `time_budget_secs`: a step is refused once that long has passed since
//!   the agent's first step in the run. A step already running is not
//!   interrupted.
//! - The circuit breaker: a step is refused once that many of the agent's
//!   steps in a row have failed. A completed step resets the count.
//!
//! A limit of zero refuses the first step.

use jamjet_agents::card::{AutonomyConstraints, DEFAULT_MAX_CONSECUTIVE_ERRORS};

/// What one agent has used so far in a run, across its segments.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentUsage {
    /// Steps admitted: nodes acting for the agent that were allowed to fire.
    pub iterations: u64,
    /// Tool calls those steps made.
    pub tool_calls: u64,
    /// Input plus output tokens recorded on the agent's completed steps.
    pub tokens: u64,
    /// USD cost recorded on the agent's completed steps.
    pub cost_usd: f64,
    /// The agent's steps that failed since its last completed step.
    pub consecutive_errors: u32,
    /// Milliseconds since the agent's first step in the run; `None` before
    /// its first step.
    pub elapsed_ms: Option<u64>,
}

/// The step about to fire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Step {
    /// Tool calls it makes.
    pub tool_calls: u32,
}

/// The decision for one agent and one step.
#[derive(Debug, Clone, PartialEq)]
pub enum AutonomyDecision {
    /// No limit stops the step.
    Proceed,
    /// The step would take the agent past a limit.
    LimitReached {
        /// `max_iterations`, `max_tool_calls`, `token_budget`, `cost_budget`
        /// or `time_budget`.
        limit_type: &'static str,
        limit_value: serde_json::Value,
        /// What the agent had used (for `max_tool_calls`, what it would use
        /// with this step's calls).
        actual_value: serde_json::Value,
    },
    /// Too many of the agent's steps in a row have failed.
    CircuitBreakerTripped {
        consecutive_errors: u32,
        threshold: u32,
    },
}

/// Decide whether `step` may fire for an agent whose card sets
/// `constraints` (`None`: a card with no constraints, where only the
/// default circuit breaker applies) and which has used `usage`.
pub fn check(
    constraints: Option<&AutonomyConstraints>,
    usage: &AgentUsage,
    step: Step,
) -> AutonomyDecision {
    let threshold = constraints
        .and_then(|c| c.max_consecutive_errors)
        .unwrap_or(DEFAULT_MAX_CONSECUTIVE_ERRORS);
    if usage.consecutive_errors >= threshold {
        return AutonomyDecision::CircuitBreakerTripped {
            consecutive_errors: usage.consecutive_errors,
            threshold,
        };
    }
    let Some(c) = constraints else {
        return AutonomyDecision::Proceed;
    };
    let reached = |limit_type, limit_value, actual_value| AutonomyDecision::LimitReached {
        limit_type,
        limit_value,
        actual_value,
    };

    if let Some(max) = c.max_iterations {
        if usage.iterations >= u64::from(max) {
            return reached("max_iterations", max.into(), usage.iterations.into());
        }
    }
    if let Some(max) = c.max_tool_calls {
        let would_be = usage.tool_calls + u64::from(step.tool_calls);
        if step.tool_calls > 0 && would_be > u64::from(max) {
            return reached("max_tool_calls", max.into(), would_be.into());
        }
    }
    if let Some(max) = c.token_budget {
        if usage.tokens >= max {
            return reached("token_budget", max.into(), usage.tokens.into());
        }
    }
    if let Some(max) = c.cost_budget_usd {
        // Stated positively: only a cost known to be under the budget may
        // proceed, so a NaN total (or budget) refuses.
        let under = usage.cost_usd < max;
        if !under {
            return reached(
                "cost_budget",
                serde_json::json!(max),
                serde_json::json!(usage.cost_usd),
            );
        }
    }
    if let (Some(secs), Some(elapsed_ms)) = (c.time_budget_secs, usage.elapsed_ms) {
        if elapsed_ms >= secs.saturating_mul(1_000) {
            return reached("time_budget", secs.into(), elapsed_ms.into());
        }
    }
    AutonomyDecision::Proceed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(v: serde_json::Value) -> AutonomyConstraints {
        serde_json::from_value(v).unwrap()
    }

    fn used(f: impl FnOnce(&mut AgentUsage)) -> AgentUsage {
        let mut u = AgentUsage::default();
        f(&mut u);
        u
    }

    fn limit_of(d: &AutonomyDecision) -> Option<&'static str> {
        match d {
            AutonomyDecision::LimitReached { limit_type, .. } => Some(limit_type),
            _ => None,
        }
    }

    const ONE_CALL: Step = Step { tool_calls: 1 };
    const NO_CALL: Step = Step { tool_calls: 0 };

    #[test]
    fn iterations_trip_at_the_limit_and_not_before() {
        let c = limits(serde_json::json!({ "max_iterations": 3 }));
        let below = used(|u| u.iterations = 2);
        let at = used(|u| u.iterations = 3);
        assert_eq!(check(Some(&c), &below, NO_CALL), AutonomyDecision::Proceed);
        let d = check(Some(&c), &at, NO_CALL);
        assert_eq!(limit_of(&d), Some("max_iterations"), "{d:?}");
        assert_eq!(
            d,
            AutonomyDecision::LimitReached {
                limit_type: "max_iterations",
                limit_value: serde_json::json!(3),
                actual_value: serde_json::json!(3),
            }
        );
    }

    #[test]
    fn tool_calls_count_the_calls_this_step_would_make() {
        let c = limits(serde_json::json!({ "max_tool_calls": 5 }));
        // 3 used + 2 now = 5: exactly at the limit, allowed.
        let three = used(|u| u.tool_calls = 3);
        assert_eq!(
            check(Some(&c), &three, Step { tool_calls: 2 }),
            AutonomyDecision::Proceed
        );
        // 3 used + 3 now = 6: over.
        let d = check(Some(&c), &three, Step { tool_calls: 3 });
        assert_eq!(
            d,
            AutonomyDecision::LimitReached {
                limit_type: "max_tool_calls",
                limit_value: serde_json::json!(5),
                actual_value: serde_json::json!(6),
            }
        );
        // A step that calls no tool is not limited by the tool call count.
        let five = used(|u| u.tool_calls = 5);
        assert_eq!(check(Some(&c), &five, NO_CALL), AutonomyDecision::Proceed);
        assert_eq!(
            limit_of(&check(Some(&c), &five, ONE_CALL)),
            Some("max_tool_calls")
        );
        // Past the limit (an approved escalation took it there), a step that
        // calls no tool still is not limited by the tool call count.
        let six = used(|u| u.tool_calls = 6);
        assert_eq!(check(Some(&c), &six, NO_CALL), AutonomyDecision::Proceed);
    }

    #[test]
    fn token_budget_trips_once_reached() {
        let c = limits(serde_json::json!({ "token_budget": 100 }));
        assert_eq!(
            check(Some(&c), &used(|u| u.tokens = 99), ONE_CALL),
            AutonomyDecision::Proceed
        );
        assert_eq!(
            limit_of(&check(Some(&c), &used(|u| u.tokens = 100), ONE_CALL)),
            Some("token_budget")
        );
    }

    #[test]
    fn cost_budget_trips_once_reached() {
        let c = limits(serde_json::json!({ "cost_budget_usd": 0.5 }));
        assert_eq!(
            check(Some(&c), &used(|u| u.cost_usd = 0.49), ONE_CALL),
            AutonomyDecision::Proceed
        );
        let d = check(Some(&c), &used(|u| u.cost_usd = 0.5), ONE_CALL);
        assert_eq!(
            d,
            AutonomyDecision::LimitReached {
                limit_type: "cost_budget",
                limit_value: serde_json::json!(0.5),
                actual_value: serde_json::json!(0.5),
            }
        );
    }

    /// A cost that is not a number (no JSON source can produce one, but the
    /// fold adds floats) must not walk through a `<` guard.
    #[test]
    fn a_nan_cost_is_refused() {
        let c = limits(serde_json::json!({ "cost_budget_usd": 0.5 }));
        assert_eq!(
            limit_of(&check(Some(&c), &used(|u| u.cost_usd = f64::NAN), ONE_CALL)),
            Some("cost_budget")
        );
    }

    #[test]
    fn time_budget_trips_once_elapsed() {
        let c = limits(serde_json::json!({ "time_budget_secs": 2 }));
        // Before the first step there is no clock to run out.
        assert_eq!(
            check(Some(&c), &AgentUsage::default(), ONE_CALL),
            AutonomyDecision::Proceed
        );
        assert_eq!(
            check(Some(&c), &used(|u| u.elapsed_ms = Some(1_999)), ONE_CALL),
            AutonomyDecision::Proceed
        );
        assert_eq!(
            limit_of(&check(
                Some(&c),
                &used(|u| u.elapsed_ms = Some(2_000)),
                ONE_CALL
            )),
            Some("time_budget")
        );
    }

    #[test]
    fn zero_limits_refuse_the_first_step() {
        for (field, limit) in [
            ("max_iterations", "max_iterations"),
            ("max_tool_calls", "max_tool_calls"),
            ("token_budget", "token_budget"),
            ("cost_budget_usd", "cost_budget"),
        ] {
            let c = limits(serde_json::json!({ field: 0 }));
            assert_eq!(
                limit_of(&check(Some(&c), &AgentUsage::default(), ONE_CALL)),
                Some(limit),
                "{field}"
            );
        }
        // A zero time budget refuses once the clock has started: the first
        // step starts it at zero elapsed.
        let c = limits(serde_json::json!({ "time_budget_secs": 0 }));
        assert_eq!(
            limit_of(&check(
                Some(&c),
                &used(|u| u.elapsed_ms = Some(0)),
                ONE_CALL
            )),
            Some("time_budget")
        );
    }

    #[test]
    fn the_circuit_breaker_defaults_to_three_and_applies_without_constraints() {
        assert_eq!(
            check(None, &used(|u| u.consecutive_errors = 2), ONE_CALL),
            AutonomyDecision::Proceed
        );
        assert_eq!(
            check(None, &used(|u| u.consecutive_errors = 3), ONE_CALL),
            AutonomyDecision::CircuitBreakerTripped {
                consecutive_errors: 3,
                threshold: 3
            }
        );
        let c = limits(serde_json::json!({ "max_consecutive_errors": 1 }));
        assert_eq!(
            check(Some(&c), &AgentUsage::default(), ONE_CALL),
            AutonomyDecision::Proceed
        );
        assert_eq!(
            check(Some(&c), &used(|u| u.consecutive_errors = 1), ONE_CALL),
            AutonomyDecision::CircuitBreakerTripped {
                consecutive_errors: 1,
                threshold: 1
            }
        );
    }

    #[test]
    fn no_limits_proceed_whatever_was_used() {
        let u = used(|u| {
            u.iterations = 1_000;
            u.tool_calls = 1_000;
            u.tokens = 1_000_000;
            u.cost_usd = 1_000.0;
            u.elapsed_ms = Some(1_000_000);
        });
        assert_eq!(
            check(Some(&AutonomyConstraints::default()), &u, ONE_CALL),
            AutonomyDecision::Proceed
        );
        assert_eq!(check(None, &u, ONE_CALL), AutonomyDecision::Proceed);
    }

    #[test]
    fn escalate_to_supervisor_is_not_accepted() {
        let r: Result<AutonomyConstraints, _> = serde_json::from_value(
            serde_json::json!({ "on_limit_exceeded": "escalate_to_supervisor" }),
        );
        assert!(r.is_err());
        let ok: AutonomyConstraints =
            serde_json::from_value(serde_json::json!({ "on_limit_exceeded": "escalate_to_human" }))
                .unwrap();
        assert_eq!(
            ok.on_limit_exceeded,
            Some(jamjet_agents::card::OnLimitExceeded::EscalateToHuman)
        );
    }
}
