//! An agent's `allowed_tools` is enforced on every node attributed to it.
//!
//! `AutonomyConstraints::allowed_tools` was declared on the Agent Card and
//! checked nowhere, while docs/guides/security.md said it was enforced at the
//! runtime. These tests drive both transports (the HTTP claim route and the
//! in-process worker) on both backends (memory and SQLite, each with its own
//! agent registry), through the shared guard.

use async_trait::async_trait;
use jamjet_agents::{AgentRegistry, InMemoryAgentRegistry, SqliteAgentRegistry};
use jamjet_api::state::AppState;
use jamjet_core::workflow::ExecutionId;
use jamjet_state::backend::WorkItem;
use jamjet_state::event::EventKind;
use jamjet_worker::executor::{ExecutionResult, ExecutorError, NodeExecutor};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

mod support;

const AGENT: &str = "jamjet://acme/researcher";

fn card(uri: &str, allowed_tools: &[&str]) -> Value {
    json!({
        "id": uri,
        "uri": uri,
        "name": "researcher",
        "description": "research agent",
        "version": "1.0.0",
        "capabilities": { "skills": [], "protocols": [], "tools_provided": [], "tools_consumed": [] },
        "autonomy": "bounded_autonomous",
        // Only the constraint under test: every other field is optional.
        "constraints": { "allowed_tools": allowed_tools },
        "auth": { "type": "none" }
    })
}

fn mcp_tool(tool: &str) -> Value {
    json!({ "type": "mcp_tool", "server": "srv", "tool": tool, "input_mapping": {}, "output_schema": "" })
}

fn adk_dispatch() -> Value {
    json!({
        "type": "python_fn",
        "module": "jamjet.agents.tool_runtime",
        "function": "dispatch_tool_calls",
        "output_schema": "",
        "agent_tool_dispatch": true
    })
}

/// A one-node workflow. `workflow_agent` / `node_agent` attribute it.
fn ir(
    workflow_id: &str,
    kind: Value,
    workflow_agent: Option<&str>,
    node_agent: Option<&str>,
    policy: Option<Value>,
) -> Value {
    let mut node = json!({
        "id": "n1",
        "kind": kind,
        "retry_policy": null,
        "node_timeout_secs": null,
        "description": null,
        "labels": {}
    });
    if let Some(a) = node_agent {
        node["agent_ref"] = json!(a);
    }
    let mut ir = json!({
        "workflow_id": workflow_id,
        "version": "0.1.0",
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": "n1",
        "nodes": { "n1": node },
        "edges": [ { "from": "n1", "to": "end", "condition": null } ],
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {}
    });
    if let Some(a) = workflow_agent {
        ir["agent_ref"] = json!(a);
    }
    if let Some(p) = policy {
        ir["policy"] = p;
    }
    ir
}

fn calls(names_and_handlers: &[(&str, Option<&str>)]) -> Value {
    let tool_calls: Vec<Value> = names_and_handlers
        .iter()
        .map(|(n, _)| json!({ "id": "c", "name": n, "arguments": {} }))
        .collect();
    let mut tools = serde_json::Map::new();
    for (n, h) in names_and_handlers {
        if let Some(h) = h {
            tools.insert(n.to_string(), json!(h));
        }
    }
    json!({ "tool_calls": tool_calls, "tools": tools })
}

#[derive(Clone, Default)]
struct Counting(Arc<AtomicUsize>);

