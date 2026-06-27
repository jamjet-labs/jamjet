//! Executor for `Condition` workflow nodes.
//!
//! Reads the node's `branches` from the work-item payload, evaluates each
//! branch expression against the committed workflow state (also in the payload),
//! picks the first matching branch, and records the routing decision on the
//! `NodeCompleted` output so the scheduler (FDL-3) can honor it.
//!
//! # Routing logic (ordered evaluation)
//!
//! 1. For each branch in order: if `condition` is `Some(expr)` and
//!    `eval_condition(expr, &state)` is true, that branch wins.
//! 2. If no conditional branch matched, use the first branch with
//!    `condition: None` (the default/else branch).
//! 3. If neither — no match and no default — `chosen_target` is `null`.
//!    The scheduler (FDL-3) will treat every branch target as dead and emit
//!    `NodeSkipped` for them.
//!
//! # Output shape
//!
//! Only `output` carries the routing dict:
//! ```json
//! { "chosen_target": "<NodeId>" | null, "branch_targets": ["<NodeId>", ...] }
//! ```
//! `state_patch` is always `{}` — routing metadata must not enter workflow state.
//!
//! FDL-3 reads `chosen_target` and `branch_targets` off the `NodeCompleted`
//! `output` to build the dead-edge set and emit `NodeSkipped` for dominated tails.
//!
//! # Backward compatibility
//!
//! An empty `branches` list (used as a generic pass-through in scheduler tests)
//! returns empty `output` and `state_patch` — identical to the old no-op stub.

use crate::executor::{ExecutionResult, ExecutorError, NodeExecutor};
use async_trait::async_trait;
use jamjet_core::eval_condition;
use jamjet_state::backend::WorkItem;
use serde_json::{json, Value};

/// Executor for `condition` workflow nodes.
#[derive(Debug, Default)]
pub struct ConditionNodeExecutor;

impl ConditionNodeExecutor {
    pub fn new() -> Self {
        Self
    }
}

/// Local mirror of `jamjet_core::node::ConditionalBranch`.
///
/// We deserialize from the payload rather than importing the core struct so
/// this crate does not need to reach into `jamjet_core::node` internals.
#[derive(Debug, serde::Deserialize)]
struct BranchDef {
    /// `None` marks the default/else branch.
    condition: Option<String>,
    /// The NodeId this branch routes to.
    target: String,
}

#[async_trait]
impl NodeExecutor for ConditionNodeExecutor {
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        let start = std::time::Instant::now();

        // Three-case handling for the `branches` payload field:
        //
        //   1. Missing key  -> no-op (backward-compat: a non-Condition or
        //      unenriched node; the scheduler didn't inject branches).
        //   2. Present + explicitly empty array `[]` -> no-op (backward-compat:
        //      empty-branches Condition used as a pass-through in topology tests).
        //   3. Present but malformed (wrong type, or objects missing required
        //      fields) -> Fatal error. Silently dropping routing is worse than
        //      failing loudly; the scheduler must not run all branches.
        let noop_result = || ExecutionResult {
            output: json!({}),
            state_patch: json!({}),
            duration_ms: start.elapsed().as_millis() as u64,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        };

        let branches_raw = match item.payload.get("branches") {
            None => return Ok(noop_result()), // case 1: key absent
            Some(v) => v.clone(),
        };

        let branches: Vec<BranchDef> = serde_json::from_value(branches_raw).map_err(|e| {
            ExecutorError::Fatal(format!(
                "condition node `{}`: malformed `branches` payload — \
                     expected an array of {{condition, target}} objects; {e}",
                item.node_id
            ))
        })?; // case 3: present but malformed -> propagate Fatal

        // Case 2: present but explicitly empty -> no-op pass-through.
        if branches.is_empty() {
            return Ok(noop_result());
        }

        // Committed workflow state injected by the scheduler into the payload.
        let state: Value = item.payload.get("state").cloned().unwrap_or(json!({}));

        // All branch targets — written into the output so FDL-3 can identify
        // every branch target to potentially skip.
        let branch_targets: Vec<Value> = branches
            .iter()
            .map(|b| Value::String(b.target.clone()))
            .collect();

