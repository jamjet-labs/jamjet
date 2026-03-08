//! Autonomy enforcement for JamJet agents (Phase 4 — Tier 1).
//!
//! Enforces the three autonomy levels:
//! - `Guided`: every tool call pauses for human approval
//! - `BoundedAutonomous`: runs within declared limits (max_iterations, cost, tokens, tool calls)
//! - `FullyAutonomous`: only the circuit breaker applies
//!
//! The circuit breaker trips when an agent emits N consecutive errors (default: 3),
//! regardless of autonomy level.

use jamjet_agents::card::{AutonomyConstraints, AutonomyLevel};
use tracing::warn;

/// Current runtime context for an agent node.
#[derive(Debug, Clone)]
pub struct AutonomyContext {
    /// Identifies the agent for logging / event emission.
    pub agent_ref: String,
    pub autonomy_level: AutonomyLevel,
    pub constraints: Option<AutonomyConstraints>,
    /// Accumulated iterations across the full execution.
    pub current_iterations: u32,
    /// Accumulated tool calls across the full execution.
    pub current_tool_calls: u32,
    /// Accumulated USD cost across the full execution.
    pub current_cost_usd: f64,
    /// Accumulated tokens across the full execution.
    pub current_tokens: u64,
    /// Consecutive error count (for circuit breaker).
    pub consecutive_errors: u32,
    /// Max consecutive errors before the circuit breaker trips (default: 3).
    pub circuit_breaker_threshold: u32,
}

impl Default for AutonomyContext {
    fn default() -> Self {
        Self {
            agent_ref: String::new(),
            autonomy_level: AutonomyLevel::Guided,
            constraints: None,
            current_iterations: 0,
            current_tool_calls: 0,
            current_cost_usd: 0.0,
            current_tokens: 0,
            consecutive_errors: 0,
            circuit_breaker_threshold: 3,
        }
    }
}

/// Decision returned by `AutonomyEnforcer::check`.
#[derive(Debug)]
pub enum AutonomyDecision {
    /// Proceed — no autonomy constraint blocks this node.
    Proceed,
    /// Guided mode: every tool call pauses for human approval.
    RequireToolApproval { tool_name: String },
    /// A declared limit was hit — escalate.
    EscalateLimit {
        limit_type: String,
        limit_value: serde_json::Value,
        actual_value: serde_json::Value,
        escalation_target: EscalationTarget,
    },
    /// The circuit breaker tripped.
    TripCircuitBreaker {
        consecutive_errors: u32,
        threshold: u32,
        escalation_target: EscalationTarget,
    },
}

/// Where to escalate when a limit or circuit breaker fires.
#[derive(Debug, Clone)]
pub enum EscalationTarget {
    SupervisorAgent { agent_id: String },
    HumanApproval,
}

impl EscalationTarget {
    pub fn as_str(&self) -> String {
        match self {
            Self::SupervisorAgent { agent_id } => format!("supervisor_agent:{agent_id}"),
            Self::HumanApproval => "human_approval".to_string(),
        }
    }
}

/// Checks autonomy constraints before a node executes.
pub struct AutonomyEnforcer;

