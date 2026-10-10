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
    /// The context of every `ToolApprovalRequired`.
    requests: Vec<Value>,
    eid: ExecutionId,
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
    run_in(tb, transport, cards, ir, input).await.1
}

/// [`run`], keeping the state for a later step.
async fn run_in(
    tb: &support::TestBackend,
    transport: Transport,
    cards: &[Value],
    ir: Value,
    input: Value,
) -> (AppState, Outcome) {
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
    let ran = fire(&state, tb, transport).await;
    let o = observe(tb, &eid, ran).await;
    (state, o)
}

/// Take the next item off the queue over `transport`: true if it was handed
/// out (claim) or executed (worker).
async fn fire(state: &AppState, tb: &support::TestBackend, transport: Transport) -> bool {
    let queues = ["python_tool", "tool", "general"];
    match transport {
        Transport::Claim => {
            let (_, claimed) = support::post(
                state,
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
            .register_executor("python_fn", Arc::new(exec.clone()))
            .register_executor("agent_tool", Arc::new(exec.clone()))
            .register_executor("coordinator", Arc::new(exec.clone()))
            .register_executor("agent", Arc::new(exec.clone()))
            .register_executor("a2a_task", Arc::new(exec.clone()));
            let _ = worker.poll_and_execute().await;
            exec.0.load(Ordering::SeqCst) > 0
        }
    }
}

async fn observe(tb: &support::TestBackend, eid: &ExecutionId, ran: bool) -> Outcome {
    let events = tb.dyn_backend().get_events(eid).await.unwrap();
    let violations = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::PolicyViolation {
                rule, policy_scope, ..
            } => Some((rule.clone(), policy_scope.clone())),
            _ => None,
        })
        .collect();
    let requests = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired { context, .. } => Some(context.clone()),
            _ => None,
        })
        .collect();
    Outcome {
        ran,
        violations,
        requests,
        eid: eid.clone(),
    }
}

fn assert_refused(o: &Outcome, needle: &str, ctx: &str) {
    assert!(!o.ran, "{ctx}: the node ran");
    assert!(
        o.violations
            .iter()
            .any(|(rule, scope)| rule.starts_with("agent ")
                && rule.contains(needle)
                && scope == "agent"),
        "{ctx}: no agent card violation naming {needle}: {:?}",
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

fn agent_tool(target: Value) -> Value {
    json!({ "type": "agent_tool", "agent": target, "output_key": "o", "timeout_ms": null, "budget": null })
}

/// An `agent_tool` node calls another agent as a tool: it is matched on the
/// agent it names (review I1).
#[tokio::test]
async fn an_unlisted_agent_tool_is_refused() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["search_*"])],
            ir(
                &wf,
                agent_tool(json!({ "explicit": "https://evil.example/wire-agent" })),
                Some(AGENT),
                None,
                None,
            ),
            json!({}),
        )
    })
    .await
    {
        assert_refused(&o, "https://evil.example/wire-agent", &ctx);
    }
}

/// Control: a listed agent runs.
#[tokio::test]
async fn a_listed_agent_tool_runs() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["https://ok.example/*"])],
            ir(
                &wf,
                agent_tool(json!({ "explicit": "https://ok.example/search-agent" })),
                Some(AGENT),
                None,
                None,
            ),
            json!({}),
        )
    })
    .await
    {
        assert!(o.ran, "{ctx}: {:?}", o.violations);
    }
}

/// A node that picks what to call at run time has no name to check, so a
/// restricted agent cannot run it.
#[tokio::test]
async fn a_coordinator_under_a_list_is_refused() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["search_*"])],
            ir(
                &wf,
                json!({ "type": "coordinator", "task": "t", "required_skills": [], "output_key": "o" }),
                Some(AGENT),
                None,
                None,
            ),
            json!({}),
        )
    })
    .await
    {
        assert_refused(&o, "coordinator", &ctx);
    }
}

