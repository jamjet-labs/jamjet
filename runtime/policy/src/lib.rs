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

pub mod allowed_tools;
pub mod autonomy;
pub mod dispatch;
pub mod engine;
pub mod redaction;

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

/// Node kinds whose code is named by a handler reference, not a tool. For these
/// the node id is a policy name too.
pub const FUNCTION_KIND_TAGS: &[&str] = &["python_fn", "java_fn", "external_fn"];

/// The evaluation tag for a Python `module:function` reference that is not a
/// node: the handler an agent tool call resolves to through the run's `tools`
/// map. Matched like a `python_fn` handler (literal and canonical dotted form),
/// but with no node id as a second name.
pub const PYTHON_REF_TAG: &str = "python_ref";

impl EvaluationContext {
    /// Every name `blocked_tools` and `require_approval_for` patterns are
    /// matched against: the tool name, and for code-backed kinds
    /// ([`FUNCTION_KIND_TAGS`]) also the node id. A pattern applies if it
    /// matches any of them. Other kinds are not matched on their node id, so
    /// existing tool policies keep their meaning.
    pub fn policy_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.tool_name.iter().map(String::as_str).collect();
        if FUNCTION_KIND_TAGS.contains(&self.node_kind_tag.as_str()) {
            names.push(self.node_id.as_str());
        }
        names
    }

    /// The handler reference of a code-backed node as a canonical dotted
    /// path, or `None` for any other kind. The Python worker resolves
    /// `function` as an attribute chain, so `payments.transfer:send` and
    /// `payments:transfer.send` run the same code; both canonicalise to
    /// `payments.transfer.send`. A Java `class/method` becomes `class.method`.
    /// An external handler is opaque to the engine and has no canonical form.
    ///
    /// This closes re-spellings of one dotted path, not aliases: a module that
    /// re-exports the function (`myapp.api:send`) names the same code under a
    /// different path, and no name pattern can see that.
    pub fn canonical_handler(&self) -> Option<String> {
        let name = self.tool_name.as_deref()?;
        match self.node_kind_tag.as_str() {
            "python_fn" | PYTHON_REF_TAG => Some(name.replace(':', ".")),
            "java_fn" => Some(name.replace('/', ".")),
            _ => None,
        }
    }

    /// Whether `pattern` names this node: it matches one of
    /// [`Self::policy_names`] as written, or, for a code-backed node, its
    /// canonical form matches [`Self::canonical_handler`].
    pub fn matches(&self, pattern: &str) -> Option<String> {
        // `tool:*` is every tool, in every layer: the tools-only catch-all.
        let pattern = if is_tools_catch_all(pattern) {
            "*"
        } else {
            pattern
        };
        if let Some(name) = self
            .policy_names()
            .into_iter()
            .find(|n| glob_match(pattern, n))
        {
            return Some(name.to_string());
        }
        let canonical = self.canonical_handler()?;
        glob_match(&canonical_pattern(pattern), &canonical).then_some(canonical)
    }

    /// [`Self::matches`] for a tenant pattern. A tenant catch-all (a pattern
    /// of only `*`, see [`is_catch_all`]) also names a model call, which has
    /// no tool name of its own: the operator who writes `["*"]` means every
    /// action. Inside the IR `["*"]` keeps its meaning of "every tool"
    /// (`approval_required=True` compiles to it), so model calls are not
    /// matched there.
    fn matches_tenant(&self, pattern: &str) -> Option<String> {
        if let Some(name) = self.matches(pattern) {
            return Some(name);
        }
        (self.node_kind_tag == "model" && is_catch_all(pattern)).then(|| {
            self.model_ref
                .clone()
                .unwrap_or_else(|| self.node_id.clone())
        })
    }

    /// Whether the model this context calls is on `allowlist`. An empty list
    /// allows every model, and a context that calls no model is not checked.
    fn model_allowed(&self, allowlist: &[String]) -> bool {
        if self.node_kind_tag != "model" || allowlist.is_empty() {
            return true;
        }
        allowlist.iter().any(|pattern| {
            self.model_ref
                .as_deref()
                .map(|m| model_matches(pattern, m))
                .unwrap_or(false)
        })
    }

    fn model_block(&self) -> PolicyDecision {
        PolicyDecision::Block {
            reason: format!(
                "model '{}' is not in the allowlist",
                self.model_ref.as_deref().unwrap_or("unknown")
            ),
        }
    }
}

/// A pattern that matches every name: one or more `*` and nothing else. In a
/// tenant policy it also matches every `model` node.
pub fn is_catch_all(pattern: &str) -> bool {
    !pattern.is_empty() && pattern.chars().all(|c| c == '*')
}

