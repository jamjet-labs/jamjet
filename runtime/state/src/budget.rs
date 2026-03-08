//! Budget state accumulator — tracks token and cost usage per execution.
//!
//! `BudgetState` is stored as a JSON sub-object under the `__budget` key
//! in the execution's workflow state (inside `Snapshot.state`). This avoids
//! a separate table or schema migration.

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
}