/// A name pattern does not reach across a module path (review M1): under
/// `read_*`, a call named `read_doc` that resolves to
/// `read_utils:delete_everything` is refused, and one that resolves to a
/// function named `read_doc` runs.
#[tokio::test]
async fn a_name_pattern_does_not_cross_a_module_path() {
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["read_*"])],
            ir(&wf, adk_dispatch(), Some(AGENT), None, None),
            calls(&[("read_doc", Some("read_utils:delete_everything"))]),
        )
    })
    .await
    {
        assert_refused(&o, "read_utils:delete_everything", &ctx);
    }
    for (ctx, o) in each(|wf| {
        (
            vec![card(AGENT, &["read_*"])],
            ir(&wf, adk_dispatch(), Some(AGENT), None, None),
            calls(&[("read_doc", Some("app.tools:read_doc"))]),
        )
    })
    .await
    {
        assert!(o.ran, "{ctx}: {:?}", o.violations);
    }
}

/// The completion gate checks a dispatch node's calls too (review T3).
#[tokio::test]
async fn a_result_for_an_unlisted_dispatch_call_is_not_recorded() {
    for tb in support::backends().await {
        let state = state_for(&tb).await;
        let (st, _) = support::post(&state, "/agents", card(AGENT, &["search_web"])).await;
        assert!(st.is_success());
        let eid = support::register_and_start(
            &state,
            ir(
                "wf-complete-dispatch",
                adk_dispatch(),
                Some(AGENT),
                None,
                None,
            ),
            calls(&[("send_wire", Some("bank.wires:send_wire"))]),
        )
        .await;
        let item = tb
            .dyn_backend()
            .claim_work_item_with_lease("in-process", &["python_tool"], None)
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

// ── The card's own blocked_tools and require_approval_for ──────────────────

fn card_with(uri: &str, constraints: Value) -> Value {
    let mut c = card(uri, &[]);
    c["constraints"] = constraints;
    c
}

#[tokio::test]
async fn a_card_block_refuses_a_node() {
    for (ctx, o) in each(|wf| {
        (
            vec![card_with(AGENT, json!({ "blocked_tools": ["send_*"] }))],
            ir(&wf, mcp_tool("send_wire"), Some(AGENT), None, None),
            json!({}),
        )
    })
    .await
    {
        assert_refused(&o, "send_wire", &ctx);
        assert!(
            o.violations
                .iter()
                .any(|(r, _)| r.starts_with("agent blocked_tools")),
            "{ctx}: {:?}",
            o.violations
        );
    }
}

/// A call is blocked under another name when its code is blocked.
#[tokio::test]
async fn a_card_block_catches_a_call_by_its_code() {
    for (ctx, o) in each(|wf| {
        (
            vec![card_with(AGENT, json!({ "blocked_tools": ["send_wire"] }))],
            ir(&wf, adk_dispatch(), Some(AGENT), None, None),
            calls(&[("pay", Some("bank.wires:send_wire"))]),
        )
    })
    .await
    {
        assert_refused(&o, "bank.wires:send_wire", &ctx);
    }
}

/// A block wins over the agent's own allow list.
#[tokio::test]
async fn a_card_block_wins_over_its_allow_list() {
    for (ctx, o) in each(|wf| {
        (
            vec![card_with(
                AGENT,
                json!({ "allowed_tools": ["send_*"], "blocked_tools": ["send_wire"] }),
            )],
            ir(&wf, mcp_tool("send_wire"), Some(AGENT), None, None),
            json!({}),
        )
    })
    .await
    {
        assert_refused(&o, "send_wire", &ctx);
    }
}

/// Control: a card block names nothing else.
#[tokio::test]
async fn a_card_block_leaves_other_tools_alone() {
    for (ctx, o) in each(|wf| {
        (
            vec![card_with(AGENT, json!({ "blocked_tools": ["send_*"] }))],
            ir(&wf, mcp_tool("search_web"), Some(AGENT), None, None),
            json!({}),
        )
    })
    .await
    {
        assert!(o.ran, "{ctx}: {:?}", o.violations);
    }
}

/// A card gate holds the node behind the ordinary approval request: bound to
/// the action digest, with the policy chain's deadline, and no denial.
#[tokio::test]
async fn a_card_gate_holds_the_node_for_approval() {
    let policy = json!({
        "blocked_tools": [], "require_approval_for": [], "model_allowlist": [],
        "approval_timeout_secs": 3600
    });
    for (ctx, o) in each(|wf| {
        (
            vec![card_with(
                AGENT,
                json!({ "require_approval_for": ["send_*"] }),
            )],
            ir(
                &wf,
                mcp_tool("send_wire"),
                Some(AGENT),
                None,
                Some(policy.clone()),
            ),
            json!({}),
        )
    })
    .await
    {
        assert!(!o.ran, "{ctx}: the gated node ran");
        assert!(o.violations.is_empty(), "{ctx}: {:?}", o.violations);
        assert_eq!(o.requests.len(), 1, "{ctx}");
        let req = &o.requests[0];
        assert!(req["action_digest"].is_string(), "{ctx}: {req}");
        assert!(
            req["expires_at"].is_string(),
            "{ctx}: the chain's deadline: {req}"
        );
    }
}

/// A card gate on a dispatch call holds the batch.
#[tokio::test]
async fn a_card_gate_holds_a_dispatch_call() {
    for (ctx, o) in each(|wf| {
        (
            vec![card_with(
                AGENT,
                json!({ "require_approval_for": ["send_wire"] }),
            )],
            ir(&wf, adk_dispatch(), Some(AGENT), None, None),
            calls(&[("pay", Some("bank.wires:send_wire"))]),
        )
    })
    .await
    {
        assert!(!o.ran, "{ctx}");
        assert_eq!(o.requests.len(), 1, "{ctx}: {:?}", o.violations);
    }
}

/// Approving the request releases the action it was raised for, on both
/// transports.
#[tokio::test]
async fn an_approved_card_gate_fires() {
    for t in TRANSPORTS {
        for tb in support::backends().await {
            let ctx = format!("{} over {t:?}", tb.name());
            let wf = format!("wf-gate-{}-{t:?}", tb.name()).to_lowercase();
            let ir_v = ir(&wf, mcp_tool("send_wire"), Some(AGENT), None, None);
            let (state, o) = run_in(
                &tb,
                t,
                &[card_with(
                    AGENT,
                    json!({ "require_approval_for": ["send_*"] }),
                )],
                ir_v.clone(),
                json!({}),
            )
            .await;
            assert!(!o.ran, "{ctx}");
            let digest = o.requests[0]["action_digest"].as_str().unwrap().to_string();
            let eid = format!("exec_{}", o.eid.0);
            let (st, body) = support::post(
                &state,
                &format!("/executions/{eid}/approve"),
                json!({ "decision": "approved", "node_id": "n1", "action_digest": digest }),
            )
            .await;
            assert!(st.is_success(), "{ctx}: approve: {st} {body}");
            // Re-dispatch the node as the scheduler would.
            let ir_t: jamjet_ir::WorkflowIr = serde_json::from_value(ir_v).unwrap();
            let payload = jamjet_scheduler::build_work_item_payload(
                &ir_t,
                "n1",
                ir_t.node("n1").unwrap(),
                &serde_json::Map::new(),
                &wf,
                "0.1.0",
            );
            let (st, body) = support::post(
                &state,
                "/work-items",
                json!({ "execution_id": eid, "node_id": "n1", "queue_type": "tool", "payload": payload }),
            )
            .await;
            assert!(st.is_success(), "{ctx}: enqueue: {st} {body}");
            assert!(
                fire(&state, &tb, t).await,
                "{ctx}: the approved node did not fire"
            );
        }
    }
}

// ── allowed_delegations ─────────────────────────────────────────────────────

fn agent_node(target: &str) -> Value {
    json!({ "type": "agent", "agent_ref": target, "input_mapping": {}, "output_schema": "" })
}

fn a2a_task(remote: &str) -> Value {
    json!({
        "type": "a2a_task", "remote_agent": remote, "skill": "s", "input_mapping": {},
        "output_schema": "", "stream": false, "on_input_required": null, "timeout_secs": null
    })
}

fn delegating(to: &[&str]) -> Value {
    card_with(AGENT, json!({ "allowed_delegations": to }))
}

#[tokio::test]
async fn a_delegation_to_an_unlisted_agent_is_refused() {
    for (kind, target) in [
        (agent_node("jamjet://acme/payer"), "jamjet://acme/payer"),
        (
            agent_tool(json!({ "explicit": "https://evil.example/payer" })),
            "https://evil.example/payer",
        ),
        (a2a_task("payer"), "payer"),
    ] {
        for (ctx, o) in each(|wf| {
            (
                vec![delegating(&["jamjet://acme/helper", "helper"])],
                ir(&wf, kind.clone(), Some(AGENT), None, None),
                json!({}),
            )
        })
        .await
        {
            assert_refused(&o, target, &ctx);
            assert!(
                o.violations
                    .iter()
                    .any(|(r, _)| r.starts_with("agent allowed_delegations")),
                "{ctx}: {:?}",
                o.violations
            );
        }
    }
}

#[tokio::test]
async fn a_delegation_to_a_listed_agent_runs() {
    for kind in [
        agent_node("jamjet://acme/helper"),
        agent_tool(json!({ "explicit": "jamjet://acme/helper" })),
    ] {
        for (ctx, o) in each(|wf| {
            (
                vec![delegating(&["jamjet://acme/helper", "helper"])],
                ir(&wf, kind.clone(), Some(AGENT), None, None),
                json!({}),
            )
        })
        .await
        {
            assert!(o.ran, "{ctx}: {kind}: {:?}", o.violations);
        }
    }
}

/// A node that chooses whom to call at run time cannot be checked against
/// a delegation list, so it is refused under one.
#[tokio::test]
async fn a_run_time_delegation_is_refused_under_a_list() {
    for (ctx, o) in each(|wf| {
        (
            vec![delegating(&["jamjet://acme/helper"])],
            ir(
                &wf,
                json!({ "type": "coordinator", "task": "t", "required_skills": [], "output_key": "o" }),
                Some(AGENT),
                None,
                None,
            ),
            json!({}),
        )
    })
    .await
    {
        assert_refused(&o, "coordinator", &ctx);
    }
}

/// An empty delegation list restricts nothing.
#[tokio::test]
async fn an_empty_delegation_list_restricts_nothing() {
    for (ctx, o) in each(|wf| {
        (
            vec![delegating(&[])],
            ir(
                &wf,
                agent_node("jamjet://acme/payer"),
                Some(AGENT),
                None,
                None,
            ),
            json!({}),
        )
    })
    .await
    {
        assert!(o.ran, "{ctx}: {:?}", o.violations);
    }
}

// ── The SDK's agent_ref, end to end ─────────────────────────────────────────

/// The IR the Python agent compiler emits for an `Agent(agent_ref=...)`
/// (written by `sdk/python/tests/test_agent_ref.py`), registered as the SDK
/// registers it, with the dispatch node re-dispatched carrying model-chosen
/// calls. The card is enforced through the IR's `agent_ref` alone: nothing
/// in this test attributes the node.
#[tokio::test]
async fn the_sdk_compiled_agent_is_held_to_its_card() {
    let fixture: Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../sdk/python/tests/fixtures/agent_ref_agent_ir.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let ir_v = fixture["ir"].clone();
    assert_eq!(
        ir_v["agent_ref"],
        json!(AGENT),
        "the SDK emitted no agent_ref"
    );
    let tools = fixture["tools"].clone();
    let wf = ir_v["workflow_id"].as_str().unwrap().to_string();
    let version = ir_v["version"].as_str().unwrap().to_string();
    let ir_t: jamjet_ir::WorkflowIr = serde_json::from_value(ir_v.clone()).unwrap();
    let dispatch = ir_t
        .nodes
        .values()
        .find(|n| jamjet_worker::dispatch_guard::is_agent_tool_dispatch(&n.kind))
        .expect("a dispatch node")
        .clone();

    for (call, expect_run) in [("search_web", true), ("send_wire", false)] {
        for t in TRANSPORTS {
            for tb in support::backends().await {
                let ctx = format!("{call} on {} over {t:?}", tb.name());
                let state = state_for(&tb).await;
                let (st, _) = support::post(&state, "/agents", card(AGENT, &["search_web"])).await;
                assert!(st.is_success());
                let (st, body) = support::post(&state, "/workflows", json!({ "ir": ir_v })).await;
                assert!(st.is_success(), "{ctx}: register: {st} {body}");
                let (st, body) = support::post(
                    &state,
                    "/executions",
                    json!({ "workflow_id": wf, "workflow_version": version, "input": {} }),
                )
                .await;
                assert!(st.is_success(), "{ctx}: start: {st} {body}");
                let eid = body["execution_id"].as_str().unwrap().to_string();
                // Drain the start (model) item so only the dispatch is queued.
                let _ = tb
                    .dyn_backend()
                    .claim_work_item_with_lease("drain", &["model"], None)
                    .await
                    .unwrap();
                let mut run_state = serde_json::Map::new();
                run_state.insert(
                    "last_model_tool_calls".into(),
                    json!([{ "id": "c1", "name": call, "arguments": {} }]),
                );
                run_state.insert("tools".into(), tools.clone());
                let payload = jamjet_scheduler::build_work_item_payload(
                    &ir_t,
                    &dispatch.id,
                    &dispatch,
                    &run_state,
                    &wf,
                    &version,
                );
                let (st, body) = support::post(
                    &state,
                    "/work-items",
                    json!({ "execution_id": eid, "node_id": dispatch.id, "queue_type": "python_tool", "payload": payload }),
                )
                .await;
                assert!(st.is_success(), "{ctx}: enqueue: {st} {body}");
                let ran = fire(&state, &tb, t).await;
                assert_eq!(ran, expect_run, "{ctx}");
                if !expect_run {
                    let eid = ExecutionId(
                        uuid::Uuid::parse_str(eid.strip_prefix("exec_").unwrap_or(&eid)).unwrap(),
                    );
                    let o = observe(&tb, &eid, ran).await;
                    assert_refused(&o, "send_wire", &ctx);
                }
            }
        }
    }
}

// ── Second review: card gates, chain gates and blocks in one batch ─────────

fn chain(blocked: &[&str], gated: &[&str]) -> Value {
    json!({ "blocked_tools": blocked, "require_approval_for": gated, "model_allowlist": [] })
}

fn dispatch_input(calls_v: Value, extra: &[(&str, Value)]) -> Value {
    let mut tools = serde_json::Map::new();
    for c in calls_v.as_array().unwrap() {
        if let Some(h) = c.get("handler") {
            tools.insert(c["name"].as_str().unwrap().to_string(), h.clone());
        }
    }
    let calls_v: Vec<Value> = calls_v
        .as_array()
        .unwrap()
        .iter()
        .map(|c| json!({ "id": "c", "name": c["name"], "arguments": c.get("arguments").cloned().unwrap_or(json!({})) }))
        .collect();
    let mut input = json!({ "tool_calls": calls_v, "tools": tools });
    for (k, v) in extra {
        input[*k] = v.clone();
    }
    input
}

/// R1: a call the chain blocks is refused before any approval is asked for,
/// even when the card gates another call in the batch.
#[tokio::test]
async fn a_chain_blocked_call_is_refused_not_gated_by_a_card() {
    for (ctx, o) in each(|wf| {
        (
            vec![card_with(
                AGENT,
                json!({ "require_approval_for": ["search"] }),
            )],
            ir(
                &wf,
                adk_dispatch(),
                Some(AGENT),
                None,
                Some(chain(&["send_wire"], &[])),
            ),
            calls(&[
                ("search", Some("app.tools:search")),
                ("pay", Some("bank.wires:send_wire")),
            ]),
        )
    })
    .await
    {
        assert!(!o.ran, "{ctx}");
        assert!(
            o.requests.is_empty(),
            "{ctx}: an approval was asked for a blocked call"
        );
        assert!(
            !o.violations.is_empty(),
            "{ctx}: the block was not recorded"
        );
    }
}

/// R3: the request a card gate raises names every gated call, the chain's
/// included.
#[tokio::test]
async fn a_card_gate_request_names_every_gated_call() {
    for (ctx, o) in each(|wf| {
        (
            vec![card_with(
                AGENT,
                json!({ "require_approval_for": ["search"] }),
            )],
            ir(
                &wf,
                adk_dispatch(),
                Some(AGENT),
                None,
                Some(chain(&[], &["send_wire"])),
            ),
            calls(&[
                ("search", Some("app.tools:search")),
                ("pay", Some("bank.wires:send_wire")),
            ]),
        )
    })
    .await
    {
        assert_eq!(o.requests.len(), 1, "{ctx}");
        let gated = &o.requests[0]["gated_tools"];
        assert!(
            gated
                .as_array()
                .is_some_and(|g| g.contains(&json!("search")) && g.contains(&json!("pay"))),
            "{ctx}: {gated}"
        );
    }
}

/// R2: the approver sees the calls and their arguments, even when the
/// payload is too large to preview.
#[tokio::test]
async fn a_card_gate_request_shows_the_calls_of_a_large_payload() {
    for (ctx, o) in each(|wf| {
        (
            vec![card_with(AGENT, json!({ "require_approval_for": ["send_wire"] }))],
            ir(&wf, adk_dispatch(), Some(AGENT), None, None),
            dispatch_input(
                json!([{ "name": "pay", "handler": "bank.wires:send_wire", "arguments": { "to": "ATTACKER-IBAN" } }]),
                &[("messages", json!("x".repeat(20_000)))],
            ),
        )
    })
    .await
    {
        assert_eq!(o.requests.len(), 1, "{ctx}");
        assert!(
            o.requests[0]["calls"].to_string().contains("ATTACKER-IBAN"),
            "{ctx}: the approver cannot see the call"
        );
    }
}

/// An approved card-gated dispatch fires, on both transports.
#[tokio::test]
async fn an_approved_card_gated_dispatch_fires() {
    let input = calls(&[("pay", Some("bank.wires:send_wire"))]);
    for t in TRANSPORTS {
        for tb in support::backends().await {
            let ctx = format!("{} over {t:?}", tb.name());
            let wf = format!("wf-dgate-{}-{t:?}", tb.name()).to_lowercase();
            let ir_v = ir(&wf, adk_dispatch(), Some(AGENT), None, None);
            let (state, o) = run_in(
                &tb,
                t,
                &[card_with(
                    AGENT,
                    json!({ "require_approval_for": ["send_wire"] }),
                )],
                ir_v.clone(),
                input.clone(),
            )
            .await;
            assert!(!o.ran, "{ctx}");
            let digest = o.requests[0]["action_digest"].as_str().unwrap().to_string();
            let eid = format!("exec_{}", o.eid.0);
            let (st, body) = support::post(
                &state,
                &format!("/executions/{eid}/approve"),
                json!({ "decision": "approved", "node_id": "n1", "action_digest": digest }),
            )
            .await;
            assert!(st.is_success(), "{ctx}: approve: {st} {body}");
            let ir_t: jamjet_ir::WorkflowIr = serde_json::from_value(ir_v).unwrap();
            let payload = jamjet_scheduler::build_work_item_payload(
                &ir_t,
                "n1",
                ir_t.node("n1").unwrap(),
                input.as_object().unwrap(),
                &wf,
                "0.1.0",
            );
            let (st, body) = support::post(
                &state,
                "/work-items",
                json!({ "execution_id": eid, "node_id": "n1", "queue_type": "python_tool", "payload": payload }),
            )
            .await;
            assert!(st.is_success(), "{ctx}: enqueue: {st} {body}");
            assert!(
                fire(&state, &tb, t).await,
                "{ctx}: the approved dispatch did not fire"
            );
        }
    }
}

/// R4: every node that picks its delegate at run time is refused under a
/// delegation list.
#[tokio::test]
async fn every_run_time_delegation_is_refused_under_a_list() {
    for kind in [
        json!({ "type": "agent_discovery", "skill": "s", "protocol": null, "output_binding": "o" }),
        agent_tool(json!("auto")),
    ] {
        for (ctx, o) in each(|wf| {
            (
                vec![delegating(&["jamjet://acme/helper"])],
                ir(&wf, kind.clone(), Some(AGENT), None, None),
                json!({}),
            )
        })
        .await
        {
            assert!(!o.ran, "{ctx}: {kind}");
            assert!(
                o.violations
                    .iter()
                    .any(|(r, _)| r.starts_with("agent allowed_delegations")),
                "{ctx}: {kind}: {:?}",
                o.violations
            );
        }
    }
}

/// R5: a chain block wins over a card gate on the same node.
#[tokio::test]
async fn a_chain_block_wins_over_a_card_gate() {
    for (ctx, o) in each(|wf| {
        (
            vec![card_with(
                AGENT,
                json!({ "require_approval_for": ["send_*"] }),
            )],
            ir(
                &wf,
                mcp_tool("send_wire"),
                Some(AGENT),
                None,
                Some(chain(&["send_wire"], &[])),
            ),
            json!({}),
        )
    })
    .await
    {
        assert!(!o.ran, "{ctx}");
        assert!(o.requests.is_empty(), "{ctx}");
        assert!(
            o.violations.iter().any(|(_, scope)| scope == "workflow"),
            "{ctx}: {:?}",
            o.violations
        );
    }
}

/// R6: a result for a card-gated node nobody approved is not recorded.
#[tokio::test]
async fn completing_an_unapproved_card_gated_node_is_refused() {
    for tb in support::backends().await {
        let state = state_for(&tb).await;
        let (st, _) = support::post(
            &state,
            "/agents",
            card_with(AGENT, json!({ "require_approval_for": ["send_*"] })),
        )
        .await;
        assert!(st.is_success());
        support::register_and_start(
            &state,
            ir(
                "wf-gate-complete",
                mcp_tool("send_wire"),
                Some(AGENT),
                None,
                None,
            ),
            json!({}),
        )
        .await;
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
    }
}

fn with_remote(mut ir_v: Value, name: &str, url: &str) -> Value {
    ir_v["remote_agents"] = json!({ name: { "url": url, "agent_card_path": null, "auth": null } });
    ir_v
}

/// I3: an a2a_task is checked by the URL it calls, which the IR maps the name
/// to, not by the name alone.
#[tokio::test]
async fn an_a2a_delegation_is_checked_by_its_url() {
    for (url, expect) in [
        ("https://partner.example/a2a", true),
        ("https://evil.example/a2a", false),
    ] {
        for (ctx, o) in each(|wf| {
            (
                vec![delegating(&["https://partner.example/*", "partner"])],
                with_remote(
                    ir(&wf, a2a_task("partner"), Some(AGENT), None, None),
                    "partner",
                    url,
                ),
                json!({}),
            )
        })
        .await
        {
            if expect {
                assert!(o.ran, "{ctx}: {url}: {:?}", o.violations);
            } else {
                assert_refused(&o, url, &ctx);
            }
        }
    }
}

struct BrokenRegistry;

#[async_trait]
impl AgentRegistry for BrokenRegistry {
    async fn register(&self, _: jamjet_agents::AgentCard) -> Result<uuid::Uuid, String> {
        Err("down".into())
    }
    async fn get(&self, _: uuid::Uuid) -> Result<Option<jamjet_agents::registry::Agent>, String> {
        Err("down".into())
    }
    async fn get_by_uri(&self, _: &str) -> Result<Option<jamjet_agents::registry::Agent>, String> {
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

/// The autonomy check fails closed: an `agent` node whose callee's card
/// cannot be read does not run.
#[tokio::test]
async fn an_agent_node_with_an_unreadable_card_does_not_run() {
    for tb in support::backends().await {
        let mut state = support::make_state(tb.dyn_backend());
        state.agents = Arc::new(BrokenRegistry);
        support::register_and_start(
            &state,
            ir(
                "wf-autonomy",
                agent_node("jamjet://acme/helper"),
                None,
                None,
                None,
            ),
            json!({}),
        )
        .await;
        assert!(!fire(&state, &tb, Transport::Worker).await, "{}", tb.name());
    }
}

/// A listed remote agent name with no URL in the workflow is refused: the
/// URL it would call is unknown.
#[tokio::test]
async fn an_a2a_name_without_a_url_is_refused() {
    for (ctx, o) in each(|wf| {
        (
            vec![delegating(&["partner"])],
            ir(&wf, a2a_task("partner"), Some(AGENT), None, None),
            json!({}),
        )
    })
    .await
    {
        assert_refused(&o, "not in the workflow's remote_agents", &ctx);
    }
}
