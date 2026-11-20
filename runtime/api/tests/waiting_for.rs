//! What a live run is waiting for.
//!
//! `GET /executions/:id` says whether a run waits on a human (`approval`), on
//! a step a worker holds (`step`), on a worker to claim its next step
//! (`worker`) or on the engine itself (`engine`), and null once it has ended.
//! A caller that starts workers on demand reads it to decide whether one is
//! needed. `GET /executions?waiting_for=<v>` lists the running runs in one
//! state, derived the same way.
//!
//! A missing key and a null one read the same through `body["waiting_for"]`,
//! so every check here reads the key with `.get` and fails when it is absent.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::Event;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use tower::ServiceExt;

mod support;

fn workflow(workflow_id: &str, start: &str, nodes: Value, edges: Value) -> Value {
    json!({
        "workflow_id": workflow_id, "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": start,
        "nodes": nodes, "edges": edges,
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

fn node(id: &str, kind: Value, policy: Value) -> Value {
    json!({
        "id": id, "kind": kind, "retry_policy": null, "node_timeout_secs": null,
        "description": null, "labels": {}, "policy": policy
    })
}

fn python_fn(function: &str) -> Value {
    json!({ "type": "python_fn", "module": "myapp.steps", "function": function, "output_schema": "" })
}

/// A node policy that gates the node, with deadline settings merged in.
fn gate(extra: Value) -> Value {
    let mut p = json!({ "require_approval_for": ["*"] });
    for (k, v) in extra.as_object().unwrap() {
        p[k] = v.clone();
    }
    p
}

/// One `python_fn` step, `pay`, under `policy`.
fn gated_ir(workflow_id: &str, policy: Value) -> Value {
    workflow(
        workflow_id,
        "pay",
        json!({ "pay": node("pay", python_fn("transfer"), policy) }),
        json!([{ "from": "pay", "to": "end", "condition": null }]),
    )
}

/// One `model` step, which the engine runs itself.
fn model_ir() -> Value {
    workflow(
        "wf-model",
        "think",
        json!({ "think": node(
            "think",
            json!({ "type": "model", "model_ref": "m", "prompt_ref": "", "output_schema": "" }),
            Value::Null,
        ) }),
        json!([{ "from": "think", "to": "end", "condition": null }]),
    )
}

/// A first step, then two branches side by side: `pay`, gated, and `side`,
/// an `external_fn` step on a queue of its own.
fn fan_ir() -> Value {
    workflow(
        "wf-fan",
        "first",
        json!({
            "first": node("first", python_fn("prepare"), Value::Null),
            "pay": node("pay", python_fn("transfer"), gate(json!({}))),
            "side": node(
                "side",
                json!({ "type": "external_fn", "queue": "side-jobs", "handler": "h", "output_schema": "" }),
                Value::Null,
            ),
        }),
        json!([
            { "from": "first", "to": "pay", "condition": null },
            { "from": "first", "to": "side", "condition": null },
            { "from": "pay", "to": "end", "condition": null },
            { "from": "side", "to": "end", "condition": null }
        ]),
    )
}

struct Env {
    name: &'static str,
    tb: support::TestBackend,
    backend: Arc<dyn StateBackend>,
    app: axum::Router,
    /// Starts the runs and plays their worker.
    alice: String,
    /// Reviewer.
    bob: String,
}

/// One environment per backend, with every workflow of this file registered.
async fn envs() -> Vec<Env> {
    let mut out = Vec::new();
    for tb in support::backends().await {
        let backend = tb.dyn_backend();
        let app = build_router_with_opts(support::make_state(backend.clone()), false);
        let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
        let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
        let env = Env {
            name: tb.name(),
            tb,
            backend,
            app,
            alice,
            bob,
        };
        for ir in [
            gated_ir("wf-gate", gate(json!({}))),
            gated_ir("wf-deadline", gate(json!({ "approval_timeout_secs": 600 }))),
            support::python_fn_start_ir("wf-plain"),
            model_ir(),
            fan_ir(),
        ] {
            let (st, body) = call(
                &env.app,
                "POST",
                "/workflows",
                &env.alice,
                Some(json!({ "ir": ir })),
            )
            .await;
            assert!(st.is_success(), "{}: {st} {body}", env.name);
        }
        out.push(env);
    }
    out
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"));
    let req = match body {
        Some(b) => req.body(Body::from(b.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn exec_path(eid: &ExecutionId) -> String {
    format!("exec_{}", eid.0.simple())
}

/// An execution id in either form the API writes (`exec_<simple>` or a
/// hyphenated uuid).
fn id_of(v: &Value) -> ExecutionId {
    let s = v.as_str().unwrap_or_else(|| panic!("not an id: {v}"));
    ExecutionId(uuid::Uuid::parse_str(s.strip_prefix("exec_").unwrap_or(s)).unwrap())
}

async fn start(env: &Env, workflow_id: &str) -> ExecutionId {
    let (st, body) = call(
        &env.app,
        "POST",
        "/executions",
        &env.alice,
        Some(json!({ "workflow_id": workflow_id, "workflow_version": "0.1.0", "input": { "amount": 5 } })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{}: {body}", env.name);
    id_of(&body["execution_id"])
}

/// One claim on the `python_tool` queue, as alice.
async fn claim(env: &Env) -> Value {
    let (st, body) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        Some(json!({ "worker_id": "w", "queue_types": ["python_tool"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
    body
}

/// Claim the run's gated step, which raises its approval request.
async fn raise_request(env: &Env) {
    let claimed = claim(env).await;
    assert_eq!(
        claimed,
        json!({ "claimed": false }),
        "{}: a gated step is withheld until approved",
        env.name
    );
}

/// Claim the next step, which must be handed out.
async fn take_step(env: &Env) -> Value {
    let claimed = claim(env).await;
    assert_eq!(claimed["claimed"], true, "{}: {claimed}", env.name);
    claimed
}

async fn complete(env: &Env, claimed: &Value) {
    let item = &claimed["work_item"];
    let (st, body) = call(
        &env.app,
        "POST",
        &format!("/work-items/{}/complete", item["id"].as_str().unwrap()),
        &env.alice,
        Some(json!({ "output": {}, "state_patch": {}, "lease_fence": item["lease_fence"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
}

async fn scheduler_pass(env: &Env) {
    Scheduler::new(env.backend.clone()).tick().await.unwrap();
}

async fn status_of(env: &Env, eid: &ExecutionId) -> WorkflowStatus {
    env.backend
        .get_execution(eid)
        .await
        .unwrap()
        .unwrap()
        .status
}

/// `waiting_for` from `GET /executions/:id`. The key must be present.
async fn waiting_for(env: &Env, eid: &ExecutionId) -> Value {
    let (st, body) = call(
        &env.app,
        "GET",
        &format!("/executions/{}", exec_path(eid)),
        &env.alice,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
    body.get("waiting_for")
        .cloned()
        .unwrap_or_else(|| panic!("{}: no waiting_for key: {body}", env.name))
}

async fn events(env: &Env, eid: &ExecutionId) -> Vec<Event> {
    env.backend.get_events(eid).await.unwrap()
}

/// The latest request's context.
fn request_context(events: &[Event]) -> Value {
    events
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired { context, .. } => Some(context.clone()),
            _ => None,
        })
        .expect("an approval request")
}

#[tokio::test]
async fn waiting_for_is_approval_while_a_request_is_open() {
    for env in envs().await {
        let eid = start(&env, "wf-gate").await;
        raise_request(&env).await;
        assert_eq!(waiting_for(&env, &eid).await, "approval", "{}", env.name);
    }
}

#[tokio::test]
async fn waiting_for_is_worker_for_a_pending_python_fn_item() {
    for env in envs().await {
        let eid = start(&env, "wf-plain").await;
        assert_eq!(waiting_for(&env, &eid).await, "worker", "{}", env.name);
    }
}

#[tokio::test]
async fn waiting_for_is_step_while_a_lease_is_held() {
    for env in envs().await {
        let eid = start(&env, "wf-plain").await;
        take_step(&env).await;
        assert_eq!(waiting_for(&env, &eid).await, "step", "{}", env.name);
    }
}

#[tokio::test]
async fn waiting_for_is_engine_for_a_model_item_and_between_passes() {
    for env in envs().await {
        // A model step sits on an engine queue: no external worker takes it.
        let eid = start(&env, "wf-model").await;
        assert_eq!(waiting_for(&env, &eid).await, "engine", "{}", env.name);

        // Right after an approval nothing is queued until the scheduler's
        // next pass puts the approved step back.
        let eid = start(&env, "wf-gate").await;
        raise_request(&env).await;
        let digest = request_context(&events(&env, &eid).await)["action_digest"].clone();
        let (st, body) = call(
            &env.app,
            "POST",
            &format!("/executions/{}/approve", exec_path(&eid)),
            &env.bob,
            Some(json!({ "decision": "approved", "node_id": "pay", "action_digest": digest })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        assert_eq!(waiting_for(&env, &eid).await, "engine", "{}", env.name);

        scheduler_pass(&env).await;
        assert_eq!(waiting_for(&env, &eid).await, "worker", "{}", env.name);
    }
}

#[tokio::test]
async fn waiting_for_is_null_on_a_finished_run() {
    for env in envs().await {
        let done = start(&env, "wf-plain").await;
        let claimed = take_step(&env).await;
        complete(&env, &claimed).await;
        for _ in 0..5 {
            if status_of(&env, &done).await.is_terminal() {
                break;
            }
            scheduler_pass(&env).await;
        }
        assert_eq!(
            status_of(&env, &done).await,
            WorkflowStatus::Completed,
            "{}",
            env.name
        );
        assert_eq!(waiting_for(&env, &done).await, Value::Null, "{}", env.name);

        let cancelled = start(&env, "wf-plain").await;
        let (st, body) = call(
            &env.app,
            "POST",
            &format!("/executions/{}/cancel", exec_path(&cancelled)),
            &env.alice,
            Some(json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        assert_eq!(
            waiting_for(&env, &cancelled).await,
            Value::Null,
            "{}",
            env.name
        );
    }
}

/// A claimed item whose lease has run out is claimable again, so the run
/// waits for a worker, not on the one that let it lapse.
#[tokio::test]
async fn a_lapsed_lease_reads_as_worker() {
    for env in envs().await {
        let eid = start(&env, "wf-plain").await;
        let claimed = take_step(&env).await;
        assert_eq!(waiting_for(&env, &eid).await, "step", "{}", env.name);

        let item = uuid::Uuid::parse_str(claimed["work_item"]["id"].as_str().unwrap()).unwrap();
        env.tb.expire_lease(item).await;
        assert_eq!(waiting_for(&env, &eid).await, "worker", "{}", env.name);
    }
}

/// A request past its deadline can no longer be decided. Until the sweep
/// records the expiry, the run waits on the engine, not on a reviewer.
#[tokio::test]
async fn an_expired_unswept_request_reads_as_engine() {
    for env in envs().await {
        let eid = start(&env, "wf-deadline").await;
        raise_request(&env).await;
        assert_eq!(waiting_for(&env, &eid).await, "approval", "{}", env.name);

        // Re-raise the request as if it had been made long ago, with the same
        // binding: its deadline has passed and no sweep has run.
        let mut ctx = request_context(&events(&env, &eid).await);
        ctx["expires_at"] = json!((Utc::now() - Duration::seconds(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        env.backend
            .append_event(Event::new(
                eid.clone(),
                0,
                EventKind::ToolApprovalRequired {
                    node_id: "pay".into(),
                    tool_name: "pay".into(),
                    approver: "human".into(),
                    context: ctx,
                },
            ))
            .await
            .unwrap();
        assert_eq!(waiting_for(&env, &eid).await, "engine", "{}", env.name);
    }
}

#[tokio::test]
async fn parallel_branches_report_the_first_match() {
    for env in envs().await {
        let eid = start(&env, "wf-fan").await;
        let first = take_step(&env).await;
        assert_eq!(first["work_item"]["node_id"], "first", "{}", env.name);
        complete(&env, &first).await;
        scheduler_pass(&env).await;
        // `pay` asks for approval; `side` waits on its own queue.
        raise_request(&env).await;
        let evs = events(&env, &eid).await;
        assert!(
            evs.iter().any(|e| matches!(&e.kind,
                EventKind::NodeScheduled { node_id, .. } if node_id == "side")),
            "{}: side is dispatched: {evs:#?}",
            env.name
        );
        assert!(
            !evs.iter().any(|e| matches!(&e.kind,
                EventKind::NodeCompleted { node_id, .. } if node_id == "side")),
            "{}",
            env.name
        );

        assert_eq!(waiting_for(&env, &eid).await, "approval", "{}", env.name);
    }
}

#[tokio::test]
async fn the_filtered_list_and_the_single_read_agree() {
    for env in envs().await {
        // One run in each state, set up in an order where each claim can
        // only take the step it is meant for.
        let approval = start(&env, "wf-gate").await;
        raise_request(&env).await;
        let step = start(&env, "wf-plain").await;
        take_step(&env).await;
        let worker = start(&env, "wf-plain").await;
        let engine = start(&env, "wf-model").await;
        let finished = start(&env, "wf-plain").await;
        let (st, body) = call(
            &env.app,
            "POST",
            &format!("/executions/{}/cancel", exec_path(&finished)),
            &env.alice,
            Some(json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);

        for (state, eid) in [
            ("approval", &approval),
            ("step", &step),
            ("worker", &worker),
            ("engine", &engine),
        ] {
            assert_eq!(waiting_for(&env, eid).await, state, "{}", env.name);

            let (st, page) = call(
                &env.app,
                "GET",
                &format!("/executions?waiting_for={state}"),
                &env.alice,
                None,
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{}: {state}: {page}", env.name);
            let listed = page["executions"]
                .as_array()
                .unwrap_or_else(|| panic!("{}: {state}: {page}", env.name));
            let ids: BTreeSet<uuid::Uuid> =
                listed.iter().map(|e| id_of(&e["execution_id"]).0).collect();
            assert_eq!(
                ids,
                BTreeSet::from([eid.0]),
                "{}: {state} lists exactly its run: {page}",
                env.name
            );
            for entry in listed {
                assert_eq!(
                    entry.get("waiting_for"),
                    Some(&json!(state)),
                    "{}: {entry}",
                    env.name
                );
                assert_eq!(
                    waiting_for(&env, &id_of(&entry["execution_id"])).await,
                    state,
                    "{}: the single read agrees",
                    env.name
                );
            }
        }
    }
}

#[tokio::test]
async fn a_waiting_for_filter_with_status_completed_is_400() {
    for env in envs().await {
        // Only running runs wait for anything, and the filter pages by
        // cursor, not by offset. A value outside the four is refused too.
        for query in [
            "?waiting_for=approval&status=completed",
            "?waiting_for=worker&status=failed",
            "?waiting_for=step&offset=0",
            "?waiting_for=soon",
        ] {
            let (st, body) = call(
                &env.app,
                "GET",
                &format!("/executions{query}"),
                &env.alice,
                None,
            )
            .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{}: {query}: {body}", env.name);
            assert!(body["error"].is_string(), "{}: {query}: {body}", env.name);
        }

        let (st, body) = call(
            &env.app,
            "GET",
            "/executions?waiting_for=approval&status=running",
            &env.alice,
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        assert!(body["executions"].is_array(), "{}: {body}", env.name);
    }
}