#[async_trait]
impl NodeExecutor for Counting {
    async fn execute(&self, _item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ExecutionResult {
            output: json!({}),
            state_patch: json!({}),
            duration_ms: 0,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum Transport {
    Claim,
    Worker,
}

const TRANSPORTS: [Transport; 2] = [Transport::Claim, Transport::Worker];

struct Outcome {
    /// The node's work was handed out (claim) or executed (worker).
    ran: bool,
    /// `(rule, policy_scope)` of every `PolicyViolation`.
    violations: Vec<(String, String)>,
}

/// A state over `tb`, with an agent registry of the same kind.
async fn state_for(tb: &support::TestBackend) -> AppState {
    let mut state = support::make_state(tb.dyn_backend());
    if let support::TestBackend::Sqlite { url, .. } = tb {
        state.agents = Arc::new(SqliteAgentRegistry::connect(url).await.unwrap());
    } else {
        state.agents = Arc::new(InMemoryAgentRegistry::new());
    }
    state
}

/// Register `cards`, register and start `ir` with `input`, then run the start
/// node over `transport`.
async fn run(
    tb: &support::TestBackend,
    transport: Transport,
    cards: &[Value],
    ir: Value,
    input: Value,
) -> Outcome {
    let state = state_for(tb).await;
    for c in cards {
        let (st, body) = support::post(&state, "/agents", c.clone()).await;
        assert!(st.is_success(), "register agent: {st} {body}");
    }
    let eid = if ir["nodes"]["n1"]["kind"]["module"] == json!("bank.wires") {
        // Stored as an older release would have accepted it: registration
        // now refuses the dispatch marker on a non-dispatcher.
        let workflow_id = ir["workflow_id"].as_str().unwrap().to_string();
        tb.dyn_backend()
            .store_workflow(jamjet_state::WorkflowDefinition {
                workflow_id: workflow_id.clone(),
                version: "0.1.0".into(),
                ir,
                created_at: chrono::Utc::now(),
                tenant_id: "default".into(),
            })
            .await
            .unwrap();
        let (st, body) = support::post(
            &state,
            "/executions",
            json!({ "workflow_id": workflow_id, "workflow_version": "0.1.0", "input": input }),
        )
        .await;
        assert!(st.is_success(), "start: {st} {body}");
        body["execution_id"].as_str().unwrap().to_string()
    } else {
        support::register_and_start(&state, ir, input).await
    };
    let eid =
        ExecutionId(uuid::Uuid::parse_str(eid.strip_prefix("exec_").unwrap_or(&eid)).unwrap());
    let queues = ["python_tool", "tool", "general"];
    let ran = match transport {
        Transport::Claim => {
            let (_, claimed) = support::post(
                &state,
                "/work-items/claim",
                json!({ "worker_id": "w", "queue_types": queues }),
            )
            .await;
            claimed["claimed"] == json!(true)
        }
        Transport::Worker => {
            let exec = Counting::default();
            let worker = jamjet_worker::Worker::new(
                "w".into(),
                tb.dyn_backend(),
                queues.iter().map(|q| q.to_string()).collect(),
            )
            .with_agents(state.agents.clone())
            .register_executor("mcp_tool", Arc::new(exec.clone()))
            .register_executor("python_fn", Arc::new(exec.clone()));
            let _ = worker.poll_and_execute().await;
            exec.0.load(Ordering::SeqCst) > 0
        }
    };
    let violations = tb
        .dyn_backend()
        .get_events(&eid)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::PolicyViolation {
                rule, policy_scope, ..
            } => Some((rule, policy_scope)),
            _ => None,
        })
        .collect();
    Outcome { ran, violations }
}

fn assert_refused(o: &Outcome, needle: &str, ctx: &str) {
    assert!(!o.ran, "{ctx}: the node ran");
    assert!(
        o.violations
            .iter()
            .any(|(rule, scope)| rule.starts_with("agent allowed_tools")
                && rule.contains(needle)
                && scope == "agent"),
        "{ctx}: no agent allowed_tools violation naming {needle}: {:?}",
        o.violations
    );
}

/// Every case on every backend and transport.
async fn each(f: impl Fn(String) -> (Vec<Value>, Value, Value)) -> Vec<(String, Outcome)> {
    let mut out = Vec::new();
    // A fresh pair of backends per transport: the SQLite one holds the
    // registered cards, and URIs are unique.
    for t in TRANSPORTS {
        for tb in support::backends().await {
            let ctx = format!("{} over {:?}", tb.name(), t);
            let (cards, ir, input) = f(format!("wf-{}-{:?}", tb.name(), t).to_lowercase());
            out.push((ctx, run(&tb, t, &cards, ir, input).await));
        }
    }
    out
}

#[tokio::test]
async fn an_unlisted_mcp_tool_is_refused() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["search_*"])],
            ir(&wf, mcp_tool("send_wire"), Some(AGENT), None, None),
            json!({}),
        )
    })
    .await
    {
        assert_refused(&o, "send_wire", &ctx);
    }
}