/// The prefix of the tools-only catch-all, [`is_tools_catch_all`].
pub const TOOLS_ONLY_PREFIX: &str = "tool:";

/// `tool:*`: every tool and never a `model` node, in the tenant layer as in
/// the IR. A tenant `*` also covers model calls (a ceiling fails safe); an
/// operator who means "every tool, but not each model turn" writes this.
pub fn is_tools_catch_all(pattern: &str) -> bool {
    pattern
        .strip_prefix(TOOLS_ONLY_PREFIX)
        .is_some_and(is_catch_all)
}

/// A handler pattern in canonical dotted form: `:` and `/` become `.`, so a
/// pattern in either spelling matches [`EvaluationContext::canonical_handler`].
pub(crate) fn canonical_pattern(pattern: &str) -> String {
    pattern.replace([':', '/'], ".")
}

// ── PolicyEvaluator ────────────────────────────────────────────────────────

/// Evaluates a node against its policy layers.
///
/// Two kinds of layer, with different precedence:
///
/// - The IR's own layers (workflow, then node), in [`Self::evaluate`]. The
///   author of the IR owns both, so the most specific rule that matches
///   decides: a node `require_approval_for` turns a workflow block into an
///   approval request, and a node `model_allowlist` replaces the workflow's.
/// - The tenant layer, in [`Self::evaluate_chain`]. The operator owns it and
///   it is a ceiling over the IR: deny overrides. A tenant block stands
///   whatever the IR says, a tenant approval gate holds a node the IR allows,
///   and a model must be on the tenant's allowlist as well as the IR's.
pub struct PolicyEvaluator;

impl PolicyEvaluator {
    /// Evaluate `ctx` under a tenant policy and the IR's layers. This is the
    /// decision the engine enforces.
    ///
    /// Deny overrides across the two: a block from either side blocks (the
    /// tenant's is reported first), otherwise an approval gate from either
    /// side holds the node, otherwise it is allowed. So no rule an IR author
    /// writes can lift a tenant rule; it can only add to it.
    pub fn evaluate_chain(
        &self,
        ctx: &EvaluationContext,
        tenant: Option<&PolicySetIr>,
        layers: &[&PolicySetIr],
    ) -> PolicyDecision {
        let ceiling = match tenant {
            Some(t) => self.evaluate_tenant(ctx, t),
            None => PolicyDecision::Allow,
        };
        if matches!(ceiling, PolicyDecision::Block { .. }) {
            return ceiling;
        }
        match (ceiling, self.evaluate_ir(ctx, layers)) {
            (_, block @ PolicyDecision::Block { .. }) => block,
            (gate @ PolicyDecision::RequireApproval { .. }, _) => gate,
            (_, gate @ PolicyDecision::RequireApproval { .. }) => gate,
            _ => PolicyDecision::Allow,
        }
    }

    /// The tenant layer on its own: its model allowlist, then its blocked
    /// tools, then its approval gates. Unlike an IR layer, a model that is on
    /// the allowlist does not skip the rest, so a tenant `blocked_tools` or
    /// `require_approval_for` catch-all still applies to the call.
    pub fn evaluate_tenant(&self, ctx: &EvaluationContext, tenant: &PolicySetIr) -> PolicyDecision {
        if !ctx.model_allowed(&tenant.model_allowlist) {
            return ctx.model_block();
        }
        for pattern in &tenant.blocked_tools {
            if let Some(name) = ctx.matches_tenant(pattern) {
                return PolicyDecision::Block {
                    reason: format!("tool '{name}' matches blocked pattern '{pattern}'"),
                };
            }
        }
        if tenant
            .require_approval_for
            .iter()
            .any(|pattern| ctx.matches_tenant(pattern).is_some())
        {
            return PolicyDecision::RequireApproval {
                approver: "human".to_string(),
            };
        }
        PolicyDecision::Allow
    }

