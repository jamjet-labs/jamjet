//! JamJet Policy Engine (Phase 4 — Tier 1)
//!
//! Evaluates policy rules at node execution time:
//! - Block disallowed tools
//! - Require approval for sensitive operations
//! - Enforce model allowlists
//!
//! Also provides autonomy enforcement:
//! - `guided` / `bounded_autonomous` / `fully_autonomous` levels
//! - Circuit breaker for agents in error loops

pub mod autonomy;
pub mod engine;

use jamjet_ir::workflow::PolicySetIr;
use serde::{Deserialize, Serialize};

// ── PolicyDecision ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PolicyDecision {
    Allow,
    Block { reason: String },
    RequireApproval { approver: String },
}

// ── EvaluationContext ──────────────────────────────────────────────────────

/// Describes the node being evaluated so the policy engine can apply rules.
#[derive(Debug, Clone)]
pub struct EvaluationContext {
    pub node_id: String,
    /// The serde tag of the `NodeKind` (e.g. `"tool"`, `"model"`, `"mcp_tool"`).
    pub node_kind_tag: String,
    /// Name of the tool being invoked, if any.
    pub tool_name: Option<String>,
    /// Model reference string, if this is a model node.
    pub model_ref: Option<String>,
}

// ── PolicyEvaluator ────────────────────────────────────────────────────────

/// Merges policy sets from most-general (global) to most-specific (node) and
/// returns the first matching decision. Returns `Allow` if no rule matches.
pub struct PolicyEvaluator;

impl PolicyEvaluator {
    /// Evaluate policy for a given node context.
    ///
    /// `policy_sets` is ordered from least-specific (global/workflow) to
    /// most-specific (node). The first matching rule in the most-specific set
    /// wins; we iterate in reverse so node-level rules take priority.
    pub fn evaluate(
        &self,
        ctx: &EvaluationContext,
        policy_sets: &[&PolicySetIr],
    ) -> PolicyDecision {
        // Iterate most-specific first (node → workflow → global).
        for policy in policy_sets.iter().rev() {
            // 1. Model allowlist check (for model nodes).
            if ctx.node_kind_tag == "model" && !policy.model_allowlist.is_empty() {
                let allowed = policy.model_allowlist.iter().any(|pattern| {
                    ctx.model_ref
                        .as_deref()
                        .map(|m| glob_match(pattern, m))
                        .unwrap_or(false)
                });
                if !allowed {
                    return PolicyDecision::Block {
                        reason: format!(
                            "model '{}' is not in the allowlist",
                            ctx.model_ref.as_deref().unwrap_or("unknown")
                        ),
                    };
                }
            }

            // 2. Blocked tools check.
            if let Some(tool_name) = &ctx.tool_name {
                for pattern in &policy.blocked_tools {
                    if glob_match(pattern, tool_name) {
                        return PolicyDecision::Block {
                            reason: format!(
                                "tool '{tool_name}' matches blocked pattern '{pattern}'"
                            ),
                        };
                    }
                }

                // 3. Require-approval check.
                for pattern in &policy.require_approval_for {
                    if glob_match(pattern, tool_name) {
                        return PolicyDecision::RequireApproval {
                            approver: "human".to_string(),
                        };
                    }
                }
            }
        }

        PolicyDecision::Allow
    }
}

// ── Glob matching ──────────────────────────────────────────────────────────

/// Minimal glob matcher supporting `*` (any chars) and `?` (single char).
/// Operates on ASCII byte sequences — sufficient for tool names and model refs.
fn glob_match(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    glob_match_inner(&p, &v)
}

