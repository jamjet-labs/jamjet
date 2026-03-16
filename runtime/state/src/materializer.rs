//! State materialization — reconstruct current workflow state from events.
//!
//! Current state = latest_snapshot.state + apply(events since snapshot)
//!
//! Each `NodeCompleted` event carries a `state_patch` (a JSON merge patch, RFC 7396)
//! that is applied in sequence to evolve the workflow's `current_state`.

use crate::backend::StateBackend;
use crate::event::{Event, EventKind};
use crate::snapshot::DEFAULT_SNAPSHOT_INTERVAL;
use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use serde_json::Value;
use std::collections::HashMap;

/// The materialized state of a workflow execution at a point in time.
#[derive(Debug, Clone)]
pub struct MaterializedState {
    /// The current workflow state value (JSON).
    pub current_state: Value,
    /// Status derived from events.
    pub status: WorkflowStatus,
    /// All nodes that have reached a terminal state and their outputs.
    pub completed_nodes: HashMap<String, Value>,
    /// Nodes currently scheduled or running.
    pub active_nodes: std::collections::HashSet<String>,
    /// The highest event sequence number seen.
    pub last_sequence: i64,
}

/// Reconstruct the current workflow state from the event log.
///
/// Algorithm:
/// 1. Load the latest snapshot (if any). If none, start from the initial_input.
/// 2. Load all events since the snapshot's `at_sequence`.
/// 3. Apply state patches from `NodeCompleted` events in order.
/// 4. Derive `status`, `completed_nodes`, and `active_nodes` from all events.
pub async fn materialize(
    backend: &dyn StateBackend,
    execution_id: &ExecutionId,
) -> Result<MaterializedState, crate::backend::StateBackendError> {
    let execution = backend
        .get_execution(execution_id)
        .await?
        .ok_or_else(|| crate::backend::StateBackendError::NotFound(format!("{execution_id}")))?;

    // Load the latest snapshot as the base.
    let (base_state, base_sequence) = match backend.latest_snapshot(execution_id).await? {
        Some(snap) => (snap.state, snap.at_sequence),
        None => (execution.initial_input.clone(), 0),
    };

    // Load all events since the snapshot (or from the beginning).
    let events = backend
        .get_events_since(execution_id, base_sequence)
        .await?;

    Ok(apply_events(base_state, &events, &execution.status))
}

/// Apply a sequence of events on top of a base state to produce `MaterializedState`.
pub fn apply_events(
    mut current_state: Value,
    events: &[Event],
    _initial_status: &WorkflowStatus,
) -> MaterializedState {
    let mut status = WorkflowStatus::Pending;
    let mut completed_nodes: HashMap<String, Value> = HashMap::new();
    let mut active_nodes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_sequence = 0i64;

    for event in events {
        last_sequence = last_sequence.max(event.sequence);

        match &event.kind {
            EventKind::WorkflowStarted { .. } => {
                status = WorkflowStatus::Running;
            }
            EventKind::WorkflowCompleted { final_state } => {
                current_state = final_state.clone();
                status = WorkflowStatus::Completed;
            }
            EventKind::WorkflowFailed { .. } => {
                status = WorkflowStatus::Failed;
            }
            EventKind::WorkflowCancelled { .. } => {
                status = WorkflowStatus::Cancelled;
            }
            EventKind::StrategyLimitHit { .. } => {
                status = WorkflowStatus::LimitExceeded;
            }
            EventKind::NodeScheduled { node_id, .. } | EventKind::NodeStarted { node_id, .. } => {
                active_nodes.insert(node_id.clone());
            }
            EventKind::NodeCompleted {
                node_id,
                output,
                state_patch,
                ..
            } => {
                active_nodes.remove(node_id);
                completed_nodes.insert(node_id.clone(), output.clone());
                // Apply JSON merge patch (RFC 7396) to current state.
                json_merge_patch(&mut current_state, state_patch);
            }
            EventKind::NodeFailed { node_id, .. }
            | EventKind::NodeSkipped { node_id, .. }
            | EventKind::NodeCancelled { node_id } => {
                active_nodes.remove(node_id);
            }
            EventKind::InterruptRaised { .. } => {
                if status == WorkflowStatus::Running {
                    status = WorkflowStatus::Paused;
                }
            }
            EventKind::ApprovalReceived { state_patch, .. } => {
                if let Some(patch) = state_patch {
                    json_merge_patch(&mut current_state, patch);
                }
                status = WorkflowStatus::Running;
            }
            _ => {}
        }
    }

    MaterializedState {
        current_state,
        status,
        completed_nodes,
        active_nodes,
        last_sequence,
    }
}

