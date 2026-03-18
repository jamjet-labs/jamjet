use jamjet_state::event::EventKind;

#[test]
fn coordinator_discovery_round_trip() {
    let event = EventKind::CoordinatorDiscovery {
        node_id: "route_to_analyst".into(),
        query_skills: vec!["data-analysis".into(), "statistics".into()],
        query_trust_domain: Some("internal".into()),
        candidates: vec![serde_json::json!({"uri": "jamjet://org/agent-a", "skills": ["data-analysis"]})],
        filtered_out: vec![serde_json::json!({"uri": "jamjet://org/agent-c", "reason": "trust mismatch"})],
    };
    let json = serde_json::to_string(&event).unwrap();
    let deserialized: EventKind = serde_json::from_str(&json).unwrap();
    assert!(matches!(deserialized, EventKind::CoordinatorDiscovery { .. }));
    assert_eq!(event.node_id(), Some("route_to_analyst"));
}

#[test]
fn coordinator_scoring_round_trip() {
    let event = EventKind::CoordinatorScoring {
        node_id: "route_to_analyst".into(),
        rankings: vec![
            serde_json::json!({"uri": "jamjet://org/agent-a", "composite": 0.92}),
            serde_json::json!({"uri": "jamjet://org/agent-b", "composite": 0.78}),
        ],
        spread: 0.14,
        weights: serde_json::json!({"capability_fit": 1.0, "cost_fit": 1.0}),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deserialized: EventKind = serde_json::from_str(&json).unwrap();
    assert!(matches!(deserialized, EventKind::CoordinatorScoring { .. }));
}

#[test]
fn coordinator_decision_structured_round_trip() {
    let event = EventKind::CoordinatorDecision {
        node_id: "route_to_analyst".into(),
        selected: Some("jamjet://org/agent-a".into()),
        method: "structured".into(),
        reasoning: None,
        confidence: 0.92,
        rejected: vec![serde_json::json!({"uri": "jamjet://org/agent-b", "reason": "lower score"})],
        tiebreaker_tokens: None,
        tiebreaker_cost: None,
    };
    let json = serde_json::to_string(&event).unwrap();
    let deserialized: EventKind = serde_json::from_str(&json).unwrap();
    assert!(matches!(deserialized, EventKind::CoordinatorDecision { .. }));
}

#[test]
fn coordinator_decision_no_candidates() {
    let event = EventKind::CoordinatorDecision {
        node_id: "route".into(),
        selected: None,
        method: "no_candidates".into(),
        reasoning: None,
        confidence: 0.0,
        rejected: vec![],
        tiebreaker_tokens: None,
        tiebreaker_cost: None,
    };
    let json = serde_json::to_string(&event).unwrap();
    let deserialized: EventKind = serde_json::from_str(&json).unwrap();
    match deserialized {
        EventKind::CoordinatorDecision { selected, method, .. } => {
            assert!(selected.is_none());
            assert_eq!(method, "no_candidates");
        }
        _ => panic!("Wrong variant"),
    }
}

#[test]
fn agent_tool_invoked_round_trip() {
    let event = EventKind::AgentToolInvoked {
        node_id: "classify".into(),
        agent_uri: "jamjet://org/classifier".into(),
        mode: "sync".into(),
        protocol: "a2a".into(),
        input_hash: "abc123".into(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deserialized: EventKind = serde_json::from_str(&json).unwrap();
    assert!(matches!(deserialized, EventKind::AgentToolInvoked { .. }));
    assert_eq!(event.node_id(), Some("classify"));
}

#[test]
fn agent_tool_completed_round_trip() {
    let event = EventKind::AgentToolCompleted {
        node_id: "classify".into(),
        output: serde_json::json!({"class": "positive", "confidence": 0.95}),
        provenance: None,
        total_cost: 0.003,
        latency_ms: 150,
        total_turns: None,
    };
    let json = serde_json::to_string(&event).unwrap();
    let deserialized: EventKind = serde_json::from_str(&json).unwrap();
    assert!(matches!(deserialized, EventKind::AgentToolCompleted { .. }));
}

#[test]
fn agent_tool_failed_round_trip() {
    let event = EventKind::AgentToolFailed {
        node_id: "classify".into(),
        failure_type: "delegate_unreachable".into(),
        message: "Connection refused".into(),
        retryable: true,
    };
    let json = serde_json::to_string(&event).unwrap();
    let deserialized: EventKind = serde_json::from_str(&json).unwrap();
    match deserialized {
        EventKind::AgentToolFailed { retryable, .. } => assert!(retryable),
        _ => panic!("Wrong variant"),
    }
}
