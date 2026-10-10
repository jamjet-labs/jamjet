//! Event logs written before the approval-integrity fields existed must still
//! deserialize. The fields are optional on the wire in both directions: absent
//! on read means `None`, and `None` is not written, so an old reader sees the
//! exact shape it always did.

use jamjet_state::EventKind;
use serde_json::json;

#[test]
fn an_old_approval_received_deserializes_without_a_principal() {
    let old = json!({
        "type": "approval_received",
        "node_id": "gate",
        "user_id": "alice",
        "decision": "approved",
        "comment": null,
        "state_patch": null,
    });
    match serde_json::from_value::<EventKind>(old).expect("old shape must parse") {
        EventKind::ApprovalReceived {
            user_id, principal, ..
        } => {
            assert_eq!(user_id, "alice");
            assert_eq!(principal, None);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn a_principal_round_trips_and_none_is_not_written() {
    let with = EventKind::ApprovalReceived {
        node_id: "gate".into(),
        user_id: "alice".into(),
        decision: jamjet_state::event::ApprovalDecision::Approved,
        comment: None,
        state_patch: None,
        principal: Some("token:abc".into()),
        approved_digest: None,
        request_sequence: None,
    };
    let v = serde_json::to_value(&with).unwrap();
    assert_eq!(v["principal"], "token:abc");
    let back: EventKind = serde_json::from_value(v).unwrap();
    assert!(
        matches!(back, EventKind::ApprovalReceived { principal: Some(p), .. } if p == "token:abc")
    );

    let without = EventKind::ApprovalReceived {
        node_id: "gate".into(),
        user_id: "alice".into(),
        decision: jamjet_state::event::ApprovalDecision::Approved,
        comment: None,
        state_patch: None,
        principal: None,
        approved_digest: None,
        request_sequence: None,
    };
    let v = serde_json::to_value(&without).unwrap();
    assert!(
        v.get("principal").is_none(),
        "None must not be serialized: {v}"
    );
}

/// Review M-8: an event kind a later release adds deserializes as `Unknown`
/// instead of failing the whole log, and the approval fold fails closed on
/// it. A SQLite log holding one still loads.
#[tokio::test]
async fn an_event_kind_from_a_later_release_reads_as_unknown() {
    use jamjet_state::approvals::{node_approval_status, NodeApprovalStatus};
    use jamjet_state::{Event, SqliteBackend, StateBackend};

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

    let mut p = std::env::temp_dir();
    p.push(format!(
        "jamjet_unknown_kind_{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let url = format!("sqlite://{}", p.display());
    let b = SqliteBackend::open(&url).await.unwrap();
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
    let pool = b.pool();
    sqlx::query("INSERT INTO events (id, execution_id, sequence, kind_json, created_at) VALUES (?, ?, 2, ?, ?)")
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(id.0.to_string())
        .bind(future.to_string())
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&pool)
        .await
        .unwrap();
    let loaded = b
        .get_events(&id)
        .await
        .expect("a log with a future kind loads");
    assert_eq!(loaded.len(), 2);
    assert!(matches!(loaded[1].kind, EventKind::Unknown));
}