#[tokio::test]
async fn a_listed_mcp_tool_runs() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["search_*"])],
            ir(&wf, mcp_tool("search_web"), Some(AGENT), None, None),
            json!({}),
        )
    })
    .await
    {
        assert!(o.ran, "{ctx}: {:?}", o.violations);
        assert!(o.violations.is_empty(), "{ctx}: {:?}", o.violations);
    }
}

#[tokio::test]
async fn an_empty_list_restricts_nothing() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &[])],
            ir(&wf, mcp_tool("send_wire"), Some(AGENT), None, None),
            json!({}),
        )
    })
    .await
    {
        assert!(o.ran, "{ctx}: {:?}", o.violations);
    }
}

#[tokio::test]
async fn a_node_level_agent_is_enforced() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["search_*"])],
            ir(&wf, mcp_tool("send_wire"), None, Some(AGENT), None),
            json!({}),
        )
    })
    .await
    {
        assert_refused(&o, "send_wire", &ctx);
    }
}

/// Both agents must allow the node: the node's own agent cannot widen the
/// workflow's.
#[tokio::test]
async fn every_agent_the_node_acts_for_must_allow_it() {
    for (ctx, o) in each(|wf| {
        (
            vec![
                card(AGENT, &["search_*"]),
                card("jamjet://acme/payer", &["send_wire"]),
            ],
            ir(
                &wf,
                mcp_tool("send_wire"),
                Some(AGENT),
                Some("jamjet://acme/payer"),
                None,
            ),
            json!({}),
        )
    })
    .await
    {
        assert_refused(&o, AGENT, &ctx);
    }
}

#[tokio::test]
async fn an_unregistered_agent_refuses_the_node() {
    for (ctx, o) in each(|wf| {
        (
            vec![],
            ir(&wf, mcp_tool("search_web"), Some(AGENT), None, None),
            json!({}),
        )
    })
    .await
    {
        assert_refused(&o, "not registered", &ctx);
    }
}

/// An additional restriction: the policy chain still applies to a listed tool.
#[tokio::test]
async fn a_policy_block_still_applies_to_a_listed_tool() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["search_*"])],
            ir(
                &wf,
                mcp_tool("search_web"),
                Some(AGENT),
                None,
                Some(json!({ "blocked_tools": ["search_web"], "require_approval_for": [], "model_allowlist": [] })),
            ),
            json!({}),
        )
    })
    .await
    {
        assert!(!o.ran, "{ctx}");
        assert!(
            o.violations
                .iter()
                .any(|(rule, scope)| rule.contains("blocked pattern") && scope == "workflow"),
            "{ctx}: {:?}",
            o.violations
        );
    }
}

/// And a policy that allows the tool does not override the list.
#[tokio::test]
async fn a_policy_cannot_widen_the_list() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["search_*"])],
            ir(
                &wf,
                mcp_tool("send_wire"),
                Some(AGENT),
                None,
                Some(json!({ "blocked_tools": [], "require_approval_for": [], "model_allowlist": [] })),
            ),
            json!({}),
        )
    })
    .await
    {
        assert_refused(&o, "send_wire", &ctx);
    }
}

#[tokio::test]
async fn an_unlisted_dispatch_call_is_refused() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["search_web"])],
            ir(&wf, adk_dispatch(), Some(AGENT), None, None),
            calls(&[
                ("search_web", Some("app.tools:search_web")),
                ("send_wire", Some("bank.wires:send_wire")),
            ]),
        )
    })
    .await
    {
        assert_refused(&o, "send_wire", &ctx);
    }
}

#[tokio::test]
async fn a_listed_dispatch_call_runs() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["search_web"])],
            ir(&wf, adk_dispatch(), Some(AGENT), None, None),
            calls(&[("search_web", Some("app.tools:search_web"))]),
        )
    })
    .await
    {
        assert!(o.ran, "{ctx}: {:?}", o.violations);
    }
}

/// A listed name whose `tools` entry resolves to other code is refused.
#[tokio::test]
async fn a_listed_name_resolving_to_unlisted_code_is_refused() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["search_web"])],
            ir(&wf, adk_dispatch(), Some(AGENT), None, None),
            calls(&[("search_web", Some("bank.wires:send_wire"))]),
        )
    })
    .await
    {
        assert_refused(&o, "bank.wires:send_wire", &ctx);
    }
}