fn glob_match_inner(p: &[char], v: &[char]) -> bool {
    match (p.first(), v.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some('*'), _) => {
            // Star matches empty or one-or-more chars.
            glob_match_inner(&p[1..], v) || (!v.is_empty() && glob_match_inner(p, &v[1..]))
        }
        (Some('?'), Some(_)) => glob_match_inner(&p[1..], &v[1..]),
        (Some(pc), Some(vc)) if pc == vc => glob_match_inner(&p[1..], &v[1..]),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(blocked: &[&str], require_approval: &[&str], allowlist: &[&str]) -> PolicySetIr {
        PolicySetIr {
            blocked_tools: blocked.iter().map(|s| s.to_string()).collect(),
            require_approval_for: require_approval.iter().map(|s| s.to_string()).collect(),
            model_allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn allow_when_no_rules() {
        let ev = PolicyEvaluator;
        let ctx = EvaluationContext {
            node_id: "n1".into(),
            node_kind_tag: "tool".into(),
            tool_name: Some("search".into()),
            model_ref: None,
        };
        let sets: &[&PolicySetIr] = &[];
        assert!(matches!(ev.evaluate(&ctx, sets), PolicyDecision::Allow));
    }

    #[test]
    fn block_tool_by_exact_name() {
        let ev = PolicyEvaluator;
        let p = policy(&["delete_user"], &[], &[]);
        let ctx = EvaluationContext {
            node_id: "n1".into(),
            node_kind_tag: "tool".into(),
            tool_name: Some("delete_user".into()),
            model_ref: None,
        };
        assert!(matches!(
            ev.evaluate(&ctx, &[&p]),
            PolicyDecision::Block { .. }
        ));
    }

    #[test]
    fn block_tool_by_glob() {
        let ev = PolicyEvaluator;
        let p = policy(&["payments.*"], &[], &[]);
        let ctx = EvaluationContext {
            node_id: "n1".into(),
            node_kind_tag: "tool".into(),
            tool_name: Some("payments.transfer".into()),
            model_ref: None,
        };
        assert!(matches!(
            ev.evaluate(&ctx, &[&p]),
            PolicyDecision::Block { .. }
        ));
    }

    #[test]
    fn require_approval_pattern() {
        let ev = PolicyEvaluator;
        let p = policy(&[], &["delete_*"], &[]);
        let ctx = EvaluationContext {
            node_id: "n1".into(),
            node_kind_tag: "tool".into(),
            tool_name: Some("delete_record".into()),
            model_ref: None,
        };
        assert!(matches!(
            ev.evaluate(&ctx, &[&p]),
            PolicyDecision::RequireApproval { .. }
        ));
    }

    #[test]
    fn model_allowlist_blocks_unknown_model() {
        let ev = PolicyEvaluator;
        let p = policy(&[], &[], &["claude-*"]);
        let ctx = EvaluationContext {
            node_id: "n1".into(),
            node_kind_tag: "model".into(),
            tool_name: None,
            model_ref: Some("gpt-4".into()),
        };
        assert!(matches!(
            ev.evaluate(&ctx, &[&p]),
            PolicyDecision::Block { .. }
        ));
    }

    #[test]
    fn model_allowlist_allows_matching_model() {
        let ev = PolicyEvaluator;
        let p = policy(&[], &[], &["claude-*"]);
        let ctx = EvaluationContext {
            node_id: "n1".into(),
            node_kind_tag: "model".into(),
            tool_name: None,
            model_ref: Some("claude-sonnet-4-6".into()),
        };
        assert!(matches!(ev.evaluate(&ctx, &[&p]), PolicyDecision::Allow));
    }

    #[test]
    fn node_level_policy_overrides_workflow_level() {
        let ev = PolicyEvaluator;
        // workflow: block all deletions
        let workflow_policy = policy(&["delete_*"], &[], &[]);
        // node: require approval for delete_user (node-level is more specific)
        let node_policy = policy(&[], &["delete_user"], &[]);
        let ctx = EvaluationContext {
            node_id: "n1".into(),
            node_kind_tag: "tool".into(),
            tool_name: Some("delete_user".into()),
            model_ref: None,
        };
        // Node-level policy is checked first (most-specific wins in rev iteration).
        // Node has require_approval_for: ["delete_user"] — this fires first.
        // Result: RequireApproval (node-level overrides workflow-level block).
        let decision = ev.evaluate(&ctx, &[&workflow_policy, &node_policy]);
        assert!(matches!(decision, PolicyDecision::RequireApproval { .. }));
    }

    #[test]
    fn workflow_level_block_applies_when_no_node_policy() {
        let ev = PolicyEvaluator;
        let workflow_policy = policy(&["delete_*"], &[], &[]);
        let ctx = EvaluationContext {
            node_id: "n1".into(),
            node_kind_tag: "tool".into(),
            tool_name: Some("delete_record".into()),
            model_ref: None,
        };
        // No node-level policy — workflow-level block applies.
        let decision = ev.evaluate(&ctx, &[&workflow_policy]);
        assert!(matches!(decision, PolicyDecision::Block { .. }));
    }

    #[test]
    fn glob_star() {
        assert!(glob_match("foo.*", "foo.bar"));
        assert!(glob_match("foo.*", "foo."));
        assert!(!glob_match("foo.*", "foobar"));
    }

    #[test]
    fn glob_question_mark() {
        assert!(glob_match("foo?", "foob"));
        assert!(!glob_match("foo?", "foo"));
        assert!(!glob_match("foo?", "fooba"));
    }
}
