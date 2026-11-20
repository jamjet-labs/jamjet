//! Event logs written before the approval-integrity fields existed must still
//! deserialize. The fields are optional on the wire in both directions: absent
//! on read means `None`, and `None` is not written, so an old reader sees the
//! exact shape it always did.
//!
//! The case that loads a stored log is in the backend suite
//! (`runtime/testkit`, module `event_compat`).

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
        choice: None,
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
        choice: None,
    };
    let v = serde_json::to_value(&without).unwrap();
    assert!(
        v.get("principal").is_none(),
        "None must not be serialized: {v}"
    );
}

/// A decision recorded before options existed has no `choice`.
#[test]
fn an_approval_received_without_choice_reads() {
    let old = json!({
        "type": "approval_received",
        "node_id": "pick_topic",
        "user_id": "bob",
        "decision": "approved",
        "comment": "ship it",
        "state_patch": null,
        "principal": "token:abc",
        "approved_digest": "9f2c",
        "request_sequence": 4,
    });
    match serde_json::from_value::<EventKind>(old).expect("old shape must parse") {
        EventKind::ApprovalReceived {
            choice,
            comment,
            request_sequence,
            ..
        } => {
            assert_eq!(choice, None);
            assert_eq!(comment.as_deref(), Some("ship it"));
            assert_eq!(request_sequence, Some(4));
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn a_choice_round_trips_and_none_is_not_written() {
    let decision = |choice: Option<&str>| EventKind::ApprovalReceived {
        node_id: "pick_topic".into(),
        user_id: "bob".into(),
        decision: jamjet_state::event::ApprovalDecision::Approved,
        comment: Some("Go with the second one.".into()),
        state_patch: None,
        principal: Some("token:abc".into()),
        approved_digest: Some("9f2c".into()),
        request_sequence: Some(4),
        choice: choice.map(String::from),
    };

    let with = serde_json::to_value(decision(Some("topic_2"))).unwrap();
    assert_eq!(
        with,
        json!({
            "type": "approval_received",
            "node_id": "pick_topic",
            "user_id": "bob",
            "decision": "approved",
            "comment": "Go with the second one.",
            "state_patch": null,
            "principal": "token:abc",
            "approved_digest": "9f2c",
            "request_sequence": 4,
            "choice": "topic_2",
        })
    );
    let back: EventKind = serde_json::from_value(with).unwrap();
    assert!(
        matches!(&back, EventKind::ApprovalReceived { choice: Some(c), .. } if c == "topic_2"),
        "{back:?}"
    );

    let without = serde_json::to_value(decision(None)).unwrap();
    assert_eq!(
        without,
        json!({
            "type": "approval_received",
            "node_id": "pick_topic",
            "user_id": "bob",
            "decision": "approved",
            "comment": "Go with the second one.",
            "state_patch": null,
            "principal": "token:abc",
            "approved_digest": "9f2c",
            "request_sequence": 4,
        }),
        "None must not be serialized"
    );
}