/// Apply a JSON merge patch (RFC 7396) to a target value.
///
/// - Object keys in the patch are merged recursively.
/// - Patch values of `null` remove the key from the target.
/// - Non-object patches replace the target entirely.
fn json_merge_patch(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(patch_map) => {
            if !target.is_object() {
                *target = Value::Object(serde_json::Map::new());
            }
            let target_map = target.as_object_mut().unwrap();
            for (key, val) in patch_map {
                if val.is_null() {
                    target_map.remove(key);
                } else {
                    let entry = target_map.entry(key.clone()).or_insert(Value::Null);
                    json_merge_patch(entry, val);
                }
            }
        }
        Value::Null => {} // null patch = no-op at top level
        other => {
            *target = other.clone();
        }
    }
}

/// Check whether a snapshot should be taken given the event count since the last snapshot.
pub fn should_snapshot(events_since_snapshot: i64) -> bool {
    events_since_snapshot >= DEFAULT_SNAPSHOT_INTERVAL
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, EventKind};
    use jamjet_core::workflow::ExecutionId;
    use serde_json::json;

    fn make_event(seq: i64, kind: EventKind) -> Event {
        Event::new(ExecutionId::new(), seq, kind)
    }

    #[test]
    fn test_state_patch_applied() {
        let base = json!({ "x": 1, "y": 2 });
        let events = vec![make_event(
            1,
            EventKind::NodeCompleted {
                node_id: "a".into(),
                output: json!("result"),
                state_patch: json!({ "x": 10 }),
                duration_ms: 5,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
                cost_usd: None,
                provenance: None,
            },
        )];
        let mat = apply_events(base, &events, &WorkflowStatus::Running);
        assert_eq!(mat.current_state["x"], 10);
        assert_eq!(mat.current_state["y"], 2);
        assert!(mat.completed_nodes.contains_key("a"));
    }

    #[test]
    fn test_null_patch_removes_key() {
        let base = json!({ "a": 1, "b": 2 });
        let events = vec![make_event(
            1,
            EventKind::NodeCompleted {
                node_id: "n".into(),
                output: json!(null),
                state_patch: json!({ "b": null }),
                duration_ms: 1,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
                cost_usd: None,
                provenance: None,
            },
        )];
        let mat = apply_events(base, &events, &WorkflowStatus::Running);
        assert!(!mat.current_state.as_object().unwrap().contains_key("b"));
    }

    #[test]
    fn test_workflow_lifecycle_events() {
        let base = json!({});
        let events = vec![
            make_event(
                1,
                EventKind::WorkflowStarted {
                    workflow_id: "wf".into(),
                    workflow_version: "1.0.0".into(),
                    initial_input: json!({}),
                },
            ),
            make_event(
                2,
                EventKind::NodeScheduled {
                    node_id: "a".into(),
                    queue_type: "tool".into(),
                },
            ),
            make_event(
                3,
                EventKind::NodeCompleted {
                    node_id: "a".into(),
                    output: json!("ok"),
                    state_patch: json!({ "result": "ok" }),
                    duration_ms: 10,
                    gen_ai_system: None,
                    gen_ai_model: None,
                    input_tokens: None,
                    output_tokens: None,
                    finish_reason: None,
                    cost_usd: None,
                    provenance: None,
                },
            ),
            make_event(
                4,
                EventKind::WorkflowCompleted {
                    final_state: json!({ "result": "ok" }),
                },
            ),
        ];
        let mat = apply_events(base, &events, &WorkflowStatus::Pending);
        assert_eq!(mat.status, WorkflowStatus::Completed);
        assert!(mat.active_nodes.is_empty());
        assert_eq!(mat.completed_nodes["a"], json!("ok"));
        assert_eq!(mat.last_sequence, 4);
    }

    #[test]
    fn test_json_merge_patch_nested() {
        let mut target = json!({ "a": { "b": 1, "c": 2 }, "d": 3 });
        let patch = json!({ "a": { "b": 10, "c": null }, "e": 5 });
        json_merge_patch(&mut target, &patch);
        assert_eq!(target["a"]["b"], 10);
        assert!(target["a"].as_object().unwrap().get("c").is_none());
        assert_eq!(target["d"], 3);
        assert_eq!(target["e"], 5);
    }

    #[test]
    fn test_should_snapshot() {
        assert!(!should_snapshot(49));
        assert!(should_snapshot(50));
        assert!(should_snapshot(100));
    }
}
