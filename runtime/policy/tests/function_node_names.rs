//! Code-backed nodes (python_fn, java_fn, external_fn) carry no tool name, so
//! `require_approval_for` and `blocked_tools` never matched them and a
//! developer could reach any gated tool's code through one (review I3). Their
//! policy context now carries two names: the handler reference and the node
//! id. A pattern applies if it matches either.

use jamjet_core::node::NodeKind;
use jamjet_ir::workflow::PolicySetIr;
use jamjet_policy::{EvaluationContext, PolicyDecision, PolicyEvaluator};

fn approve(patterns: &[&str]) -> PolicySetIr {
    PolicySetIr {
        blocked_tools: vec![],
        require_approval_for: patterns.iter().map(|s| s.to_string()).collect(),
        model_allowlist: vec![],
    }
}

fn block(patterns: &[&str]) -> PolicySetIr {
    PolicySetIr {
        blocked_tools: patterns.iter().map(|s| s.to_string()).collect(),
        require_approval_for: vec![],
        model_allowlist: vec![],
    }
}

fn python_fn() -> NodeKind {
    NodeKind::PythonFn {
        module: "myapp.payments".into(),
        function: "transfer".into(),
        output_schema: String::new(),
        agent_tool_dispatch: false,
    }
}

fn java_fn() -> NodeKind {
    NodeKind::JavaFn {
        class_name: "com.acme.Payments".into(),
        method: "transfer".into(),
        output_schema: String::new(),
        agent_tool_dispatch: false,
    }
}

fn external_fn() -> NodeKind {
    NodeKind::ExternalFn {
        queue: "pas-control".into(),
        handler: "transfer".into(),
        output_schema: String::new(),
    }
}

fn decide(node_id: &str, kind: &NodeKind, policy: &PolicySetIr) -> PolicyDecision {
    let ctx = EvaluationContext::from_node_kind(node_id, kind);
    PolicyEvaluator.evaluate(&ctx, &[policy])
}

fn gated(d: &PolicyDecision) -> bool {
    matches!(d, PolicyDecision::RequireApproval { .. })
}

fn blocked(d: &PolicyDecision) -> bool {
    matches!(d, PolicyDecision::Block { .. })
}

#[test]
fn a_star_gates_every_function_kind() {
    for kind in [python_fn(), java_fn(), external_fn()] {
        assert!(gated(&decide("pay", &kind, &approve(&["*"]))), "{kind:?}");
        assert!(blocked(&decide("pay", &kind, &block(&["*"]))), "{kind:?}");
    }
}

#[test]
fn the_handler_reference_is_a_policy_name() {
    assert!(gated(&decide(
        "pay",
        &python_fn(),
        &approve(&["myapp.payments:transfer"])
    )));
    assert!(gated(&decide(
        "pay",
        &python_fn(),
        &approve(&["myapp.payments:*"])
    )));
    assert!(gated(&decide(
        "pay",
        &java_fn(),
        &approve(&["com.acme.Payments/transfer"])
    )));
    assert!(gated(&decide(
        "pay",
        &external_fn(),
        &approve(&["pas-control/transfer"])
    )));
    assert!(blocked(&decide(
        "pay",
        &external_fn(),
        &block(&["pas-control/*"])
    )));
}

#[test]
fn the_node_id_is_a_policy_name() {
    for kind in [python_fn(), java_fn(), external_fn()] {
        assert!(
            gated(&decide(
                "payments.transfer",
                &kind,
                &approve(&["payments.*"])
            )),
            "{kind:?}"
        );
        assert!(
            blocked(&decide("payments.transfer", &kind, &block(&["payments.*"]))),
            "{kind:?}"
        );
    }
}

#[test]
fn an_unrelated_pattern_does_not_match() {
    for kind in [python_fn(), java_fn(), external_fn()] {
        assert!(
            matches!(
                decide("pay", &kind, &approve(&["other.*"])),
                PolicyDecision::Allow
            ),
            "{kind:?}"
        );
    }
}

/// The node id is a name for code-backed kinds only. A tool node keeps being
/// matched on its tool name, so existing tool policies do not change meaning.
#[test]
fn a_tool_node_is_not_matched_on_its_node_id() {
    let tool = NodeKind::Tool {
        tool_ref: "search".into(),
        input_mapping: Default::default(),
        output_schema: String::new(),
    };
    assert!(matches!(
        decide("payments.transfer", &tool, &approve(&["payments.*"])),
        PolicyDecision::Allow
    ));
    assert!(gated(&decide("x", &tool, &approve(&["search"]))));
}

/// The approval request names the handler, so a reviewer sees what code runs.
#[test]
fn the_tool_name_of_a_function_node_is_its_handler_reference() {
    let ctx = EvaluationContext::from_node_kind("pay", &python_fn());
    assert_eq!(ctx.tool_name.as_deref(), Some("myapp.payments:transfer"));
    let ctx = EvaluationContext::from_node_kind("pay", &java_fn());
    assert_eq!(ctx.tool_name.as_deref(), Some("com.acme.Payments/transfer"));
    let ctx = EvaluationContext::from_node_kind("pay", &external_fn());
    assert_eq!(ctx.tool_name.as_deref(), Some("pas-control/transfer"));
}
