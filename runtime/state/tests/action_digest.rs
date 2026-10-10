//! The approval action digest and the rule that decides whether an approval
//! covers an action.

use jamjet_core::workflow::ExecutionId;
use jamjet_state::approvals::{
    action_digest, approval_covers, node_approval_status, NodeApprovalStatus,
};
use jamjet_state::{Event, EventKind};
use serde_json::json;

#[test]
fn every_component_changes_the_digest() {
    let base = action_digest("run", 0, "gate", "def", &json!({ "amount": 10 }));
    assert_eq!(
        base,
        action_digest("run", 0, "gate", "def", &json!({ "amount": 10 }))
    );
    assert_ne!(
        base,
        action_digest("other", 0, "gate", "def", &json!({ "amount": 10 })),
        "run"
    );
    assert_ne!(
        base,
        action_digest("run", 1, "gate", "def", &json!({ "amount": 10 })),
        "step (loop)"
    );
    assert_ne!(
        base,
        action_digest("run", 0, "other", "def", &json!({ "amount": 10 })),
        "node"
    );
    assert_ne!(
        base,
        action_digest("run", 0, "gate", "def", &json!({ "amount": 11 })),
        "state"
    );
    assert_ne!(
        base,
        action_digest("run", 0, "gate", "other-def", &json!({ "amount": 10 })),
        "definition (a different node body under the same id)"
    );
}

#[test]
fn the_definition_hash_covers_the_node_body_and_its_policy() {
    use jamjet_state::approvals::definition_hash;
    let node = json!({ "id": "gate", "kind": { "type": "tool", "tool_ref": "payments.transfer" } });
    let base = definition_hash(&node);
    let mut other_tool = node.clone();
    other_tool["kind"]["tool_ref"] = json!("payments.refund");
    assert_ne!(base, definition_hash(&other_tool), "tool");
    let mut with_policy = node.clone();
    with_policy["policy"] = json!({ "require_approval_for": ["payments.*"] });
    assert_ne!(base, definition_hash(&with_policy), "policy");
    // Canonical: key order does not matter.
    let reordered: serde_json::Value = serde_json::from_str(
        r#"{ "kind": { "tool_ref": "payments.transfer", "type": "tool" }, "id": "gate" }"#,
    )
    .unwrap();
    assert_eq!(base, definition_hash(&reordered));
}

#[test]
fn the_digest_is_not_the_idempotency_key() {
    // Domain separation: same coordinates, different purpose, different hash.
    let state = json!({});
    let digest = action_digest("run", 0, "gate", "def", &state);
    let key =
        jamjet_state::idempotency_key("run", 0, 0, "gate", &jamjet_state::content_hash(&state));
    assert_ne!(digest, key);
}

fn approved(digest: Option<&str>) -> NodeApprovalStatus {
    NodeApprovalStatus::Approved {
        user_id: "a".into(),
        sequence: 2,
        approved_digest: digest.map(String::from),
    }
}

#[test]
fn only_a_matching_approved_digest_covers() {
    assert!(approval_covers(&approved(Some("d1")), "d1"));
    assert!(
        !approval_covers(&approved(Some("d1")), "d2"),
        "different action"
    );
    assert!(!approval_covers(&approved(None), "d1"), "legacy approval");
    assert!(!approval_covers(&NodeApprovalStatus::NotRequested, "d1"));
    let rejected = NodeApprovalStatus::Rejected {
        user_id: "a".into(),
        comment: None,
        sequence: 2,
    };
    assert!(!approval_covers(&rejected, "d1"));
}

#[test]
fn the_fold_carries_the_approved_digest() {
    let eid = ExecutionId::new();
    let events = vec![
        Event::new(
            eid.clone(),
            1,
            EventKind::ToolApprovalRequired {
                node_id: "gate".into(),
                tool_name: "t".into(),
                approver: "human".into(),
                context: json!({ "action_digest": "d1" }),
            },
        ),
        Event::new(
            eid,
            2,
            serde_json::from_value(json!({
                "type": "approval_received", "node_id": "gate", "user_id": "a",
                "decision": "approved", "comment": null, "state_patch": null,
                "approved_digest": "d1", "request_sequence": 1
            }))
            .unwrap(),
        ),
    ];
    match node_approval_status(&events, "gate") {
        NodeApprovalStatus::Approved {
            approved_digest, ..
        } => {
            assert_eq!(approved_digest.as_deref(), Some("d1"))
        }
        other => panic!("expected Approved, got {other:?}"),
    }
}

#[test]
fn an_old_approval_received_parses_with_no_binding() {
    let kind: EventKind = serde_json::from_value(json!({
        "type": "approval_received", "node_id": "gate", "user_id": "a",
        "decision": "approved", "comment": null, "state_patch": null
    }))
    .unwrap();
    match kind {
        EventKind::ApprovalReceived {
            approved_digest,
            request_sequence,
            principal,
            ..
        } => {
            assert_eq!(approved_digest, None);
            assert_eq!(request_sequence, None);
            assert_eq!(principal, None);
        }
        other => panic!("wrong variant {other:?}"),
    }
    // And None is not written back.
    let v = serde_json::to_value(EventKind::ApprovalReceived {
        node_id: "gate".into(),
        user_id: "a".into(),
        decision: jamjet_state::event::ApprovalDecision::Approved,
        comment: None,
        state_patch: None,
        principal: None,
        approved_digest: None,
        request_sequence: None,
    })
    .unwrap();
    assert!(
        v.get("approved_digest").is_none() && v.get("request_sequence").is_none(),
        "{v}"
    );
}

#[test]
fn engine_state_keys_cover_the_budget_writer() {
    // The digest excludes exactly ENGINE_STATE_KEYS. If the engine's budget
    // writer ever puts another key into state, it must be listed, or every
    // sibling completion would void waiting approvals again.
    let mut state = json!({});
    jamjet_state::BudgetState::default().patch_into_snapshot_state(&mut state);
    for key in state.as_object().unwrap().keys() {
        assert!(
            jamjet_state::approvals::ENGINE_STATE_KEYS.contains(&key.as_str()),
            "engine-written key {key} is not excluded from the digest"
        );
    }
}

#[test]
fn only_the_engine_keys_are_left_out_of_what_is_bound() {
    use jamjet_state::approvals::action_state;
    let state = json!({ "__budget": { "spent": 1 }, "__limits": {}, "amount": 10 });
    assert_eq!(
        action_state(&state),
        json!({ "__limits": {}, "amount": 10 })
    );
}
