//! Tests for tenant-level policy scope (4.2).
//!
//! Validates the four-tier policy chain: global -> tenant -> workflow -> node.
//! The evaluator iterates in reverse (most-specific first), so node-level
//! rules take priority over workflow, which takes priority over tenant, etc.

use jamjet_ir::workflow::PolicySetIr;
use jamjet_policy::{EvaluationContext, PolicyDecision, PolicyEvaluator};

#[test]
fn tenant_policy_blocks_tool_overriding_global() {
    let global = PolicySetIr {
        blocked_tools: vec![],
        require_approval_for: vec![],
        model_allowlist: vec![],
    };
    let tenant = PolicySetIr {
        blocked_tools: vec!["dangerous_tool".into()],
        require_approval_for: vec![],
        model_allowlist: vec![],
    };
    let workflow = PolicySetIr {
        blocked_tools: vec![],
        require_approval_for: vec![],
        model_allowlist: vec![],
    };
    let ev = PolicyEvaluator;
    let ctx = EvaluationContext {
        node_id: "n1".into(),
        node_kind_tag: "tool".into(),
        tool_name: Some("dangerous_tool".into()),
        model_ref: None,
    };
    // Order: global (least specific) -> tenant -> workflow (most specific).
    // Evaluator iterates reverse: workflow (empty) -> tenant (blocks dangerous_tool) -> global.
    // Tenant blocks it.
    let decision = ev.evaluate(&ctx, &[&global, &tenant, &workflow]);
    assert!(matches!(decision, PolicyDecision::Block { .. }));
}

#[test]
fn tenant_block_with_node_approval_override() {
    let tenant = PolicySetIr {
        blocked_tools: vec!["tool_a".into()],
        require_approval_for: vec![],
        model_allowlist: vec![],
    };
    let node = PolicySetIr {
        blocked_tools: vec![],
        require_approval_for: vec!["tool_a".into()],
        model_allowlist: vec![],
    };
    let ev = PolicyEvaluator;
    let ctx = EvaluationContext {
        node_id: "n1".into(),
        node_kind_tag: "tool".into(),
        tool_name: Some("tool_a".into()),
        model_ref: None,
    };
    // Order: tenant -> node (most specific).
    // Node has require_approval_for tool_a, checked first -> RequireApproval wins.
    let decision = ev.evaluate(&ctx, &[&tenant, &node]);
    assert!(matches!(decision, PolicyDecision::RequireApproval { .. }));
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

#[test]
fn four_scope_chain_tenant_between_global_and_workflow() {
    let global = PolicySetIr {
        blocked_tools: vec![],
        require_approval_for: vec![],
        model_allowlist: vec![],
    };
    let tenant = PolicySetIr {
        blocked_tools: vec!["delete_*".into()],
        require_approval_for: vec![],
        model_allowlist: vec![],
    };
    let workflow = PolicySetIr {
        blocked_tools: vec![],
        require_approval_for: vec![],
        model_allowlist: vec![],
    };
    let node = PolicySetIr {
        blocked_tools: vec![],
        require_approval_for: vec![],
        model_allowlist: vec![],
    };
    let ev = PolicyEvaluator;
    let ctx = EvaluationContext {
        node_id: "n1".into(),
        node_kind_tag: "tool".into(),
        tool_name: Some("delete_user".into()),
        model_ref: None,
    };
    // Full chain: global -> tenant -> workflow -> node.
    // Evaluator reverse: node (empty) -> workflow (empty) -> tenant (blocks delete_*) -> global.
    // Tenant blocks delete_user via glob.
    let decision = ev.evaluate(&ctx, &[&global, &tenant, &workflow, &node]);
    assert!(matches!(decision, PolicyDecision::Block { .. }));
}

#[test]
fn workflow_overrides_tenant_block_with_approval() {
    let tenant = PolicySetIr {
        blocked_tools: vec!["deploy_*".into()],
        require_approval_for: vec![],
        model_allowlist: vec![],
    };
    let workflow = PolicySetIr {
        blocked_tools: vec![],
        require_approval_for: vec!["deploy_*".into()],
        model_allowlist: vec![],
    };
    let ev = PolicyEvaluator;
    let ctx = EvaluationContext {
        node_id: "n1".into(),
        node_kind_tag: "tool".into(),
        tool_name: Some("deploy_staging".into()),
        model_ref: None,
    };
    // Workflow (more specific) has require_approval, checked before tenant block.
    let decision = ev.evaluate(&ctx, &[&tenant, &workflow]);
    assert!(matches!(decision, PolicyDecision::RequireApproval { .. }));
}

#[test]
fn tenant_model_allowlist_blocks_unallowed_model() {
    let tenant = PolicySetIr {
        blocked_tools: vec![],
        require_approval_for: vec![],
        model_allowlist: vec!["claude-*".into()],
    };
    let workflow = PolicySetIr {
        blocked_tools: vec![],
        require_approval_for: vec![],
        model_allowlist: vec![],
    };
    let ev = PolicyEvaluator;
    let ctx = EvaluationContext {
        node_id: "n1".into(),
        node_kind_tag: "model".into(),
        tool_name: None,
        model_ref: Some("gpt-4".into()),
    };
    // Workflow has no model_allowlist (empty), so evaluator falls through to tenant.
    // Tenant allowlist only allows claude-*, so gpt-4 is blocked.
    let decision = ev.evaluate(&ctx, &[&tenant, &workflow]);
    assert!(matches!(decision, PolicyDecision::Block { .. }));
}

#[test]
fn empty_tenant_policy_allows_everything() {
    let tenant = PolicySetIr {
        blocked_tools: vec![],
        require_approval_for: vec![],
        model_allowlist: vec![],
    };
    let workflow = PolicySetIr {
        blocked_tools: vec![],
        require_approval_for: vec![],
        model_allowlist: vec![],
    };
    let ev = PolicyEvaluator;
    let ctx = EvaluationContext {
        node_id: "n1".into(),
        node_kind_tag: "tool".into(),
        tool_name: Some("any_tool".into()),
        model_ref: None,
    };
    let decision = ev.evaluate(&ctx, &[&tenant, &workflow]);
    assert!(matches!(decision, PolicyDecision::Allow));
}