impl AutonomyEnforcer {
    /// Evaluate autonomy constraints for the given context.
    ///
    /// Call this before executing an agent or tool node to determine whether
    /// the node should proceed, pause for approval, or escalate.
    pub fn check(&self, ctx: &AutonomyContext, tool_name: Option<&str>) -> AutonomyDecision {
        // Circuit breaker fires for all autonomy levels.
        if ctx.consecutive_errors >= ctx.circuit_breaker_threshold {
            warn!(
                agent_ref = %ctx.agent_ref,
                consecutive_errors = ctx.consecutive_errors,
                threshold = ctx.circuit_breaker_threshold,
                "Circuit breaker tripped"
            );
            return AutonomyDecision::TripCircuitBreaker {
                consecutive_errors: ctx.consecutive_errors,
                threshold: ctx.circuit_breaker_threshold,
                escalation_target: EscalationTarget::HumanApproval,
            };
        }

        match &ctx.autonomy_level {
            AutonomyLevel::Guided => {
                // Every tool call requires approval in guided mode.
                if let Some(name) = tool_name {
                    return AutonomyDecision::RequireToolApproval {
                        tool_name: name.to_string(),
                    };
                }
                AutonomyDecision::Proceed
            }

            AutonomyLevel::BoundedAutonomous => {
                let constraints = match &ctx.constraints {
                    Some(c) => c,
                    None => return AutonomyDecision::Proceed,
                };

                // Check max_iterations.
                if let Some(max) = constraints.max_iterations {
                    if ctx.current_iterations >= max {
                        return AutonomyDecision::EscalateLimit {
                            limit_type: "max_iterations".to_string(),
                            limit_value: serde_json::json!(max),
                            actual_value: serde_json::json!(ctx.current_iterations),
                            escalation_target: EscalationTarget::HumanApproval,
                        };
                    }
                }

                // Check max_tool_calls.
                if let Some(max) = constraints.max_tool_calls {
                    if ctx.current_tool_calls >= max {
                        return AutonomyDecision::EscalateLimit {
                            limit_type: "max_tool_calls".to_string(),
                            limit_value: serde_json::json!(max),
                            actual_value: serde_json::json!(ctx.current_tool_calls),
                            escalation_target: EscalationTarget::HumanApproval,
                        };
                    }
                }

                // Check cost budget.
                if let Some(max_cost) = constraints.cost_budget_usd {
                    if ctx.current_cost_usd >= max_cost {
                        return AutonomyDecision::EscalateLimit {
                            limit_type: "cost_budget".to_string(),
                            limit_value: serde_json::json!(max_cost),
                            actual_value: serde_json::json!(ctx.current_cost_usd),
                            escalation_target: EscalationTarget::HumanApproval,
                        };
                    }
                }

                // Check token budget.
                if let Some(max_tokens) = constraints.token_budget {
                    if ctx.current_tokens >= max_tokens {
                        return AutonomyDecision::EscalateLimit {
                            limit_type: "token_budget".to_string(),
                            limit_value: serde_json::json!(max_tokens),
                            actual_value: serde_json::json!(ctx.current_tokens),
                            escalation_target: EscalationTarget::HumanApproval,
                        };
                    }
                }

                AutonomyDecision::Proceed
            }

            // Fully autonomous: only circuit breaker (checked above).
            AutonomyLevel::FullyAutonomous | AutonomyLevel::Deterministic => {
                AutonomyDecision::Proceed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamjet_agents::card::AutonomyConstraints;

    fn bounded_ctx(iterations: u32, cost: f64, tokens: u64) -> AutonomyContext {
        AutonomyContext {
            agent_ref: "test-agent".into(),
            autonomy_level: AutonomyLevel::BoundedAutonomous,
            constraints: Some(AutonomyConstraints {
                max_iterations: Some(5),
                max_tool_calls: Some(10),
                token_budget: Some(1000),
                cost_budget_usd: Some(1.0),
                allowed_tools: vec![],
                blocked_tools: vec![],
                allowed_delegations: vec![],
                require_approval_for: vec![],
                time_budget_secs: None,
            }),
            current_iterations: iterations,
            current_tool_calls: 0,
            current_cost_usd: cost,
            current_tokens: tokens,
            consecutive_errors: 0,
            circuit_breaker_threshold: 3,
        }
    }

    #[test]
    fn guided_requires_approval_for_tool() {
        let enforcer = AutonomyEnforcer;
        let ctx = AutonomyContext {
            agent_ref: "a".into(),
            autonomy_level: AutonomyLevel::Guided,
            ..Default::default()
        };
        assert!(matches!(
            enforcer.check(&ctx, Some("search")),
            AutonomyDecision::RequireToolApproval { .. }
        ));
    }

    #[test]
    fn bounded_under_limits_proceeds() {
        let enforcer = AutonomyEnforcer;
        let ctx = bounded_ctx(2, 0.5, 500);
        assert!(matches!(
            enforcer.check(&ctx, None),
            AutonomyDecision::Proceed
        ));
    }

    #[test]
    fn bounded_iteration_limit_escalates() {
        let enforcer = AutonomyEnforcer;
        let ctx = bounded_ctx(5, 0.0, 0);
        assert!(matches!(
            enforcer.check(&ctx, None),
            AutonomyDecision::EscalateLimit { limit_type, .. } if limit_type == "max_iterations"
        ));
    }

    #[test]
    fn bounded_cost_limit_escalates() {
        let enforcer = AutonomyEnforcer;
        let ctx = bounded_ctx(0, 1.5, 0);
        assert!(matches!(
            enforcer.check(&ctx, None),
            AutonomyDecision::EscalateLimit { limit_type, .. } if limit_type == "cost_budget"
        ));
    }

    #[test]
    fn circuit_breaker_fires_at_threshold() {
        let enforcer = AutonomyEnforcer;
        let ctx = AutonomyContext {
            agent_ref: "a".into(),
            autonomy_level: AutonomyLevel::FullyAutonomous,
            consecutive_errors: 3,
            circuit_breaker_threshold: 3,
            ..Default::default()
        };
        assert!(matches!(
            enforcer.check(&ctx, None),
            AutonomyDecision::TripCircuitBreaker { .. }
        ));
    }

    #[test]
    fn fully_autonomous_below_cb_threshold_proceeds() {
        let enforcer = AutonomyEnforcer;
        let ctx = AutonomyContext {
            agent_ref: "a".into(),
            autonomy_level: AutonomyLevel::FullyAutonomous,
            consecutive_errors: 2,
            circuit_breaker_threshold: 3,
            ..Default::default()
        };
        assert!(matches!(
            enforcer.check(&ctx, None),
            AutonomyDecision::Proceed
        ));
    }
}
