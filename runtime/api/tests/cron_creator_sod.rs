//! The creator of a cron job is the initiator of every run it starts.
//!
//! The person who chooses the workflow, the input and the schedule started the
//! run as surely as one who POSTs `/executions`. Recording the runs as started
//! by a system principal let that person approve their own gated action
//! (review C2). The job now records the token that created it, the run records
//! that token as its initiator, and separation of duties refuses it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::cron::InProcessCronLauncher;
use jamjet_api::routes::build_router_with_opts;
use jamjet_api::state::AppState;
use jamjet_core::workflow::ExecutionId;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::SqliteBackend;
use jamjet_timers::{CronScheduler, CronStore};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

mod support;

fn gated_ir() -> Value {
    json!({
        "workflow_id": "wf-cron-gate",
        "version": "0.1.0",
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": "gate",
        "nodes": {
            "gate": {
                "id": "gate",
                "kind": {
                    "type": "tool",
                    "tool_ref": "payments.transfer",
                    "input_mapping": {},
                    "output_schema": ""
                },
                "retry_policy": null,
                "node_timeout_secs": null,
                "description": null,
                "labels": {},
                "policy": { "blocked_tools": [], "require_approval_for": ["payments.*"], "model_allowlist": [] }
            }
        },
        "edges": [ { "from": "gate", "to": "end", "condition": null } ],
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {}
    })
}

struct Env {
    sqlite: Arc<SqliteBackend>,
    state: AppState,
    app: axum::Router,
    alice: String,
    bob: String,
}

async fn env() -> Env {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "jamjet_cron_sod_{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let sqlite = Arc::new(
        SqliteBackend::open(&format!("sqlite://{}", path.display()))
            .await
            .unwrap(),
    );
    let mut state = support::make_state(sqlite.clone() as Arc<dyn StateBackend>);
    state.cron_store = Some(Arc::new(CronStore::new(sqlite.pool())));
    let app = build_router_with_opts(state.clone(), false);
    let (alice, _) = sqlite.create_token("alice", "developer").await.unwrap();
    let (bob, _) = sqlite.create_token("bob", "reviewer").await.unwrap();
    Env {
        sqlite,
        state,
        app,
        alice,
        bob,
    }
}

async fn send(
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

/// Make every stored job due now and run one scheduler tick.
async fn fire_due_jobs(env: &Env) {
    let store = CronStore::new(env.sqlite.pool());
    for mut job in store.list_all().await.unwrap() {
        job.next_run_at = chrono::Utc::now() - chrono::Duration::minutes(1);
        store.upsert(&job).await.unwrap();
    }
    CronScheduler::with_launcher(
        env.sqlite.pool(),
        Arc::new(InProcessCronLauncher::new(env.state.clone())),
    )
    .tick()
    .await
    .unwrap();
}

/// Alice registers the gated workflow and schedules it. Returns the cron run,
/// held at the gate, and the digest of its pending request.
async fn held_cron_run(env: &Env) -> (ExecutionId, String) {
    let (st, body) = send(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        Some(json!({ "ir": gated_ir() })),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let (st, body) = send(
        &env.app,
        "POST",
        "/cron",
        &env.alice,
        Some(json!({
            "name": "nightly-payout",
            "cron_expression": "0 2 * * *",
            "workflow_id": "wf-cron-gate",
            "workflow_version": "0.1.0",
            "input": { "amount": 1_000_000 },
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    fire_due_jobs(env).await;

    let runs = env.sqlite.list_executions(None, 10, 0).await.unwrap();
    assert_eq!(runs.len(), 1, "the due job started one run");
    let eid = runs[0].execution_id.clone();

    let (st, body) = send(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        Some(json!({ "worker_id": "w", "queue_types": ["tool", "general"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "claimed": false }), "the gate was handed out");

    let (st, view) = send(
        &env.app,
        "GET",
        &format!("/executions/{eid}/approvals"),
        &env.bob,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{view}");
    let digest = view["pending"][0]["context"]["action_digest"]
        .as_str()
        .expect("the pending request carries a digest")
        .to_string();
    (eid, digest)
}

fn approvals(events: &[jamjet_state::Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ApprovalReceived { .. }))
        .count()
}

/// The reviewer's demonstration: the job's creator approves the run it started.
#[tokio::test]
async fn the_creator_of_a_cron_job_cannot_approve_its_run() {
    let env = env().await;
    let (eid, digest) = held_cron_run(&env).await;

    let (st, body) = send(
        &env.app,
        "POST",
        &format!("/executions/{eid}/approve"),
        &env.alice,
        Some(json!({ "decision": "approved", "node_id": "gate", "action_digest": digest })),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(approvals(&env.sqlite.get_events(&eid).await.unwrap()), 0);

    // Someone else can.
    let (st, body) = send(
        &env.app,
        "POST",
        &format!("/executions/{eid}/approve"),
        &env.bob,
        Some(json!({ "decision": "approved", "node_id": "gate", "action_digest": digest })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
}

/// The run records the job's creator, the token principal, as its initiator.
#[tokio::test]
async fn a_cron_run_records_the_job_creator_as_initiator() {
    let env = env().await;
    let (eid, _) = held_cron_run(&env).await;
    let alice = env
        .sqlite
        .validate_token(&env.alice)
        .await
        .unwrap()
        .unwrap();
    let events = env.sqlite.get_events(&eid).await.unwrap();
    assert_eq!(
        jamjet_state::approvals::run_initiator(&events),
        Some(jamjet_api::auth::principal_of(&alice)),
    );
    let jobs = CronStore::new(env.sqlite.pool()).list_all().await.unwrap();
    assert_eq!(
        jobs[0].created_by.as_deref(),
        Some(jamjet_api::auth::principal_of(&alice).as_str())
    );
}
