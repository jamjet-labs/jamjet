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
