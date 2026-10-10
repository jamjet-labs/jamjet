//! An agent card's `allowed_tools` (RFC-005 `AutonomyConstraints`): the tools
//! an agent may use, as glob patterns.
//!
//! This module only answers "is this tool on the list"; the enforcement point
//! is `jamjet_worker::node_guard::guard_item`, which every transport runs.
//!
//! An empty list is no restriction. The card cannot tell "not set" from "set
//! to nothing" (`allowed_tools` is a plain list that serializes as `[]`), every
//! card stored before this was enforced carries `[]` with the first meaning,
//! and RFC-005 marks the field optional. To refuse every tool, use a policy
//! `blocked_tools: ["*"]`.
//!
//! Matching is the allow-side mirror of `blocked_tools`, and stricter where
//! the two differ, because an allowlist must fail closed:
//!
//! - A node is matched on its tool name only: the tool for `tool`/`mcp_tool`,
//!   the skill for `a2a_task`, the agent for `agent`, and the handler
//!   reference for code-backed kinds (literal, or in canonical dotted form).
//!   NOT on its node id: `blocked_tools` treats a code-backed node's id as a
//!   second name, which only widens what a block catches, but an id is a label
//!   the author picks, so allowing on it would let a node named `search_web`
//!   run any code.
//! - An agent tool dispatch call has two identities, the name the model
//!   called and the code its `tools` entry resolves to. Both must be allowed.
//!   The code is allowed if a path pattern matches the handler reference or a
//!   bare name pattern matches that function's own name (the name a `@tool`
//!   gets by default; a bare name never reaches across a module path), so
//!   `["search_web"]` allows a tool `search_web` whose function is
//!   `app.tools:search_web`, and refuses a call named `search_web` whose entry
//!   resolves to `bank.wires:send_wire`.
//! - A node with no tool name (a model, a condition) is not a tool use, and the
//!   list does not apply to it.

use crate::dispatch::PendingToolCall;
use crate::{glob_match, EvaluationContext, FUNCTION_KIND_TAGS, PYTHON_REF_TAG};

/// Kinds that run something they choose at run time (a coordinator, agent
/// discovery, an `auto` agent tool). There is no name to check, so a
/// restricted agent cannot run them.
pub const RUN_TIME_TARGET_TAGS: &[&str] = &["coordinator", "agent_discovery", "agent_tool"];

/// Whether `name` is matched by one of `patterns`, as written.
fn listed(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| glob_match(p, name))
}

/// A pattern that names a path (`app.tools:*`, `bank.wires.send_wire`,
/// `Payments/transfer`) rather than a bare name.
fn is_path_pattern(pattern: &str) -> bool {
    pattern.contains([':', '.', '/'])
}

/// The function's own name at the end of a handler reference.
fn function_name(handler: &str) -> &str {
    handler.rsplit([':', '.', '/']).next().unwrap_or(handler)
}

/// Whether a handler reference (code-backed node or resolved call) is on the
/// list. A path pattern is matched against the reference as written and in
/// canonical dotted form; a bare name pattern only against the function's own
/// name, so `read_*` cannot reach across a module path to
/// `read_utils:delete_everything`.
fn handler_listed(ctx: &EvaluationContext, handler: &str, allowed: &[String]) -> bool {
    let canonical = ctx.canonical_handler();
    allowed.iter().any(|p| {
        if is_path_pattern(p) {
            glob_match(p, handler)
                || canonical
                    .as_deref()
                    .is_some_and(|c| glob_match(&crate::canonical_pattern(p), c))
        } else {
            glob_match(p, function_name(handler))
        }
    })
}

/// Whether `ctx`'s tool may run under `allowed`. `Err` names the tool that is
/// not on the list. An empty list and a node with no tool name are `Ok`,
/// except a node that picks its target at run time
/// ([`RUN_TIME_TARGET_TAGS`]), which is refused under a list.
pub fn node_allowed(ctx: &EvaluationContext, allowed: &[String]) -> Result<(), String> {
    if allowed.is_empty() {
        return Ok(());
    }
    let Some(name) = ctx.tool_name.as_deref() else {
        if RUN_TIME_TARGET_TAGS.contains(&ctx.node_kind_tag.as_str()) {
            return Err(format!("{} (target chosen at run time)", ctx.node_kind_tag));
        }
        return Ok(());
    };
    let code = FUNCTION_KIND_TAGS.contains(&ctx.node_kind_tag.as_str())
        || ctx.node_kind_tag == PYTHON_REF_TAG;
    let ok = if code {
        handler_listed(ctx, name, allowed)
    } else {
        listed(allowed, name)
    };
    if ok {
        Ok(())
    } else {
        Err(name.to_string())
    }
}

