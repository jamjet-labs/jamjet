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
        ..Default::default()
    }
}

fn block(patterns: &[&str]) -> PolicySetIr {
    PolicySetIr {
        blocked_tools: patterns.iter().map(|s| s.to_string()).collect(),
        require_approval_for: vec![],
        model_allowlist: vec![],
        ..Default::default()
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

fn python_at(module: &str, function: &str) -> NodeKind {
    NodeKind::PythonFn {
        module: module.into(),
        function: function.into(),
        output_schema: String::new(),
        agent_tool_dispatch: false,
    }
}

/// The Python worker resolves `function` as a dotted attribute chain, so
/// `payments.transfer:send` and `payments:transfer.send` run the same code. A
/// pattern in either spelling must catch both (round-3 review, I2).
#[test]
fn a_handler_pattern_matches_every_spelling_of_the_same_dotted_path() {
    let spellings = [("payments.transfer", "send"), ("payments", "transfer.send")];
    let patterns = [
        "payments.transfer:send",
        "payments:transfer.send",
        "payments.transfer:*",
        "payments.*:send",
        "payments.transfer.send",
    ];
    for (module, function) in spellings {
        for pattern in patterns {
            assert!(
                gated(&decide(
                    "x",
                    &python_at(module, function),
                    &approve(&[pattern])
                )),
                "{pattern} must gate {module}:{function}"
            );
            assert!(
                blocked(&decide(
                    "x",
                    &python_at(module, function),
                    &block(&[pattern])
                )),
                "{pattern} must block {module}:{function}"
            );
        }
    }
}

#[test]
fn a_java_handler_pattern_matches_the_dotted_form() {
    for pattern in [
        "com.acme.Payments/transfer",
        "com.acme.Payments.transfer",
        "com.acme.*.transfer",
    ] {
        assert!(
            gated(&decide("x", &java_fn(), &approve(&[pattern]))),
            "{pattern}"
        );
    }
}

/// Canonical forms apply only to code-backed kinds: a colon pattern does not
/// start matching tool names it never matched.
#[test]
fn canonical_matching_does_not_widen_tool_patterns() {
    let tool = NodeKind::Tool {
        tool_ref: "payments.transfer".into(),
        input_mapping: Default::default(),
        output_schema: String::new(),
    };
    assert!(matches!(
        decide("x", &tool, &approve(&["payments:transfer"])),
        PolicyDecision::Allow
    ));
}

/// An `mcp_tool` node is named by its `tool`. The name lookup read a
/// `tool_name` field the kind does not have, so no pattern ever matched one.
#[test]
fn an_mcp_tool_node_is_named_by_its_tool() {
    let mcp = NodeKind::McpTool {
        server: "db".into(),
        tool: "delete_table".into(),
        input_mapping: Default::default(),
        output_schema: String::new(),
    };
    assert!(blocked(&decide("x", &mcp, &block(&["delete_*"]))));
    assert!(gated(&decide("x", &mcp, &approve(&["delete_table"]))));
    assert_eq!(
        EvaluationContext::from_node_kind("x", &mcp)
            .tool_name
            .as_deref(),
        Some("delete_table")
    );
}

/// A `finalizer` node runs the tool its `tool_ref` names (claimed off the
/// `tool` queue), so it is named by that tool. It had no name, so no
/// `blocked_tools` pattern and no agent `allowed_tools` list could see it.
#[test]
fn a_finalizer_node_is_named_by_its_tool() {
    let fin = NodeKind::Finalizer {
        tool_ref: "send_wire".into(),
        run_on: jamjet_core::node::FinalizerTrigger::Always,
    };
    assert!(blocked(&decide("x", &fin, &block(&["send_*"]))));
    assert_eq!(
        EvaluationContext::from_node_kind("x", &fin)
            .tool_name
            .as_deref(),
        Some("send_wire")
    );
}

/// An `agent_tool` node with an explicit target is named by that agent.
#[test]
fn an_agent_tool_node_is_named_by_its_agent() {
    let at: NodeKind = serde_json::from_value(serde_json::json!({
        "type": "agent_tool", "agent": { "explicit": "https://x.example/wire" },
        "output_key": "o", "timeout_ms": null, "budget": null
    }))
    .unwrap();
    assert!(blocked(&decide("x", &at, &block(&["https://x.example/*"]))));
}