    /// The IR's layers only: [`Self::evaluate_ir`]. Deprecated because the
    /// name invites passing the tenant policy as one more layer, where an IR
    /// rule can override it; the engine decides with [`Self::evaluate_chain`].
    #[deprecated(
        since = "0.6.0",
        note = "use evaluate_chain (tenant ceiling and IR layers) or evaluate_ir (IR layers only)"
    )]
    pub fn evaluate(
        &self,
        ctx: &EvaluationContext,
        policy_sets: &[&PolicySetIr],
    ) -> PolicyDecision {
        self.evaluate_ir(ctx, policy_sets)
    }

    /// Evaluate `ctx` against the IR's own layers.
    ///
    /// `policy_sets` is ordered from least-specific (workflow) to
    /// most-specific (node). The first matching rule in the most-specific set
    /// wins; we iterate in reverse so node-level rules take priority.
    ///
    /// A tenant policy must not be passed here: in this list it would be one
    /// more layer an IR rule can override. Use [`Self::evaluate_chain`].
    pub fn evaluate_ir(
        &self,
        ctx: &EvaluationContext,
        policy_sets: &[&PolicySetIr],
    ) -> PolicyDecision {
        // Iterate most-specific first (node → workflow).
        for policy in policy_sets.iter().rev() {
            // 1. Model allowlist check (for model nodes).
            if ctx.node_kind_tag == "model" && !policy.model_allowlist.is_empty() {
                if !ctx.model_allowed(&policy.model_allowlist) {
                    return ctx.model_block();
                }
                // Model matched this layer's allowlist — stop checking less-specific layers.
                return PolicyDecision::Allow;
            }

            // 2. Blocked tools check, then 3. require-approval, over every
            // name the node answers to, including a code-backed node's
            // canonical handler path.
            for pattern in &policy.blocked_tools {
                if let Some(name) = ctx.matches(pattern) {
                    return PolicyDecision::Block {
                        reason: format!("tool '{name}' matches blocked pattern '{pattern}'"),
                    };
                }
            }
            for pattern in &policy.require_approval_for {
                if ctx.matches(pattern).is_some() {
                    return PolicyDecision::RequireApproval {
                        approver: "human".to_string(),
                    };
                }
            }
        }

        PolicyDecision::Allow
    }
}

/// Does `pattern` allow the model reference `model`?
///
/// Model refs are provider-routed strings — `anthropic/claude-opus-4-8`. The
/// Python seam (`ModelAllowlistMiddleware`) allows an entry that matches EITHER
/// the provider OR the full string, and the built-in `"strict"` policy relies on
/// that: its allowlist is the single entry `"anthropic"`.
///
/// Glob-matching the full ref alone cannot express that. `"anthropic"` carries no
/// wildcard, so it compares exactly, and `"anthropic" != "anthropic/claude-…"`.
/// The result was that `policy="strict"` — the one built-in policy whose whole
/// job is "Anthropic only" — blocked EVERY model on the durable path while
/// working fine in-process. Same policy, opposite behaviour per transport.
///
/// So a pattern with no `/` is also tried against the provider segment. This is
/// purely additive: it can only turn a Block into an Allow, and only for patterns
/// the Python seam already allows, so no call that used to be permitted becomes
/// denied. Patterns that name a full ref or carry globs are unaffected.
fn model_matches(pattern: &str, model: &str) -> bool {
    if glob_match(pattern, model) {
        return true;
    }
    if pattern.contains('/') {
        return false;
    }
    let provider = model.split_once('/').map(|(p, _)| p).unwrap_or(model);
    glob_match(pattern, provider)
}

// ── Glob matching ──────────────────────────────────────────────────────────

/// Minimal glob matcher supporting `*` (any chars) and `?` (single char).
/// Operates on ASCII byte sequences — sufficient for tool names and model refs.
/// [`glob_match`] for callers outside this crate: `*` any run of characters,
/// `?` one character, everything else literal.
pub fn glob_matches(pattern: &str, value: &str) -> bool {
    glob_match(pattern, value)
}