/// The dispatch marker is the author's to set. A marked function node that is
/// not the ADK dispatcher is matched on its own handler too, so marking it
/// does not exempt its code. Registration refuses such a node now; one stored
/// before that is still in the backend.
#[tokio::test]
async fn a_marked_function_node_is_matched_on_its_own_handler() {
    let marked = json!({
        "type": "python_fn",
        "module": "bank.wires",
        "function": "send_wire",
        "output_schema": "",
        "agent_tool_dispatch": true
    });
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["search_web"])],
            ir(&wf, marked.clone(), Some(AGENT), None, None),
            json!({ "tool_calls": [] }),
        )
    })
    .await
    {
        assert_refused(&o, "bank.wires:send_wire", &ctx);
    }
}

/// A registry that cannot be read holds the node back; it never runs on a
/// guess and no denial is recorded.
#[tokio::test]
async fn an_unreadable_registry_holds_the_node() {
    struct Broken;
    #[async_trait]
    impl AgentRegistry for Broken {
        async fn register(&self, _: jamjet_agents::AgentCard) -> Result<uuid::Uuid, String> {
            Err("down".into())
        }
        async fn get(
            &self,
            _: uuid::Uuid,
        ) -> Result<Option<jamjet_agents::registry::Agent>, String> {
            Err("down".into())
        }
        async fn get_by_uri(
            &self,
            _: &str,
        ) -> Result<Option<jamjet_agents::registry::Agent>, String> {
            Err("down".into())
        }
        async fn find(
            &self,
            _: jamjet_agents::AgentFilter,
        ) -> Result<Vec<jamjet_agents::registry::Agent>, String> {
            Err("down".into())
        }
        async fn update_status(
            &self,
            _: uuid::Uuid,
            _: jamjet_agents::AgentStatus,
        ) -> Result<(), String> {
            Err("down".into())
        }
        async fn heartbeat(&self, _: uuid::Uuid) -> Result<(), String> {
            Err("down".into())
        }
        async fn discover_remote(&self, _: &str) -> Result<jamjet_agents::registry::Agent, String> {
            Err("down".into())
        }
    }
    for tb in support::backends().await {
        let mut state = support::make_state(tb.dyn_backend());
        state.agents = Arc::new(Broken);
        let eid = support::register_and_start(
            &state,
            ir("wf-broken", mcp_tool("search_web"), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        let (_, claimed) = support::post(
            &state,
            "/work-items/claim",
            json!({ "worker_id": "w", "queue_types": ["tool"] }),
        )
        .await;
        assert_eq!(claimed, json!({ "claimed": false }), "{}", tb.name());
        let eid =
            ExecutionId(uuid::Uuid::parse_str(eid.strip_prefix("exec_").unwrap_or(&eid)).unwrap());
        let events = tb.dyn_backend().get_events(&eid).await.unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, EventKind::PolicyViolation { .. })),
            "{}: an unreadable registry is not a denial",
            tb.name()
        );
    }
}