/// The name under which one of `patterns` names `ctx`'s node, for a card's
/// `blocked_tools` and `require_approval_for`. Matched like `allowed_tools`
/// (tool name; a code-backed node's handler, bare names against the
/// function's own name). A node that picks its target at run time is
/// treated as named by any non-empty list: nothing shows it is not.
pub fn node_named_by(ctx: &EvaluationContext, patterns: &[String]) -> Option<String> {
    if patterns.is_empty() {
        return None;
    }
    let Some(name) = ctx.tool_name.as_deref() else {
        return RUN_TIME_TARGET_TAGS
            .contains(&ctx.node_kind_tag.as_str())
            .then(|| format!("{} (target chosen at run time)", ctx.node_kind_tag));
    };
    let code = FUNCTION_KIND_TAGS.contains(&ctx.node_kind_tag.as_str())
        || ctx.node_kind_tag == PYTHON_REF_TAG;
    let hit = if code {
        handler_listed(ctx, name, patterns)
    } else {
        listed(patterns, name)
    };
    hit.then(|| name.to_string())
}

/// The identity of `call` one of `patterns` names: its name, or the handler
/// its `tools` entry resolves to.
pub fn call_named_by(call: &PendingToolCall, patterns: &[String]) -> Option<String> {
    if patterns.is_empty() {
        return None;
    }
    if listed(patterns, &call.name) {
        return Some(call.name.clone());
    }
    let handler = call.handler.as_ref()?;
    let as_ref = EvaluationContext {
        node_id: String::new(),
        node_kind_tag: PYTHON_REF_TAG.to_string(),
        tool_name: Some(handler.clone()),
        model_ref: None,
    };
    handler_listed(&as_ref, handler, patterns).then(|| handler.clone())
}

/// Why a dispatch call is not on the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallRefused {
    /// The name the model called is not on the list.
    Name(String),
    /// The name is on the list, but the code its `tools` entry resolves to
    /// is not.
    Handler { name: String, handler: String },
}