        // Evaluate branches in order:
        //   first Some(expr) that evals true -> chosen
        //   else first None (default/else)   -> chosen
        //   else null                        -> no-match
        let mut chosen: Option<String> = None;
        let mut default_branch: Option<String> = None;

        for branch in &branches {
            match &branch.condition {
                Some(expr) => {
                    if chosen.is_none() && eval_condition(expr, &state) {
                        chosen = Some(branch.target.clone());
                    }
                }
                None => {
                    if default_branch.is_none() {
                        default_branch = Some(branch.target.clone());
                    }
                }
            }
        }

        let resolved = chosen.or(default_branch);

        let chosen_json: Value = match &resolved {
            Some(t) => Value::String(t.clone()),
            None => {
                tracing::warn!(
                    node_id = %item.node_id,
                    "condition: no branch matched and no default; all targets will be skipped"
                );
                Value::Null
            }
        };

        // Record the routing decision on output only so the scheduler (FDL-3)
        // can read it off the NodeCompleted event. state_patch stays empty so
        // routing metadata never bleeds into workflow state or final_state.
        let routing = json!({
            "chosen_target": chosen_json,
            "branch_targets": branch_targets,
        });

        Ok(ExecutionResult {
            output: routing,
            state_patch: json!({}),
            duration_ms: start.elapsed().as_millis() as u64,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use jamjet_state::backend::WorkItem;

    fn make_item(branches: serde_json::Value, state: serde_json::Value) -> WorkItem {
        WorkItem {
            id: uuid::Uuid::new_v4(),
            execution_id: jamjet_core::workflow::ExecutionId::new(),
            node_id: "gate".into(),
            queue_type: "general".into(),
            payload: json!({
                "workflow_id": "wf",
                "workflow_version": "0.1.0",
                "node_id": "gate",
                "branches": branches,
                "state": state,
            }),
            attempt: 0,
            max_attempts: 3,
            created_at: chrono::Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: "default".into(),
        }
    }

    /// Build a WorkItem with a fully-custom payload (no implicit `branches` key).
    fn make_item_raw(payload: serde_json::Value) -> WorkItem {
        WorkItem {
            id: uuid::Uuid::new_v4(),
            execution_id: jamjet_core::workflow::ExecutionId::new(),
            node_id: "gate".into(),
            queue_type: "general".into(),
            payload,
            attempt: 0,
            max_attempts: 3,
            created_at: chrono::Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: "default".into(),
        }
    }

    /// G1: first conditional branch matches (state.x == "go") -> chosen_target "A".
    #[tokio::test]
    async fn first_branch_matches() {
        let executor = ConditionNodeExecutor::new();
        let item = make_item(
            json!([
                {"condition": "state.x == \"go\"", "target": "A"},
                {"condition": null, "target": "B"}
            ]),
            json!({"x": "go"}),
        );
        let result = executor.execute(&item).await.expect("must not error");

        assert_eq!(
            result.output["chosen_target"], "A",
            "first branch condition matches -> chosen_target A"
        );
        assert_eq!(
            result.state_patch,
            json!({}),
            "routing is output-only; state_patch must be empty"
        );
        let targets = result.output["branch_targets"]
            .as_array()
            .expect("branch_targets must be an array");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0], "A");
        assert_eq!(targets[1], "B");
    }

    /// G2: first conditional branch does NOT match -> falls back to the default (None) branch B.
    #[tokio::test]
    async fn falls_back_to_default_branch() {
        let executor = ConditionNodeExecutor::new();
        let item = make_item(
            json!([
                {"condition": "state.x == \"go\"", "target": "A"},
                {"condition": null, "target": "B"}
            ]),
            json!({"x": "no"}),
        );
        let result = executor.execute(&item).await.expect("must not error");

        assert_eq!(
            result.output["chosen_target"], "B",
            "no conditional match -> falls back to default branch B"
        );
    }

