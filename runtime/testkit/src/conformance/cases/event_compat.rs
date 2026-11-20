//! Event logs written before the approval-integrity fields existed must still
//! deserialize, and a stored log holding a kind from a later release still
//! loads.

use crate::conformance::cases::support::skipped;
use crate::conformance::Fixture;
use jamjet_state::EventKind;
use serde_json::json;

/// Review M-8: an event kind a later release adds deserializes as `Unknown`
/// instead of failing the whole log, and the approval fold fails closed on
/// it. A stored log holding one still loads.
pub async fn an_event_kind_from_a_later_release_reads_as_unknown(fx: &Fixture) {
    use jamjet_state::approvals::{node_approval_status, NodeApprovalStatus};
    use jamjet_state::Event;

    let future = json!({ "type": "approval_revoked_v9", "node_id": "gate", "by": "x" });
    let kind = serde_json::from_value::<EventKind>(future.clone()).expect("an unknown kind parses");
    assert!(matches!(kind, EventKind::Unknown), "{kind:?}");
    assert_eq!(kind.node_id(), None);

    let id = jamjet_core::workflow::ExecutionId::new();
    let events = vec![
        Event::new(
            id.clone(),
            1,
            EventKind::ToolApprovalRequired {
                node_id: "gate".into(),
                tool_name: "t".into(),
                approver: "human".into(),
                context: json!({}),
            },
        ),
        Event::new(id.clone(), 2, kind),
    ];
    // It is not ignored: approval state over it is indeterminate (fail closed).
    assert!(matches!(
        node_approval_status(&events, "gate"),
        NodeApprovalStatus::Indeterminate
    ));

    let b = fx.backend.as_ref();
    let now = chrono::Utc::now();
    b.create_execution(jamjet_core::workflow::WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf".into(),
        workflow_version: "1.0.0".into(),
        status: jamjet_core::workflow::WorkflowStatus::Running,
        initial_input: json!({}),
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    })
    .await
    .unwrap();
    b.append_event(events[0].clone()).await.unwrap();
    if !fx
        .hooks
        .insert_event_json(&id, 2, &future.to_string())
        .await
    {
        return skipped(
            fx,
            "memory keeps parsed events, so there is no stored text to read back",
        );
    }
    let loaded = b
        .get_events(&id)
        .await
        .expect("a log with a future kind loads");
    assert_eq!(loaded.len(), 2);
    assert!(matches!(loaded[1].kind, EventKind::Unknown));
}
