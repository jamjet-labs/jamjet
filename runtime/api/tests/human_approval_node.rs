//! A `human_approval` node waits for a person.
//!
//! Until 2026-11 the node had no executor, and a worker that claimed it
//! completed it with an empty output ("No executor; using stub"). A workflow
//! of `review (human_approval) -> publish` therefore published without anyone
//! being asked. The node now runs on the approval machinery that policy gates
//! use: the run parks on a `ToolApprovalRequired` bound to the action digest,
//! the approver is the authenticated token, the initiator cannot approve,
//! `timeout_secs` is a deadline, and an expiry routes to `fallback_node`.
//!
//! These tests drive the server's own in-process executors (the embedded
//! pool's), the scheduler and a stand-in external worker for the
//! `python_fn` steps, over the HTTP routes with auth on.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::ExecutionId;
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::{StateBackend, WorkItem};
use jamjet_state::event::EventKind;
use jamjet_state::Event;
use jamjet_worker::{ExecutionResult, ExecutorError, NodeExecutor, Worker};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

mod support;

/// `review` (human_approval) -> `publish`; with `fallback`, `review` names
/// `hold` as its fallback node. `hold` has no edge into it: the compilers
/// emit the fallback only on the approval node.
fn ir(timeout_secs: Option<u64>, fallback: bool, policy: Value) -> Value {
    let step = |id: &str, function: &str| {
        json!({
            "id": id,
            "kind": { "type": "python_fn", "module": "myapp.release", "function": function, "output_schema": "" },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
        })
    };
    let mut nodes = json!({
        "review": {
            "id": "review",
            "kind": {
                "type": "human_approval",
                "description": "Ship release 1.4 to customers?",
                "timeout_secs": timeout_secs,
                "fallback_node": if fallback { json!("hold") } else { Value::Null }
            },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
        },
        "publish": step("publish", "publish"),
    });
    let mut edges = vec![
        json!({ "from": "review", "to": "publish", "condition": null }),
        json!({ "from": "publish", "to": "end", "condition": null }),
    ];
    if fallback {
        nodes["hold"] = step("hold", "hold");
        edges.push(json!({ "from": "hold", "to": "end", "condition": null }));
    }
    json!({
        "workflow_id": "wf-release", "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "review",
        "policy": policy,
        "nodes": nodes, "edges": edges,
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

/// Records which `python_fn` nodes ran: the stand-in for an external worker.
#[derive(Clone, Default)]
struct Ran(Arc<Mutex<Vec<String>>>);

#[async_trait::async_trait]
impl NodeExecutor for Ran {
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        self.0.lock().unwrap().push(item.node_id.clone());
        Ok(ExecutionResult {
            output: json!({ "ran": item.node_id }),
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

struct Env {
    name: &'static str,
    backend: Arc<dyn StateBackend>,
    app: axum::Router,
    alice: String,
    bob: String,
    engine: Worker,
    external: Worker,
    ran: Ran,
    scheduler: Scheduler,
}

/// The worker the server embeds for the `general` queue, with exactly the
/// server's in-process executors.
fn engine_worker(backend: Arc<dyn StateBackend>) -> Worker {
    jamjet_worker::server_pool(backend, Arc::new(jamjet_models::ModelRegistry::new()))
        .build_worker("engine".into(), vec!["general".into()])
}

async fn envs() -> Vec<Env> {
    let mut out = Vec::new();
    for tb in support::backends().await {
        let backend = tb.dyn_backend();
        let app = build_router_with_opts(support::make_state(backend.clone()), false);
        let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
        let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
        let ran = Ran::default();
        let external = Worker::new("py".into(), backend.clone(), vec!["python_tool".into()])
            .register_executor("python_fn", Arc::new(ran.clone()));
        out.push(Env {
            name: tb.name(),
            engine: engine_worker(backend.clone()),
            scheduler: Scheduler::new(backend.clone()),
            backend,
            app,
            alice,
            bob,
            external,
            ran,
        });
    }
    out
}

async fn call(env: &Env, method: &str, uri: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Register `ir` and start a run of it as alice.
async fn start(env: &Env, ir: Value) -> ExecutionId {
    let (st, body) = call(env, "POST", "/workflows", &env.alice, json!({ "ir": ir })).await;
    assert!(st.is_success(), "{}: register: {st} {body}", env.name);
    let (st, body) = call(
        env,
        "POST",
        "/executions",
        &env.alice,
        json!({ "workflow_id": "wf-release", "workflow_version": "0.1.0", "input": { "version": "1.4" } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{}: {body}", env.name);
    let id = body["execution_id"].as_str().unwrap();
    ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap())
}

/// Let the workers and the scheduler run until nothing changes.
async fn settle(env: &Env) {
    for _ in 0..8 {
        let _ = env.engine.poll_and_execute().await;
        let _ = env.external.poll_and_execute().await;
        env.scheduler.tick().await.unwrap();
    }
}

async fn events(env: &Env, eid: &ExecutionId) -> Vec<Event> {
    env.backend.get_events(eid).await.unwrap()
}

fn path(eid: &ExecutionId, tail: &str) -> String {
    format!("/executions/exec_{}{tail}", eid.0.simple())
}

fn ran(env: &Env) -> Vec<String> {
    env.ran.0.lock().unwrap().clone()
}

fn finished(events: &[Event]) -> Option<&'static str> {
    events.iter().find_map(|e| match e.kind {
        EventKind::WorkflowCompleted { .. } => Some("completed"),
        EventKind::WorkflowFailed { .. } => Some("failed"),
        _ => None,
    })
}

/// The run's one approval request for `review`, after it parked.
fn request(events: &[Event]) -> (Value, DateTime<Utc>) {
    let mut requests = events.iter().filter_map(|e| match &e.kind {
        EventKind::ToolApprovalRequired {
            node_id, context, ..
        } if node_id == "review" => Some((context.clone(), e.created_at)),
        _ => None,
    });
    let first = requests.next().expect("review raised an approval request");
    assert!(requests.next().is_none(), "exactly one request");
    first
}

/// Start the run and let it reach the approval. It must park there.
async fn park(env: &Env, ir: Value) -> (ExecutionId, Value) {
    let eid = start(env, ir).await;
    settle(env).await;
    let evs = events(env, &eid).await;
    assert_eq!(
        finished(&evs),
        None,
        "{}: the run must wait: {evs:#?}",
        env.name
    );
    assert!(
        ran(env).is_empty(),
        "{}: nothing after the approval ran",
        env.name
    );
    let (ctx, _) = request(&evs);
    (eid, ctx)
}

#[tokio::test]
async fn a_human_approval_node_parks_the_run_until_someone_decides() {
    for env in envs().await {
        let (eid, ctx) = park(&env, ir(None, false, Value::Null)).await;
        assert_eq!(
            ctx["description"], "Ship release 1.4 to customers?",
            "{}",
            env.name
        );
        assert!(
            ctx["action_digest"].is_string(),
            "{}: bound to the action",
            env.name
        );
        assert!(
            ctx.get("expires_at").is_none(),
            "{}: no deadline set",
            env.name
        );

        // It stays parked: more ticks change nothing.
        settle(&env).await;
        let evs = events(&env, &eid).await;
        assert_eq!(finished(&evs), None, "{}", env.name);
        assert!(
            !evs.iter().any(|e| matches!(&e.kind,
                EventKind::NodeCompleted { node_id, .. } if node_id == "review")),
            "{}: review must not complete unapproved",
            env.name
        );

        // The pending listing shows it.
        let (st, body) = call(
            &env,
            "GET",
            &path(&eid, "/approvals"),
            &env.bob,
            Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        assert!(body.to_string().contains("review"), "{}: {body}", env.name);
    }
}

#[tokio::test]
async fn approving_resumes_the_run_with_the_decision() {
    for env in envs().await {
        let (eid, ctx) = park(&env, ir(None, false, Value::Null)).await;
        let digest = ctx["action_digest"].as_str().unwrap();
        let approve = json!({
            "decision": "approved", "node_id": "review", "action_digest": digest, "comment": "ship it"
        });

        // Separation of duties: the initiator cannot approve their own run.
        let (st, body) = call(
            &env,
            "POST",
            &path(&eid, "/approve"),
            &env.alice,
            approve.clone(),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{}: {body}", env.name);

        let (st, body) = call(&env, "POST", &path(&eid, "/approve"), &env.bob, approve).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        settle(&env).await;

        let evs = events(&env, &eid).await;
        let output = evs
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::NodeCompleted {
                    node_id, output, ..
                } if node_id == "review" => Some(output.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{}: review completes: {evs:#?}", env.name));
        assert_eq!(output["decision"], "approved", "{}", env.name);
        assert_eq!(output["approved_by"], "bob", "{}", env.name);
        assert_eq!(output["comment"], "ship it", "{}", env.name);
        assert!(
            output["principal"]
                .as_str()
                .unwrap_or("")
                .starts_with("token:"),
            "{}: {output}",
            env.name
        );
        assert_eq!(ran(&env), vec!["publish".to_string()], "{}", env.name);
        assert_eq!(finished(&evs), Some("completed"), "{}", env.name);
    }
}

#[tokio::test]
async fn rejecting_fails_the_node_and_the_run() {
    for env in envs().await {
        let (eid, _) = park(&env, ir(None, false, Value::Null)).await;
        let (st, body) = call(
            &env,
            "POST",
            &path(&eid, "/approve"),
            &env.bob,
            json!({ "decision": "rejected", "node_id": "review", "comment": "not this week" }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        settle(&env).await;

        let evs = events(&env, &eid).await;
        assert!(
            evs.iter().any(|e| matches!(&e.kind,
                EventKind::NodeFailed { node_id, retryable: false, error, .. }
                    if node_id == "review" && error.contains("rejected by bob"))),
            "{}: {evs:#?}",
            env.name
        );
        assert!(ran(&env).is_empty(), "{}: publish never runs", env.name);
        assert_eq!(finished(&evs), Some("failed"), "{}", env.name);
    }
}

#[tokio::test]
async fn an_expired_request_routes_to_the_fallback_node() {
    for env in envs().await {
        let (eid, ctx) = park(&env, ir(Some(60), true, Value::Null)).await;
        let (_, raised) = request(&events(&env, &eid).await);
        let expires_at: DateTime<Utc> = ctx["expires_at"].as_str().unwrap().parse().unwrap();
        let window = (expires_at - raised).num_seconds();
        assert!(
            (59..=61).contains(&window),
            "{}: timeout_secs is the deadline: {window}s",
            env.name
        );

        env.scheduler
            .sweep_approval_deadlines(&eid, expires_at)
            .await
            .unwrap();
        settle(&env).await;

        let evs = events(&env, &eid).await;
        let output = evs
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::NodeCompleted {
                    node_id, output, ..
                } if node_id == "review" => Some(output.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{}: review completes on expiry: {evs:#?}", env.name));
        assert_eq!(output["decision"], "expired", "{}", env.name);
        assert_eq!(output["chosen_target"], "hold", "{}", env.name);
        // The fallback can read why it runs: the decision is in the state.
        let patch = evs
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::NodeCompleted {
                    node_id,
                    state_patch,
                    ..
                } if node_id == "review" => Some(state_patch.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(patch["review"]["approved"], false, "{}: {patch}", env.name);
        assert_eq!(patch["review"]["decision"], "expired", "{}", env.name);
        assert_eq!(
            ran(&env),
            vec!["hold".to_string()],
            "{}: only the fallback runs",
            env.name
        );
        assert!(
            evs.iter().any(|e| matches!(&e.kind,
                EventKind::NodeSkipped { node_id, .. } if node_id == "publish")),
            "{}: publish is skipped",
            env.name
        );
        assert_eq!(finished(&evs), Some("completed"), "{}", env.name);
    }
}

/// Approving a node that has a fallback runs the ordinary path; the fallback
/// is skipped, never run.
#[tokio::test]
async fn approving_a_node_with_a_fallback_skips_the_fallback() {
    for env in envs().await {
        let (eid, ctx) = park(&env, ir(Some(60), true, Value::Null)).await;
        let digest = ctx["action_digest"].as_str().unwrap();
        let (st, body) = call(
            &env,
            "POST",
            &path(&eid, "/approve"),
            &env.bob,
            json!({ "decision": "approved", "node_id": "review", "action_digest": digest }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        settle(&env).await;

        let evs = events(&env, &eid).await;
        assert_eq!(ran(&env), vec!["publish".to_string()], "{}", env.name);
        assert!(
            evs.iter().any(|e| matches!(&e.kind,
                EventKind::NodeSkipped { node_id, .. } if node_id == "hold")),
            "{}: the fallback is skipped: {evs:#?}",
            env.name
        );
        assert_eq!(finished(&evs), Some("completed"), "{}", env.name);
    }
}

#[tokio::test]
async fn an_expired_request_without_a_fallback_fails_the_node() {
    for env in envs().await {
        let (eid, ctx) = park(&env, ir(Some(60), false, Value::Null)).await;
        let expires_at: DateTime<Utc> = ctx["expires_at"].as_str().unwrap().parse().unwrap();
        env.scheduler
            .sweep_approval_deadlines(&eid, expires_at)
            .await
            .unwrap();
        settle(&env).await;

        let evs = events(&env, &eid).await;
        assert!(
            evs.iter().any(|e| matches!(&e.kind, EventKind::ApprovalExpired { node_id, .. } if node_id == "review")),
            "{}: {evs:#?}",
            env.name
        );
        assert!(
            evs.iter().any(|e| matches!(&e.kind,
                EventKind::NodeFailed { node_id, retryable: false, error, .. }
                    if node_id == "review" && error.contains("system:deadline"))),
            "{}: {evs:#?}",
            env.name
        );
        assert!(ran(&env).is_empty(), "{}", env.name);
        assert_eq!(finished(&evs), Some("failed"), "{}", env.name);
    }
}

/// `timeout_secs` is a ceiling next to the policy chain's deadline: the
/// shorter one applies, whichever sets it.
#[tokio::test]
async fn the_shorter_of_the_node_timeout_and_the_policy_deadline_applies() {
    for (node_timeout, policy_timeout, expected) in [(60, 3600, 60), (3600, 30, 30)] {
        for env in envs().await {
            let (eid, ctx) = park(
                &env,
                ir(
                    Some(node_timeout),
                    false,
                    json!({ "approval_timeout_secs": policy_timeout }),
                ),
            )
            .await;
            let (_, raised) = request(&events(&env, &eid).await);
            let expires_at: DateTime<Utc> = ctx["expires_at"].as_str().unwrap().parse().unwrap();
            let window = (expires_at - raised).num_seconds();
            assert!(
                (expected - 1..=expected + 1).contains(&window),
                "{}: node {node_timeout}s, policy {policy_timeout}s: got {window}s",
                env.name
            );
        }
    }
}

/// A fallback that is not a node of the workflow, or a timeout the engine
/// cannot honour, is refused at registration.
#[tokio::test]
async fn an_unusable_timeout_or_fallback_is_refused_at_registration() {
    for env in envs().await {
        let mut bad_fallback = ir(Some(60), true, Value::Null);
        bad_fallback["nodes"]["review"]["kind"]["fallback_node"] = json!("nowhere");
        let mut self_fallback = ir(Some(60), true, Value::Null);
        self_fallback["nodes"]["review"]["kind"]["fallback_node"] = json!("review");
        for bad in [ir(Some(0), false, Value::Null), bad_fallback, self_fallback] {
            let (st, body) =
                call(&env, "POST", "/workflows", &env.alice, json!({ "ir": bad })).await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{}: {body}", env.name);
            assert!(body.to_string().contains("review"), "{}: {body}", env.name);
        }
    }
}

/// A graph where the guarded step is also a join:
///
/// ```text
/// review (human_approval, fallback: hold) ─┐
///                                          ├─> publish ─┬─> deploy
/// prep ────────────────────────────────────┴────────────┘
/// publish ─> notify <─ hold
/// ```
///
/// `prep` is a second root, so `publish` and `deploy` each have a live
/// in-edge that does not pass through the approval.
fn join_ir() -> Value {
    let step = |id: &str| {
        json!({
            "id": id,
            "kind": { "type": "python_fn", "module": "myapp.release", "function": id, "output_schema": "" },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
        })
    };
    let edge = |from: &str, to: &str| json!({ "from": from, "to": to, "condition": null });
    json!({
        "workflow_id": "wf-release", "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "review",
        "nodes": {
            "review": {
                "id": "review",
                "kind": { "type": "human_approval", "description": "Ship?", "timeout_secs": 60, "fallback_node": "hold" },
                "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
            },
            "prep": step("prep"),
            "publish": step("publish"),
            "deploy": step("deploy"),
            "hold": step("hold"),
            "notify": step("notify"),
        },
        "edges": [
            edge("review", "publish"), edge("prep", "publish"),
            edge("publish", "deploy"), edge("prep", "deploy"),
            edge("publish", "notify"), edge("hold", "notify"),
            edge("deploy", "end"), edge("notify", "end")
        ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// Review finding (PR #169): after an expiry, a guarded step that is also a
/// join ran on its other live in-edge, with no approval. The whole path the
/// approval guarded is skipped; a node the fallback also leads to still runs.
#[tokio::test]
async fn an_expiry_never_runs_the_guarded_path_through_a_join() {
    for env in envs().await {
        let eid = start(&env, join_ir()).await;
        settle(&env).await;
        assert_eq!(
            ran(&env),
            vec!["prep".to_string()],
            "{}: only prep before the decision",
            env.name
        );
        let (ctx, _) = request(&events(&env, &eid).await);
        let expires_at: DateTime<Utc> = ctx["expires_at"].as_str().unwrap().parse().unwrap();
        env.scheduler
            .sweep_approval_deadlines(&eid, expires_at)
            .await
            .unwrap();
        settle(&env).await;

        let evs = events(&env, &eid).await;
        assert_eq!(
            sorted(ran(&env)),
            vec!["hold".to_string(), "notify".to_string(), "prep".to_string()],
            "{}: {evs:#?}",
            env.name
        );
        for guarded in ["publish", "deploy"] {
            assert!(
                evs.iter().any(|e| matches!(&e.kind,
                    EventKind::NodeSkipped { node_id, .. } if node_id == guarded)),
                "{}: {guarded} is skipped",
                env.name
            );
        }
        assert_eq!(finished(&evs), Some("completed"), "{}", env.name);
    }
}

/// The same graph, approved: the guarded path runs and the fallback does not.
#[tokio::test]
async fn an_approval_runs_the_guarded_path_of_a_join() {
    for env in envs().await {
        let eid = start(&env, join_ir()).await;
        settle(&env).await;
        let (ctx, _) = request(&events(&env, &eid).await);
        let digest = ctx["action_digest"].as_str().unwrap();
        let (st, body) = call(
            &env,
            "POST",
            &path(&eid, "/approve"),
            &env.bob,
            json!({ "decision": "approved", "node_id": "review", "action_digest": digest }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        settle(&env).await;
        let evs = events(&env, &eid).await;
        assert_eq!(
            sorted(ran(&env)),
            vec![
                "deploy".to_string(),
                "notify".to_string(),
                "prep".to_string(),
                "publish".to_string()
            ],
            "{}: {evs:#?}",
            env.name
        );
        assert_eq!(finished(&evs), Some("completed"), "{}", env.name);
    }
}

/// Review finding (PR #169): a fallback upstream of the approval made the
/// approval and the fallback wait on each other, and the run ended
/// "completed" without anyone being asked. Registration refuses it.
#[tokio::test]
async fn a_fallback_upstream_of_the_approval_is_refused() {
    for env in envs().await {
        let mut ir = ir(Some(60), true, Value::Null);
        // hold -> review, so review is reachable from its own fallback.
        ir["start_node"] = json!("hold");
        ir["edges"] = json!([
            { "from": "hold", "to": "review", "condition": null },
            { "from": "review", "to": "publish", "condition": null },
            { "from": "publish", "to": "end", "condition": null }
        ]);
        let (st, body) = call(&env, "POST", "/workflows", &env.alice, json!({ "ir": ir })).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{}: {body}", env.name);
        assert!(
            body.to_string().contains("leads back"),
            "{}: {body}",
            env.name
        );
    }
}

/// A version stored before that check: the unusable fallback counts as none,
/// so the run neither deadlocks nor reports success. It waits for a decision,
/// and an expiry fails the node.
#[tokio::test]
async fn a_stored_upstream_fallback_is_ignored_and_an_expiry_fails() {
    for env in envs().await {
        let mut ir = ir(Some(60), true, Value::Null);
        ir["start_node"] = json!("hold");
        ir["edges"] = json!([
            { "from": "hold", "to": "review", "condition": null },
            { "from": "review", "to": "publish", "condition": null },
            { "from": "publish", "to": "end", "condition": null }
        ]);
        support::store_ir(env.backend.as_ref(), &ir).await;
        let (st, body) = call(
            &env,
            "POST",
            "/executions",
            &env.alice,
            json!({ "workflow_id": "wf-release", "workflow_version": "0.1.0", "input": {} }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{}: {body}", env.name);
        let id = body["execution_id"].as_str().unwrap();
        let eid = ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap());
        settle(&env).await;
        let evs = events(&env, &eid).await;
        assert_eq!(
            finished(&evs),
            None,
            "{}: must wait, not finish: {evs:#?}",
            env.name
        );
        let (ctx, _) = request(&evs);
        let expires_at: DateTime<Utc> = ctx["expires_at"].as_str().unwrap().parse().unwrap();
        env.scheduler
            .sweep_approval_deadlines(&eid, expires_at)
            .await
            .unwrap();
        settle(&env).await;
        let evs = events(&env, &eid).await;
        assert_eq!(ran(&env), vec!["hold".to_string()], "{}", env.name);
        assert_eq!(finished(&evs), Some("failed"), "{}: {evs:#?}", env.name);
    }
}

/// Review finding (PR #169): the route of a `human_approval` node was read
/// from a completion's output. A `chosen_target` in a reported result must
/// not send the run to the fallback.
#[tokio::test]
async fn a_route_in_a_completion_output_cannot_pick_the_fallback() {
    for env in envs().await {
        let (eid, ctx) = park(&env, ir(Some(60), true, Value::Null)).await;
        let digest = ctx["action_digest"].as_str().unwrap();
        let (st, body) = call(
            &env,
            "POST",
            &path(&eid, "/approve"),
            &env.bob,
            json!({ "decision": "approved", "node_id": "review", "action_digest": digest }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        env.scheduler.tick().await.unwrap();

        // A completion that names a route, written the way any reported
        // result is (`commit_turn`). External workers can no longer claim or
        // complete this item (see human_approval_binding.rs); this checks the
        // scheduler does not route on the output even if one gets written.
        let item = env
            .backend
            .claim_work_item("ext", &["general"])
            .await
            .unwrap()
            .expect("the approved item is re-dispatched");
        env.backend
            .commit_turn(
                item.id,
                item.lease_fence,
                Event::new(
                    eid.clone(),
                    0,
                    EventKind::NodeCompleted {
                        node_id: "review".into(),
                        output: json!({ "chosen_target": "hold" }),
                        state_patch: json!({}),
                        duration_ms: 0,
                        gen_ai_system: None,
                        gen_ai_model: None,
                        input_tokens: None,
                        output_tokens: None,
                        finish_reason: None,
                        cost_usd: None,
                        provenance: None,
                        idempotency_key: None,
                    },
                ),
                true,
            )
            .await
            .unwrap();
        settle(&env).await;

        let evs = events(&env, &eid).await;
        assert_eq!(
            ran(&env),
            vec!["publish".to_string()],
            "{}: {evs:#?}",
            env.name
        );
        assert_eq!(finished(&evs), Some("completed"), "{}", env.name);
    }
}

/// `fallback: end` ends the run on expiry: the guarded steps are skipped and
/// nothing runs in their place.
#[tokio::test]
async fn an_end_fallback_stops_the_run_on_expiry() {
    for env in envs().await {
        let mut ir = ir(Some(60), false, Value::Null);
        ir["nodes"]["review"]["kind"]["fallback_node"] = json!("end");
        let (eid, ctx) = park(&env, ir).await;
        let expires_at: DateTime<Utc> = ctx["expires_at"].as_str().unwrap().parse().unwrap();
        env.scheduler
            .sweep_approval_deadlines(&eid, expires_at)
            .await
            .unwrap();
        settle(&env).await;
        let evs = events(&env, &eid).await;
        assert!(ran(&env).is_empty(), "{}: {evs:#?}", env.name);
        assert!(
            evs.iter().any(|e| matches!(&e.kind,
                EventKind::NodeSkipped { node_id, .. } if node_id == "publish")),
            "{}: publish is skipped",
            env.name
        );
        assert_eq!(finished(&evs), Some("completed"), "{}", env.name);
    }
}

/// A version stored with a fallback registration now refuses still
/// re-registers unchanged (deployments re-register on boot); the scheduler
/// treats that fallback as none.
#[tokio::test]
async fn a_stored_version_with_an_upstream_fallback_still_re_registers() {
    for env in envs().await {
        let mut ir = ir(Some(60), true, Value::Null);
        ir["start_node"] = json!("hold");
        ir["edges"] = json!([
            { "from": "hold", "to": "review", "condition": null },
            { "from": "review", "to": "publish", "condition": null },
            { "from": "publish", "to": "end", "condition": null }
        ]);
        support::store_ir(env.backend.as_ref(), &ir).await;
        let (st, body) = call(&env, "POST", "/workflows", &env.alice, json!({ "ir": ir })).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
    }
}