    /// G3: no conditional branch matches and no default branch -> chosen_target null.
    #[tokio::test]
    async fn no_match_no_default_returns_null() {
        let executor = ConditionNodeExecutor::new();
        let item = make_item(
            json!([
                {"condition": "state.x == \"go\"", "target": "A"},
                {"condition": "state.x == \"maybe\"", "target": "C"}
            ]),
            json!({"x": "never"}),
        );
        let result = executor.execute(&item).await.expect("must not error");

        assert!(
            result.output["chosen_target"].is_null(),
            "no match and no default -> chosen_target must be null"
        );
        let targets = result.output["branch_targets"]
            .as_array()
            .expect("branch_targets must be an array");
        assert_eq!(targets.len(), 2);
    }

    /// G4: empty branches -> no-op (empty output/state_patch, backward-compat).
    #[tokio::test]
    async fn empty_branches_is_noop() {
        let executor = ConditionNodeExecutor::new();
        let item = make_item(json!([]), json!({"x": "go"}));
        let result = executor.execute(&item).await.expect("must not error");

        // Neither chosen_target nor branch_targets should be present.
        assert!(
            result.output["chosen_target"].is_null(),
            "empty branches -> output must not carry routing keys"
        );
        assert_eq!(
            result.output,
            json!({}),
            "empty branches -> output must be empty object (no-op)"
        );
        assert_eq!(
            result.state_patch,
            json!({}),
            "empty branches -> state_patch must be empty (no-op)"
        );
    }

    /// G5: absent state key -> the missing-path rule (eval_condition returns false)
    ///     so the default branch is taken.
    #[tokio::test]
    async fn absent_state_key_falls_back_to_default() {
        let executor = ConditionNodeExecutor::new();
        let item = make_item(
            json!([
                {"condition": "state.finish_reason == \"stop\"", "target": "END"},
                {"condition": null, "target": "TOOLS"}
            ]),
            json!({}),
        );
        let result = executor.execute(&item).await.expect("must not error");
        assert_eq!(
            result.output["chosen_target"], "TOOLS",
            "absent state key -> condition false -> default branch taken"
        );
    }

    /// G6: `branches` is present but malformed (not an array) -> Fatal error.
    ///
    /// Regression: the old code silently fell through to the no-op path, which
    /// would disable routing without any signal.
    #[tokio::test]
    async fn malformed_branches_string_returns_fatal_error() {
        let executor = ConditionNodeExecutor::new();
        let item = make_item_raw(json!({
            "workflow_id": "wf",
            "workflow_version": "0.1.0",
            "node_id": "gate",
            "branches": "not an array",
            "state": {},
        }));
        let result = executor.execute(&item).await;
        assert!(
            matches!(result, Err(ExecutorError::Fatal(_))),
            "malformed branches must return ExecutorError::Fatal, got: {:?}",
            result
        );
    }

    /// G7: `branches` is present but contains objects missing required fields -> Fatal error.
    #[tokio::test]
    async fn malformed_branches_missing_target_returns_fatal_error() {
        let executor = ConditionNodeExecutor::new();
        let item = make_item_raw(json!({
            "workflow_id": "wf",
            "workflow_version": "0.1.0",
            "node_id": "gate",
            "branches": [{"no_target": true}],
            "state": {},
        }));
        let result = executor.execute(&item).await;
        assert!(
            matches!(result, Err(ExecutorError::Fatal(_))),
            "branches array with objects missing `target` must return ExecutorError::Fatal, got: {:?}",
            result
        );
    }

    /// G8: `branches` key is absent entirely -> no-op success (backward-compat).
    #[tokio::test]
    async fn missing_branches_key_is_noop() {
        let executor = ConditionNodeExecutor::new();
        let item = make_item_raw(json!({
            "workflow_id": "wf",
            "workflow_version": "0.1.0",
            "node_id": "gate",
            // no "branches" key
            "state": {"x": "go"},
        }));
        let result = executor.execute(&item).await.expect("must not error");
        assert_eq!(
            result.output,
            json!({}),
            "missing branches key -> no-op empty output"
        );
        assert_eq!(result.state_patch, json!({}));
    }
}
