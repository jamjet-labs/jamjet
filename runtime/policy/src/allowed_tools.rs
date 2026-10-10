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
//!   The code is allowed if a pattern matches the handler reference or that
//!   function's own name (the name a `@tool` gets by default), so
//!   `["search_web"]` allows a tool `search_web` whose function is
//!   `app.tools:search_web`, and refuses a call named `search_web` whose entry
//!   resolves to `bank.wires:send_wire`.
//! - A node with no tool name (a model, a condition) is not a tool use, and the
//!   list does not apply to it.

use crate::dispatch::{handler_function_name, PendingToolCall};
use crate::{glob_match, EvaluationContext, PYTHON_REF_TAG};

/// Whether `name` is matched by one of `patterns`, as written.
fn listed(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| glob_match(p, name))
}

/// Whether `ctx`'s tool may run under `allowed`. `Err` names the tool that is
/// not on the list. An empty list and a node with no tool name are `Ok`.
pub fn node_allowed(ctx: &EvaluationContext, allowed: &[String]) -> Result<(), String> {
    if allowed.is_empty() {
        return Ok(());
    }
    let Some(name) = ctx.tool_name.as_deref() else {
        return Ok(());
    };
    if listed(allowed, name) {
        return Ok(());
    }
    if let Some(canonical) = ctx.canonical_handler() {
        if allowed
            .iter()
            .any(|p| glob_match(&crate::canonical_pattern(p), &canonical))
        {
            return Ok(());
        }
    }
    Err(name.to_string())
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
    if node_allowed(&as_ref, allowed).is_ok() || listed(allowed, handler_function_name(handler)) {
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
    fn a_renamed_tool_is_allowed_by_naming_its_code_too() {
        let call = call("lookup", Some("app.tools:do_lookup"));
        assert!(call_allowed(&call, &list(&["lookup"])).is_err());
        assert!(call_allowed(&call, &list(&["lookup", "do_lookup"])).is_ok());
        assert!(call_allowed(&call, &list(&["lookup", "app.tools:*"])).is_ok());
    }
}
