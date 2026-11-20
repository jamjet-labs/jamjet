//! A continue-as-new segment inherits its run's initiator.
//!
//! Separation of duties keys on `WorkflowStarted.initiated_by`. A rollover
//! appends a fresh `WorkflowStarted` on the new segment; if it dropped the
//! initiator, every approval after the first rollover would escape the check.
//!
//! The cases that start a segment on a backend are in the backend suite
//! (`runtime/testkit`, module `initiator`). This file keeps the wire check.

use jamjet_state::EventKind;
use serde_json::json;

/// Parsed from JSON so the test pins the wire field name.
fn started(initiated_by: Option<&str>) -> EventKind {
    let mut v = json!({
        "type": "workflow_started",
        "workflow_id": "seg-wf",
        "workflow_version": "1.0.0",
        "initial_input": {},
    });
    if let Some(p) = initiated_by {
        v["initiated_by"] = json!(p);
    }
    serde_json::from_value(v).unwrap()
}

#[test]
fn an_old_workflow_started_deserializes_without_an_initiator() {
    let kind = started(None);
    assert!(serde_json::to_value(&kind)
        .unwrap()
        .get("initiated_by")
        .is_none());
}
