//! Budget state accumulator — tracks token and cost usage per execution.
//!
//! `BudgetState` is stored as a JSON sub-object under the `__budget` key
//! in the execution's workflow state (inside `Snapshot.state`). This avoids
//! a separate table or schema migration.

use jamjet_ir::workflow::TokenBudgetIr;
use serde::{Deserialize, Serialize};

/// Accumulated runtime budget for a single workflow execution.
///
/// Embedded in `Snapshot.state` under `"__budget"`. Updated after every
/// `NodeCompleted` event that carries token / cost telemetry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetState {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
    /// Number of agent reasoning iterations across all agent nodes.
    pub iteration_count: u32,
    /// Number of tool calls across all nodes.
    pub tool_call_count: u32,
    /// Consecutive error count for circuit-breaker logic.
    pub consecutive_error_count: u32,
    /// True once the circuit breaker has fired for this execution.
    pub circuit_breaker_tripped: bool,
}

impl BudgetState {
    /// Total tokens (input + output).
    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens + self.total_output_tokens
    }

    /// Load `BudgetState` from the `__budget` key of a snapshot state value.
    pub fn from_snapshot_state(state: &serde_json::Value) -> Self {
        state
            .get("__budget")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    /// Merge this `BudgetState` back into the snapshot state JSON as `__budget`.
    pub fn patch_into_snapshot_state(&self, state: &mut serde_json::Value) {
        if let Some(obj) = state.as_object_mut() {
            obj.insert(
                "__budget".to_string(),
                serde_json::to_value(self).unwrap_or(serde_json::Value::Null),
            );
        }
    }

    /// Accumulate token / cost values from a completed model node.
    pub fn accumulate(
        &mut self,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cost_usd: Option<f64>,
    ) {
        self.total_input_tokens += input_tokens.unwrap_or(0);
        self.total_output_tokens += output_tokens.unwrap_or(0);
        self.total_cost_usd += cost_usd.unwrap_or(0.0);
    }

    /// Record a successful node — resets the consecutive error counter.
    pub fn record_success(&mut self) {
        self.consecutive_error_count = 0;
    }

    /// Record a failed node — increments the consecutive error counter.
    pub fn record_error(&mut self) {
        self.consecutive_error_count += 1;
    }
}

/// A budget ceiling that has already been reached or passed.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetTrip {
    /// Which ceiling: "total_tokens", "input_tokens", "output_tokens", "cost_usd".
    pub kind: String,
    pub limit: f64,
    pub current: f64,
}

impl BudgetState {
    /// True when the accumulated counters have already reached a ceiling, i.e.
    /// firing another billable node would overspend.
    ///
    /// Deliberately `>=`, unlike the post-execution check's `>`: a run can settle
    /// exactly at the limit, and the point of this predicate is to refuse the
    /// *next* call from that state. This is what stops a rollover child, which
    /// inherits the parent's pinned counters, from making one paid call per
    /// segment forever.
    pub fn exhausted(
        &self,
        token_budget: Option<&TokenBudgetIr>,
        cost_budget_usd: Option<f64>,
    ) -> Option<BudgetTrip> {
        if let Some(tb) = token_budget {
            if let Some(limit) = tb.total_tokens {
                if self.total_tokens() >= limit as u64 {
                    return Some(BudgetTrip {
                        kind: "total_tokens".into(),
                        limit: limit as f64,
                        current: self.total_tokens() as f64,
                    });
                }
            }
            if let Some(limit) = tb.input_tokens {
                if self.total_input_tokens >= limit as u64 {
                    return Some(BudgetTrip {
                        kind: "input_tokens".into(),
                        limit: limit as f64,
                        current: self.total_input_tokens as f64,
                    });
                }
            }
            if let Some(limit) = tb.output_tokens {
                if self.total_output_tokens >= limit as u64 {
                    return Some(BudgetTrip {
                        kind: "output_tokens".into(),
                        limit: limit as f64,
                        current: self.total_output_tokens as f64,
                    });
                }
            }
        }
        if let Some(limit) = cost_budget_usd {
            if self.total_cost_usd >= limit {
                return Some(BudgetTrip {
                    kind: "cost_usd".into(),
                    limit,
                    current: self.total_cost_usd,
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip_via_snapshot_state() {
        let mut budget = BudgetState::default();
        budget.accumulate(Some(100), Some(50), Some(0.002));
        budget.iteration_count = 2;

        let mut state = json!({ "answer": "hello" });
        budget.patch_into_snapshot_state(&mut state);

        let loaded = BudgetState::from_snapshot_state(&state);
        assert_eq!(loaded.total_input_tokens, 100);
        assert_eq!(loaded.total_output_tokens, 50);
        assert!((loaded.total_cost_usd - 0.002).abs() < 1e-9);
        assert_eq!(loaded.iteration_count, 2);

        // The original state key is preserved.
        assert_eq!(state["answer"], "hello");
    }

    #[test]
    fn total_tokens() {
        let mut b = BudgetState::default();
        b.accumulate(Some(300), Some(200), None);
        assert_eq!(b.total_tokens(), 500);
    }

    #[test]
    fn fresh_budget_is_not_exhausted() {
        let b = BudgetState::default();
        let tb = TokenBudgetIr {
            total_tokens: Some(1000),
            input_tokens: None,
            output_tokens: None,
        };
        assert_eq!(b.exhausted(Some(&tb), None), None);
    }

    #[test]
    fn budget_at_the_ceiling_is_exhausted() {
        // The post-execution check uses `>`, so a run can settle exactly at the
        // limit. Firing again from there would overspend, so `>=` is correct here.
        let b = BudgetState {
            total_input_tokens: 600,
            total_output_tokens: 400,
            ..Default::default()
        };
        let tb = TokenBudgetIr {
            total_tokens: Some(1000),
            input_tokens: None,
            output_tokens: None,
        };
        let trip = b.exhausted(Some(&tb), None).expect("must be exhausted");
        assert_eq!(trip.kind, "total_tokens");
    }

    #[test]
    fn budget_over_the_ceiling_is_exhausted() {
        // This is the carried-over state a rollover child inherits.
        let b = BudgetState {
            total_input_tokens: 5000,
            ..Default::default()
        };
        let tb = TokenBudgetIr {
            total_tokens: Some(1000),
            input_tokens: None,
            output_tokens: None,
        };
        assert!(b.exhausted(Some(&tb), None).is_some());
    }

    #[test]
    fn input_and_output_sub_ceilings_trip_independently() {
        let b = BudgetState {
            total_input_tokens: 900,
            total_output_tokens: 10,
            ..Default::default()
        };
        let tb = TokenBudgetIr {
            total_tokens: None,
            input_tokens: Some(900),
            output_tokens: None,
        };
        assert_eq!(
            b.exhausted(Some(&tb), None).expect("exhausted").kind,
            "input_tokens"
        );
    }

    #[test]
    fn cost_ceiling_trips() {
        let b = BudgetState {
            total_cost_usd: 0.5,
            ..Default::default()
        };
        assert_eq!(
            b.exhausted(None, Some(0.5)).expect("exhausted").kind,
            "cost_usd"
        );
    }

    #[test]
    fn no_budget_configured_never_trips() {
        let b = BudgetState {
            total_input_tokens: u64::MAX / 2,
            total_cost_usd: 1e9,
            ..Default::default()
        };
        assert_eq!(b.exhausted(None, None), None);
    }
}