pub(crate) fn glob_match(pattern: &str, value: &str) -> bool {
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
            ..Default::default()
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
        assert!(matches!(ev.evaluate_ir(&ctx, sets), PolicyDecision::Allow));
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
            ev.evaluate_ir(&ctx, &[&p]),
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
            ev.evaluate_ir(&ctx, &[&p]),
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
            ev.evaluate_ir(&ctx, &[&p]),
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
            ev.evaluate_ir(&ctx, &[&p]),
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
        assert!(matches!(ev.evaluate_ir(&ctx, &[&p]), PolicyDecision::Allow));
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
        let decision = ev.evaluate_ir(&ctx, &[&workflow_policy, &node_policy]);
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
        let decision = ev.evaluate_ir(&ctx, &[&workflow_policy]);
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

    // ── Model allowlist: Python/Rust parity ──────────────────────────────────

    fn model_ctx(model: &str) -> EvaluationContext {
        EvaluationContext {
            node_id: "n1".into(),
            node_kind_tag: "model".into(),
            tool_name: None,
            model_ref: Some(model.into()),
        }
    }

    /// The exact allowlist the built-in `"strict"` policy emits, against the
    /// exact model the ADK docs and site use.
    ///
    /// This blocked on the durable path while passing in-process: `"anthropic"`
    /// has no wildcard, so glob-matching it against the FULL ref compared it
    /// literally to `"anthropic/claude-opus-4-8"`. The one built-in policy whose
    /// job is "Anthropic only" denied every Anthropic model.
    #[test]
    fn strict_allows_anthropic_models_on_the_durable_path() {
        let p = policy(&[], &[], &["anthropic"]);
        assert!(
            matches!(
                PolicyEvaluator.evaluate_ir(&model_ctx("anthropic/claude-opus-4-8"), &[&p]),
                PolicyDecision::Allow
            ),
            "policy=\"strict\" must allow Anthropic models, or it means the \
             opposite of what it says on the durable path"
        );
    }

    /// A provider entry must still DENY another provider — the additive rule
    /// must not turn the allowlist into an allow-all.
    #[test]
    fn a_provider_entry_still_denies_other_providers() {
        let p = policy(&[], &[], &["anthropic"]);
        assert!(matches!(
            PolicyEvaluator.evaluate_ir(&model_ctx("openai/gpt-4o"), &[&p]),
            PolicyDecision::Block { .. }
        ));
    }

    /// A pattern naming a full ref keeps full-ref semantics: it must not be
    /// reinterpreted as a provider and widen to that provider's whole catalogue.
    #[test]
    fn a_full_ref_pattern_does_not_widen_to_the_provider() {
        let p = policy(&[], &[], &["anthropic/claude-opus-4-8"]);
        assert!(matches!(
            PolicyEvaluator.evaluate_ir(&model_ctx("anthropic/claude-opus-4-8"), &[&p]),
            PolicyDecision::Allow
        ));
        assert!(
            matches!(
                PolicyEvaluator.evaluate_ir(&model_ctx("anthropic/claude-haiku-4-5"), &[&p]),
                PolicyDecision::Block { .. }
            ),
            "pinning one model must not silently allow the provider's whole catalogue"
        );
    }

    /// Explicit provider globs keep working unchanged.
    #[test]
    fn provider_globs_still_match_the_full_ref() {
        let p = policy(&[], &[], &["anthropic/*"]);
        assert!(matches!(
            PolicyEvaluator.evaluate_ir(&model_ctx("anthropic/claude-opus-4-8"), &[&p]),
            PolicyDecision::Allow
        ));
        assert!(matches!(
            PolicyEvaluator.evaluate_ir(&model_ctx("openai/gpt-4o"), &[&p]),
            PolicyDecision::Block { .. }
        ));
    }

    /// A bare ref with no provider segment still matches exactly.
    #[test]
    fn a_ref_without_a_provider_segment_matches_exactly() {
        let p = policy(&[], &[], &["gpt-4o"]);
        assert!(matches!(
            PolicyEvaluator.evaluate_ir(&model_ctx("gpt-4o"), &[&p]),
            PolicyDecision::Allow
        ));
    }

    /// The mirror of `PARITY_TABLE` in
    /// `sdk/python/tests/middleware/test_model_allowlist_parity.py`.
    ///
    /// The SAME allowlist is evaluated by the Python seam for an in-process run
    /// and here for a durable one. A row that changes on one side and not the
    /// other means one policy now behaves two ways depending on transport, which
    /// is the class of bug this table exists to catch. Keep the two in step.
    #[test]
    fn model_allowlist_parity_table() {
        const OPUS: &str = "anthropic/claude-opus-4-8";
        let cases: &[(&str, &str, bool)] = &[
            ("anthropic", OPUS, true),
            ("openai", OPUS, false),
            ("anthropic/*", OPUS, true),
            ("openai/*", OPUS, false),
            ("anthropic/claude-opus-4-8", OPUS, true),
            ("anthropic/claude-haiku-4-5", OPUS, false),
            ("*", OPUS, true),
            ("anthropi?", OPUS, true),
            ("anthropi??", OPUS, false),
        ];
        for (pattern, model, expected) in cases {
            assert_eq!(
                model_matches(pattern, model),
                *expected,
                "{pattern:?} vs {model:?} must be {expected}"
            );
        }
    }

    /// `[seq]` is a literal here, not a character class. Python's matcher is
    /// hand-written rather than `fnmatch` for exactly this reason.
    #[test]
    fn a_character_class_is_literal_not_a_class() {
        assert!(!model_matches(
            "anthropic/[abc]laude-opus-4-8",
            "anthropic/claude-opus-4-8"
        ));
    }
}
