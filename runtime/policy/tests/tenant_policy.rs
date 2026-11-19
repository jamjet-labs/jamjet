//! Tests for tenant-level policy scope (4.2).
//!
//! The tenant policy is a ceiling over the IR's own layers (workflow, node):
//! deny overrides. Inside the IR the most specific layer still wins, since
//! one author owns both.

use jamjet_ir::workflow::PolicySetIr;
use jamjet_policy::{EvaluationContext, PolicyDecision, PolicyEvaluator};

fn set(blocked: &[&str], approval: &[&str], allowlist: &[&str]) -> PolicySetIr {
    PolicySetIr {
        blocked_tools: blocked.iter().map(|s| s.to_string()).collect(),
        require_approval_for: approval.iter().map(|s| s.to_string()).collect(),
        model_allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn tool(name: &str) -> EvaluationContext {
    EvaluationContext {
        node_id: "n1".into(),
        node_kind_tag: "tool".into(),
        tool_name: Some(name.into()),
        model_ref: None,
    }
}

fn model(model_ref: &str) -> EvaluationContext {
    EvaluationContext {
        node_id: "n1".into(),
        node_kind_tag: "model".into(),
        tool_name: None,
        model_ref: Some(model_ref.into()),
    }
}

fn decide(
    ctx: &EvaluationContext,
    tenant: &PolicySetIr,
    layers: &[&PolicySetIr],
) -> PolicyDecision {
    PolicyEvaluator.evaluate_chain(ctx, Some(tenant), layers)
}

fn is_block(d: &PolicyDecision) -> bool {
    matches!(d, PolicyDecision::Block { .. })
}

fn is_gate(d: &PolicyDecision) -> bool {
    matches!(d, PolicyDecision::RequireApproval { .. })
}

#[test]
fn a_tenant_block_applies_under_empty_ir_layers() {
    let tenant = set(&["dangerous_tool"], &[], &[]);
    let empty = set(&[], &[], &[]);
    let d = decide(&tool("dangerous_tool"), &tenant, &[&empty, &empty]);
    assert!(is_block(&d), "{d:?}");
}

#[test]
fn a_tenant_glob_block_applies_through_the_full_chain() {
    let tenant = set(&["delete_*"], &[], &[]);
    let empty = set(&[], &[], &[]);
    let d = decide(&tool("delete_user"), &tenant, &[&empty, &empty]);
    assert!(is_block(&d), "{d:?}");
}

/// Until 2026-11 a node `require_approval_for` matching the same name turned
/// the tenant's block into an approval request: the IR author could get a
/// tool the operator blocked run by asking someone to approve it.
#[test]
fn a_node_approval_rule_does_not_downgrade_a_tenant_block() {
    let tenant = set(&["tool_a"], &[], &[]);
    let node = set(&[], &["tool_a"], &[]);
    let d = decide(&tool("tool_a"), &tenant, &[&node]);
    assert!(is_block(&d), "{d:?}");
}

#[test]
fn a_workflow_approval_rule_does_not_downgrade_a_tenant_block() {
    let tenant = set(&["deploy_*"], &[], &[]);
    let workflow = set(&[], &["deploy_*"], &[]);
    let d = decide(&tool("deploy_staging"), &tenant, &[&workflow]);
    assert!(is_block(&d), "{d:?}");
}

/// The block reported is the tenant's, so the audit names the rule that
/// cannot be lifted.
#[test]
fn a_tenant_block_is_reported_before_an_ir_block() {
    let tenant = set(&["deploy_*"], &[], &[]);
    let workflow = set(&["deploy_staging"], &[], &[]);
    match decide(&tool("deploy_staging"), &tenant, &[&workflow]) {
        PolicyDecision::Block { reason } => assert!(reason.contains("'deploy_*'"), "{reason}"),
        other => panic!("expected Block, got {other:?}"),
    }
}

#[test]
fn a_tenant_gate_holds_a_tool_the_ir_allows() {
    let tenant = set(&[], &["deploy_*"], &[]);
    let empty = set(&[], &[], &[]);
    assert!(is_gate(&decide(&tool("deploy_prod"), &tenant, &[&empty])));
}

#[test]
fn an_ir_block_stands_under_a_tenant_gate() {
    let tenant = set(&[], &["deploy_*"], &[]);
    let workflow = set(&["deploy_prod"], &[], &[]);
    assert!(is_block(&decide(
        &tool("deploy_prod"),
        &tenant,
        &[&workflow]
    )));
}

/// Inside the IR the most specific layer still wins: its author owns both.
#[test]
fn a_node_approval_rule_still_downgrades_a_workflow_block() {
    let empty = set(&[], &[], &[]);
    let workflow = set(&["tool_a"], &[], &[]);
    let node = set(&[], &["tool_a"], &[]);
    assert!(is_gate(&decide(
        &tool("tool_a"),
        &empty,
        &[&workflow, &node]
    )));
}

#[test]
fn tenant_policy_deserialized_from_json() {
    let json = serde_json::json!({
        "blocked_tools": ["rm_*", "drop_*"],
        "require_approval_for": ["deploy_*"],
        "model_allowlist": ["gpt-4", "claude-*"]
    });
    let policy: PolicySetIr = serde_json::from_value(json).unwrap();
    assert_eq!(policy.blocked_tools.len(), 2);
    assert_eq!(policy.require_approval_for.len(), 1);
    assert_eq!(policy.model_allowlist.len(), 2);
}

// ── Model allowlists ─────────────────────────────────────────────────────────

#[test]
fn tenant_model_allowlist_blocks_unallowed_model() {
    let tenant = set(&[], &[], &["claude-*"]);
    let empty = set(&[], &[], &[]);
    assert!(is_block(&decide(&model("gpt-4"), &tenant, &[&empty])));
}

/// Until 2026-11 an IR allowlist that listed the model returned Allow before
/// the tenant's allowlist was read, so a workflow could widen it.
#[test]
fn an_ir_allowlist_does_not_widen_the_tenant_allowlist() {
    let tenant = set(&[], &[], &["anthropic"]);
    let workflow = set(&[], &[], &["openai/*"]);
    let node = set(&[], &[], &["openai/gpt-4o"]);
    for layers in [vec![&workflow], vec![&node], vec![&workflow, &node]] {
        let d = decide(&model("openai/gpt-4o"), &tenant, &layers);
        assert!(is_block(&d), "{d:?}");
    }
}

/// The two allowlists intersect: a model must be on both.
#[test]
fn tenant_and_ir_allowlists_intersect() {
    let tenant = set(&[], &[], &["anthropic"]);
    let node = set(&[], &[], &["anthropic/claude-opus-*"]);
    assert!(matches!(
        decide(&model("anthropic/claude-opus-4-8"), &tenant, &[&node]),
        PolicyDecision::Allow
    ));
    assert!(
        is_block(&decide(
            &model("anthropic/claude-haiku-4-5"),
            &tenant,
            &[&node]
        )),
        "the IR may narrow the tenant's list"
    );
}

// ── Catch-alls ───────────────────────────────────────────────────────────────

/// A model node has no tool name, so until 2026-11 no pattern matched it and
/// a tenant `require_approval_for: ["*"]` let every model call through.
#[test]
fn a_tenant_catch_all_gate_holds_a_model_node() {
    for pattern in ["*", "**"] {
        let tenant = set(&[], &[pattern], &[]);
        let empty = set(&[], &[], &[]);
        assert!(
            is_gate(&decide(
                &model("anthropic/claude-opus-4-8"),
                &tenant,
                &[&empty]
            )),
            "{pattern}"
        );
    }
}

/// An IR allowlist the model is on does not skip the tenant's gate, and nor
/// does the tenant's own allowlist.
#[test]
fn a_tenant_catch_all_gate_holds_a_model_on_both_allowlists() {
    let tenant = set(&[], &["*"], &["anthropic"]);
    let node = set(&[], &[], &["anthropic/*"]);
    assert!(is_gate(&decide(
        &model("anthropic/claude-opus-4-8"),
        &tenant,
        &[&node]
    )));
}

#[test]
fn a_tenant_catch_all_block_blocks_a_model_node() {
    let tenant = set(&["*"], &[], &[]);
    let node = set(&[], &[], &["anthropic/*"]);
    assert!(is_block(&decide(
        &model("anthropic/claude-opus-4-8"),
        &tenant,
        &[&node]
    )));
}

/// Only a pattern that matches every name reaches a model node. A narrower
/// tenant pattern is matched against tool names, which a model node has none
/// of.
#[test]
fn a_narrow_tenant_pattern_does_not_reach_a_model_node() {
    for pattern in ["*_*", "anthropic*", "?*"] {
        let tenant = set(&[], &[pattern], &[]);
        assert!(
            matches!(
                decide(&model("anthropic/claude-opus-4-8"), &tenant, &[]),
                PolicyDecision::Allow
            ),
            "{pattern}"
        );
    }
}

/// In the IR `["*"]` means every tool (`approval_required=True` compiles to
/// it), so it does not gate the agent's own model calls.
#[test]
fn an_ir_catch_all_does_not_gate_a_model_node() {
    let workflow = set(&[], &["*"], &[]);
    assert!(matches!(
        PolicyEvaluator.evaluate_chain(&model("anthropic/claude-opus-4-8"), None, &[&workflow]),
        PolicyDecision::Allow
    ));
}

#[test]
fn a_tenant_catch_all_gate_holds_a_tool() {
    let tenant = set(&[], &["*"], &[]);
    assert!(is_gate(&decide(&tool("anything"), &tenant, &[])));
}

#[test]
fn empty_tenant_policy_allows_everything() {
    let empty = set(&[], &[], &[]);
    assert!(matches!(
        decide(&tool("any_tool"), &empty, &[&empty]),
        PolicyDecision::Allow
    ));
    assert!(matches!(
        decide(&model("openai/gpt-4o"), &empty, &[&empty]),
        PolicyDecision::Allow
    ));
}

#[test]
fn no_tenant_policy_leaves_the_ir_decision() {
    let workflow = set(&["tool_a"], &[], &[]);
    let node = set(&[], &["tool_a"], &[]);
    assert!(is_gate(&PolicyEvaluator.evaluate_chain(
        &tool("tool_a"),
        None,
        &[&workflow, &node]
    )));
}

/// `tool:*` is the tools-only catch-all: every tool, never a model node, in
/// the tenant layer and in the IR. A tenant `*` covers model nodes too, so
/// an operator who wants every tool call approved but not every model turn
/// needs a way to say so.
#[test]
fn tool_star_covers_every_tool_and_no_model_node() {
    for pattern in ["tool:*", "tool:**"] {
        // Tenant gate and block.
        let gate = set(&[], &[pattern], &[]);
        assert!(
            is_gate(&decide(&tool("send_wire"), &gate, &[])),
            "{pattern}"
        );
        assert!(is_gate(&decide(&tool("db.drop"), &gate, &[])), "{pattern}");
        assert!(
            matches!(
                decide(&model("anthropic/claude-sonnet-4-6"), &gate, &[]),
                PolicyDecision::Allow
            ),
            "{pattern} must not hold a model node"
        );
        let block = set(&[pattern], &[], &[]);
        assert!(
            is_block(&decide(&tool("send_wire"), &block, &[])),
            "{pattern}"
        );
        assert!(
            matches!(
                decide(&model("anthropic/claude-sonnet-4-6"), &block, &[]),
                PolicyDecision::Allow
            ),
            "{pattern} must not block a model node"
        );
        // The same in an IR layer, where it matches what `*` matches.
        let workflow = set(&[], &[pattern], &[]);
        assert!(is_gate(&PolicyEvaluator.evaluate_chain(
            &tool("x"),
            None,
            &[&workflow]
        )));
        assert!(matches!(
            PolicyEvaluator.evaluate_chain(&model("anthropic/x"), None, &[&workflow]),
            PolicyDecision::Allow
        ));
    }
    // `*` still means every action at the tenant.
    assert!(is_gate(&decide(
        &model("anthropic/x"),
        &set(&[], &["*"], &[]),
        &[]
    )));
    // Only the catch-all is special: `tool:send` is the handler spelling it
    // always was, not "the tool named send".
    assert!(!is_gate(&decide(
        &tool("send"),
        &set(&[], &["tool:send"], &[]),
        &[]
    )));
    assert!(!jamjet_policy::is_tools_catch_all("tool:send*"));
    assert!(!jamjet_policy::is_tools_catch_all("tools:*"));
}

/// The cases in `sdk/python/tests/fixtures/blocked_tool_patterns.json`, which
/// the in-process Python runtime is tested on too: a `blocked_tools` pattern
/// blocks the same tools in both. `tool:*` blocked nothing in-process.
#[test]
fn blocked_tool_patterns_match_the_in_process_runtime() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdk/python/tests/fixtures/blocked_tool_patterns.json");
    let all: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let cases = all["cases"].as_array().unwrap();
    assert!(cases.len() > 10, "{} cases", cases.len());
    for case in cases {
        let pattern = case["pattern"].as_str().unwrap();
        let name = case["tool"].as_str().unwrap();
        let blocks = case["blocks"].as_bool().unwrap();
        let layer = set(&[pattern], &[], &[]);
        let d = PolicyEvaluator.evaluate_chain(&tool(name), None, &[&layer]);
        assert_eq!(is_block(&d), blocks, "{pattern:?} on {name:?}: {d:?}");
    }
}