/// Whether one agent tool dispatch call may run under `allowed`: its name and,
/// when its `tools` entry names one, its handler must both be on the list.
pub fn call_allowed(call: &PendingToolCall, allowed: &[String]) -> Result<(), CallRefused> {
    if allowed.is_empty() {
        return Ok(());
    }
    if !listed(allowed, &call.name) {
        return Err(CallRefused::Name(call.name.clone()));
    }
    let Some(handler) = &call.handler else {
        return Ok(());
    };
    let as_ref = EvaluationContext {
        node_id: String::new(),
        node_kind_tag: PYTHON_REF_TAG.to_string(),
        tool_name: Some(handler.clone()),
        model_ref: None,
    };
    if handler_listed(&as_ref, handler, allowed) {
        return Ok(());
    }
    Err(CallRefused::Handler {
        name: call.name.clone(),
        handler: handler.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn list(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    fn ctx(tag: &str, node_id: &str, tool: Option<&str>) -> EvaluationContext {
        EvaluationContext {
            node_id: node_id.into(),
            node_kind_tag: tag.into(),
            tool_name: tool.map(str::to_string),
            model_ref: None,
        }
    }

    fn call(name: &str, handler: Option<&str>) -> PendingToolCall {
        PendingToolCall {
            name: name.into(),
            arguments: json!({}),
            handler: handler.map(str::to_string),
        }
    }

    #[test]
    fn an_empty_list_restricts_nothing() {
        assert!(node_allowed(&ctx("tool", "n", Some("anything")), &[]).is_ok());
        assert!(call_allowed(&call("anything", Some("x:y")), &[]).is_ok());
    }

    #[test]
    fn a_listed_tool_runs_and_an_unlisted_one_does_not() {
        let allowed = list(&["search_*", "read_document"]);
        assert!(node_allowed(&ctx("mcp_tool", "n", Some("search_web")), &allowed).is_ok());
        assert!(node_allowed(&ctx("tool", "n", Some("read_document")), &allowed).is_ok());
        assert_eq!(
            node_allowed(&ctx("mcp_tool", "n", Some("send_wire")), &allowed),
            Err("send_wire".to_string())
        );
    }

    #[test]
    fn a_node_without_a_tool_name_is_not_a_tool_use() {
        assert!(node_allowed(&ctx("model", "n", None), &list(&["search"])).is_ok());
    }

    #[test]
    fn a_function_node_is_allowed_by_its_handler_not_its_node_id() {
        let allowed = list(&["search_web"]);
        // The node id is the author's label; it does not allow the code.
        assert!(node_allowed(
            &ctx("python_fn", "search_web", Some("bank.wires:send_wire")),
            &allowed
        )
        .is_err());
        // Either spelling of the handler allows it.
        for pattern in ["bank.wires:send_wire", "bank.wires.send_wire", "bank.*"] {
            assert!(
                node_allowed(
                    &ctx("python_fn", "n", Some("bank.wires:send_wire")),
                    &list(&[pattern])
                )
                .is_ok(),
                "{pattern}"
            );
        }
    }

    #[test]
    fn a_call_needs_its_name_and_its_code_allowed() {
        let allowed = list(&["search_web"]);
        // The compiler's map: the tool's own function.
        assert!(call_allowed(&call("search_web", Some("app.tools:search_web")), &allowed).is_ok());
        // No map entry: the name is the identity.
        assert!(call_allowed(&call("search_web", None), &allowed).is_ok());
        // An allowed name resolving to other code is refused.
        assert_eq!(
            call_allowed(&call("search_web", Some("bank.wires:send_wire")), &allowed),
            Err(CallRefused::Handler {
                name: "search_web".into(),
                handler: "bank.wires:send_wire".into()
            })
        );
        // An unlisted name is refused even when its code is listed.
        assert_eq!(
            call_allowed(
                &call("send_wire", Some("app.tools:search_web")),
                &list(&["app.tools:search_web"])
            ),
            Err(CallRefused::Name("send_wire".into()))
        );
    }

    #[test]
    fn a_name_pattern_matches_a_function_name_not_a_path() {
        let allowed = list(&["read_*"]);
        assert!(call_allowed(
            &call("read_doc", Some("read_utils:delete_everything")),
            &allowed
        )
        .is_err());
        assert!(call_allowed(&call("read_doc", Some("app.tools:read_doc")), &allowed).is_ok());
        for kind in [
            ("python_fn", "read_utils:delete_everything"),
            ("java_fn", "read_utils.Admin/deleteAll"),
            ("external_fn", "read_q/delete_everything"),
        ] {
            assert!(
                node_allowed(&ctx(kind.0, "n", Some(kind.1)), &allowed).is_err(),
                "{kind:?}"
            );
        }
        assert!(node_allowed(&ctx("python_fn", "n", Some("app.tools:read_doc")), &allowed).is_ok());
        // A path pattern still names the path.
        assert!(node_allowed(
            &ctx("python_fn", "n", Some("read_utils:delete_everything")),
            &list(&["read_utils:*"])
        )
        .is_ok());
    }

    #[test]
    fn a_node_that_picks_its_target_at_run_time_is_refused() {
        for tag in ["coordinator", "agent_discovery", "agent_tool"] {
            assert!(
                node_allowed(&ctx(tag, "n", None), &list(&["*"])).is_err(),
                "{tag}"
            );
            assert!(node_allowed(&ctx(tag, "n", None), &[]).is_ok(), "{tag}");
        }
    }

    #[test]
    fn a_block_or_gate_list_names_a_call_by_its_name_or_its_code() {
        let p = list(&["send_wire"]);
        assert_eq!(
            call_named_by(&call("send_wire", None), &p),
            Some("send_wire".into())
        );
        // Another name for the same code.
        assert_eq!(
            call_named_by(&call("pay", Some("bank.wires:send_wire")), &p),
            Some("bank.wires:send_wire".into())
        );
        assert_eq!(call_named_by(&call("search", Some("app:search")), &p), None);
        assert_eq!(call_named_by(&call("send_wire", None), &[]), None);
        // A bare name does not reach across a module path.
        assert_eq!(
            call_named_by(
                &call("x", Some("send_wire_utils:noop")),
                &list(&["send_wire*"])
            ),
            None
        );
    }

    #[test]
    fn a_block_or_gate_list_names_a_node() {
        let p = list(&["send_*"]);
        assert_eq!(
            node_named_by(&ctx("mcp_tool", "n", Some("send_wire")), &p),
            Some("send_wire".into())
        );
        assert_eq!(
            node_named_by(&ctx("python_fn", "n", Some("bank.wires:send_wire")), &p),
            Some("bank.wires:send_wire".into())
        );
        assert_eq!(
            node_named_by(&ctx("python_fn", "send_x", Some("app:noop")), &p),
            None
        );
        assert_eq!(node_named_by(&ctx("model", "n", None), &p), None);
        assert!(node_named_by(&ctx("coordinator", "n", None), &p).is_some());
        assert!(node_named_by(&ctx("coordinator", "n", None), &[]).is_none());
    }

    #[test]
    fn a_renamed_tool_is_allowed_by_naming_its_code_too() {
        let call = call("lookup", Some("app.tools:do_lookup"));
        assert!(call_allowed(&call, &list(&["lookup"])).is_err());
        assert!(call_allowed(&call, &list(&["lookup", "do_lookup"])).is_ok());
        assert!(call_allowed(&call, &list(&["lookup", "app.tools:*"])).is_ok());
    }
}