/// A worker built without a registry refuses an attributed node rather than
/// skipping the check.
#[tokio::test]
async fn a_worker_without_a_registry_refuses_an_attributed_node() {
    for tb in support::backends().await {
        let state = state_for(&tb).await;
        let (st, _) = support::post(&state, "/agents", card(AGENT, &[])).await;
        assert!(st.is_success());
        let eid = support::register_and_start(
            &state,
            ir("wf-noreg", mcp_tool("search_web"), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        let exec = Counting::default();
        let worker = jamjet_worker::Worker::new("w".into(), tb.dyn_backend(), vec!["tool".into()])
            .register_executor("mcp_tool", Arc::new(exec.clone()));
        let _ = worker.poll_and_execute().await;
        assert_eq!(exec.0.load(Ordering::SeqCst), 0, "{}", tb.name());
        let eid =
            ExecutionId(uuid::Uuid::parse_str(eid.strip_prefix("exec_").unwrap_or(&eid)).unwrap());
        let events = tb.dyn_backend().get_events(&eid).await.unwrap();
        assert!(
            events.iter().any(|e| matches!(
                &e.kind,
                EventKind::PolicyViolation { rule, .. } if rule.contains("no agent registry")
            )),
            "{}",
            tb.name()
        );
    }
}

/// A workflow-level `agent_ref` that is present but not a string is refused
/// at registration or fails the node closed; it never reads as "no agent".
#[tokio::test]
async fn a_malformed_workflow_agent_is_not_ignored() {
    for tb in support::backends().await {
        let state = state_for(&tb).await;
        let mut bad = ir("wf-bad-agent", mcp_tool("send_wire"), None, None, None);
        bad["agent_ref"] = json!(42);
        let (st, _) = support::post(&state, "/workflows", json!({ "ir": bad })).await;
        assert!(
            !st.is_success(),
            "{}: a non-string agent_ref registered",
            tb.name()
        );
    }
}

/// One agent per URI in the in-memory registry too, so a later, laxer card
/// cannot shadow the one that restricts a node.
#[tokio::test]
async fn a_second_card_at_the_same_uri_is_refused() {
    for tb in support::backends().await {
        let state = state_for(&tb).await;
        let (st, _) = support::post(&state, "/agents", card(AGENT, &["search_*"])).await;
        assert!(st.is_success());
        let (st, _) = support::post(&state, "/agents", card(AGENT, &[])).await;
        assert!(!st.is_success(), "{}: duplicate uri registered", tb.name());
    }
}

/// An agent with no list does not waive another agent's list.
#[tokio::test]
async fn an_unrestricted_agent_does_not_waive_another() {
    for (ctx, o) in each(|wf| {
        (
            vec![card("jamjet://acme/open", &[]), card(AGENT, &["search_*"])],
            ir(
                &wf,
                mcp_tool("send_wire"),
                Some("jamjet://acme/open"),
                Some(AGENT),
                None,
            ),
            json!({}),
        )
    })
    .await
    {
        assert_refused(&o, "send_wire", &ctx);
    }
}

/// Dispatch calls that cannot be read are refused under a list even when no
/// policy applies (the policy path allows an unpoliced node before it reads
/// the calls).
#[tokio::test]
async fn unreadable_dispatch_calls_are_refused() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["search_web"])],
            ir(&wf, adk_dispatch(), Some(AGENT), None, None),
            json!({ "tool_calls": "not a list" }),
        )
    })
    .await
    {
        assert!(!o.ran, "{ctx}");
        assert!(
            o.violations
                .iter()
                .any(|(rule, _)| rule.starts_with("agent dispatch tool calls unreadable")),
            "{ctx}: {:?}",
            o.violations
        );
    }
}

/// The completion gate applies the list too: a result for a node the agent
/// may not use is not recorded, whatever path the item took to a worker.
#[tokio::test]
async fn a_result_for_an_unlisted_tool_is_not_recorded() {
    for tb in support::backends().await {
        let state = state_for(&tb).await;
        let (st, _) = support::post(&state, "/agents", card(AGENT, &["search_*"])).await;
        assert!(st.is_success());
        let eid = support::register_and_start(
            &state,
            ir(
                "wf-complete",
                mcp_tool("send_wire"),
                Some(AGENT),
                None,
                None,
            ),
            json!({}),
        )
        .await;
        // Taken off the queue without the claim gate.
        let item = tb
            .dyn_backend()
            .claim_work_item_with_lease("in-process", &["tool"], None)
            .await
            .unwrap()
            .expect("the start item");
        let (st, body) = support::post(
            &state,
            &format!("/work-items/{}/complete", item.id),
            json!({ "output": {}, "state_patch": {}, "lease_fence": item.lease_fence }),
        )
        .await;
        assert_eq!(
            st,
            axum::http::StatusCode::CONFLICT,
            "{}: {body}",
            tb.name()
        );
        let eid =
            ExecutionId(uuid::Uuid::parse_str(eid.strip_prefix("exec_").unwrap_or(&eid)).unwrap());
        let events = tb.dyn_backend().get_events(&eid).await.unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, EventKind::NodeCompleted { .. })),
            "{}",
            tb.name()
        );
    }
}

/// A finalizer runs its `tool_ref`, so the list applies to it.
#[tokio::test]
async fn an_unlisted_finalizer_tool_is_refused() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["search_*"])],
            ir(
                &wf,
                json!({ "type": "finalizer", "tool_ref": "send_wire", "run_on": "always" }),
                Some(AGENT),
                None,
                None,
            ),
            json!({}),
        )
    })
    .await
    {
        assert_refused(&o, "send_wire", &ctx);
    }
}
