//! The approvals listing reports the same thing on both of its read paths.
//!
//! `GET /executions/:id/approvals` serves from the projection once the
//! projector has visited a run, and from the event log before that. The event
//! log path reported a decided approval's `approved_digest`; the projection
//! path dropped it, so the same decision read differently depending on timing.

use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::StateBackend;
use jamjet_state::event::{ApprovalDecision, EventKind};
use jamjet_state::{Event, InMemoryBackend};
use serde_json::json;
use std::sync::Arc;

mod support;

async fn append(backend: &Arc<InMemoryBackend>, eid: &ExecutionId, kind: EventKind) {
    let seq = backend.latest_sequence(eid).await.unwrap() + 1;
    backend
        .append_event(Event::new(eid.clone(), seq, kind))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_decided_approval_reports_its_digest_from_the_projection() {
    let backend = Arc::new(InMemoryBackend::new());
    let eid = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: eid.clone(),
            workflow_id: "wf".into(),
            workflow_version: "0.1.0".into(),
            status: WorkflowStatus::Running,
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
    append(
        &backend,
        &eid,
        EventKind::ToolApprovalRequired {
            node_id: "gate".into(),
            tool_name: "payments.transfer".into(),
            approver: "human".into(),
            context: json!({ "node_id": "gate", "action_digest": "d-1" }),
        },
    )
    .await;
    let seq = backend.latest_sequence(&eid).await.unwrap();
    append(
        &backend,
        &eid,
        EventKind::ApprovalReceived {
            node_id: "gate".into(),
            user_id: "bob".into(),
            decision: ApprovalDecision::Approved,
            comment: None,
            state_patch: None,
            principal: Some("token:bob".into()),
            approved_digest: Some("d-1".into()),
            request_sequence: Some(seq),
        },
    )
    .await;

    let state = support::make_state(backend.clone() as Arc<dyn StateBackend>);

    // Event-log path (nothing projected yet).
    let (_, before) = support::get(&state, &format!("/executions/{eid}/approvals")).await;
    assert_eq!(before["decided"][0]["approved_digest"], "d-1", "{before}");

    // Projection path.
    jamjet_scheduler::Projector::new(backend.clone() as Arc<dyn StateBackend>)
        .tick()
        .await
        .unwrap();
    assert!(!backend
        .get_approval_projection(&eid)
        .await
        .unwrap()
        .is_empty());
    let (_, after) = support::get(&state, &format!("/executions/{eid}/approvals")).await;
    assert_eq!(after["decided"][0]["approved_digest"], "d-1", "{after}");
}
