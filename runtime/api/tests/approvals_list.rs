//! `GET /approvals`: every pending approval, across runs, in one list.
//!
//! A reviewer could only list approvals one run at a time
//! (`GET /executions/:id/approvals`), so finding what waits meant walking every
//! run. The list reads the approvals read model, which the projector brings up
//! to date once per tick, so a row can lag the log by up to one tick. The
//! approve route still decides.
//!
//! Before the route existed, `GET /approvals` matched nothing and the router's
//! fallback answered the web page with 200. Every call here therefore checks
//! for a JSON body with an `approvals` array, never the status alone.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_scheduler::Projector;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::{Event, SqliteBackend, Tenant, TenantId, TenantStatus};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;
use tower::ServiceExt;

mod support;

/// A one-node workflow whose `python_fn` node `pay` waits for approval.
fn ir(workflow_id: &str, node_policy: Value) -> Value {
    json!({
        "workflow_id": workflow_id, "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "pay",
        "nodes": { "pay": {
            "id": "pay",
            "kind": { "type": "python_fn", "module": "myapp.payments", "function": "transfer", "output_schema": "" },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {},
            "policy": node_policy
        } },
        "edges": [ { "from": "pay", "to": "end", "condition": null } ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

/// A node policy that gates `pay`, with the given deadline settings merged in.
fn gate(extra: Value) -> Value {
    let mut p = json!({ "require_approval_for": ["*"] });
    for (k, v) in extra.as_object().unwrap() {
        p[k] = v.clone();
    }
    p
}

struct Env {
    name: &'static str,
    backend: Arc<dyn StateBackend>,
    app: axum::Router,
    /// Starts the runs and plays their worker.
    alice: String,
    /// Reviewers.
    bob: String,
    carol: String,
    /// May read, may not decide.
    vic: String,
}

async fn env_on(name: &'static str, backend: Arc<dyn StateBackend>) -> Env {
    let state = support::make_state(backend.clone());
    let app = build_router_with_opts(state, false);
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
    let (carol, _) = backend.create_token("carol", "reviewer").await.unwrap();
    let (vic, _) = backend.create_token("vic", "viewer").await.unwrap();
    Env {
        name,
        backend,
        app,
        alice,
        bob,
        carol,
        vic,
    }
}

/// One environment per backend: in-memory and SQLite.
async fn envs() -> Vec<Env> {
    let mut out = Vec::new();
    for tb in support::backends().await {
        out.push(env_on(tb.name(), tb.dyn_backend()).await);
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

async fn register(env: &Env, ir: Value) {
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

/// Start a run of `workflow_id` as alice.
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
    let id = body["execution_id"].as_str().unwrap();
    ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap())
}

/// Start a run and claim its gated step, which raises the approval request.
async fn start_and_request(env: &Env, workflow_id: &str) -> ExecutionId {
    let eid = start(env, workflow_id).await;
    let (st, claimed) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        Some(json!({ "worker_id": "w", "queue_types": ["python_tool"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{}: {claimed}", env.name);
    assert_eq!(
        claimed,
        json!({ "claimed": false }),
        "{}: a gated step is withheld until approved",
        env.name
    );
    eid
}

/// One projector pass: the read model catches up with the log.
async fn project(backend: &Arc<dyn StateBackend>) -> usize {
    Projector::new(backend.clone()).tick().await.unwrap()
}

/// One page of the list, as `token`. A JSON answer with an `approvals` array
/// is required: a path no route matches answers the web page with 200.
async fn list(env: &Env, token: &str, query: &str) -> Value {
    let (st, page) = call(&env.app, "GET", &format!("/approvals{query}"), token, None).await;
    assert_eq!(st, StatusCode::OK, "{}: {page}", env.name);
    assert!(
        page["approvals"].is_array(),
        "{}: the answer is not a list of approvals: {page}",
        env.name
    );
    page
}

fn rows(page: &Value) -> Vec<Value> {
    page["approvals"].as_array().unwrap().clone()
}

/// `(execution_id, node_id)` of every row.
fn keys(rows: &[Value]) -> BTreeSet<(String, String)> {
    rows.iter()
        .map(|r| {
            (
                r["execution_id"].as_str().unwrap().to_string(),
                r["node_id"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The latest request for `node`: its sequence, tool name, approver and
/// context, as the log holds them.
fn request_of(events: &[Event], node: &str) -> (i64, String, String, Value) {
    events
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired {
                node_id,
                tool_name,
                approver,
                context,
            } if node_id == node => Some((
                e.sequence,
                tool_name.clone(),
                approver.clone(),
                context.clone(),
            )),
            _ => None,
        })
        .expect("an approval request")
}

async fn digest_of(env: &Env, eid: &ExecutionId) -> Value {
    let events = env.backend.get_events(eid).await.unwrap();
    let digest = request_of(&events, "pay").3["action_digest"].clone();
    assert!(
        digest.is_string(),
        "{}: the request binds a digest",
        env.name
    );
    digest
}

async fn decide(env: &Env, token: &str, id: &str, digest: &Value) -> (StatusCode, Value) {
    call(
        &env.app,
        "POST",
        &format!("/executions/{id}/approve"),
        token,
        Some(json!({ "decision": "approved", "node_id": "pay", "action_digest": digest })),
    )
    .await
}

fn decisions(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ApprovalReceived { .. }))
        .count()
}

/// A live run, written at store level, with one pending request per node.
async fn seed_requests(
    backend: &Arc<dyn StateBackend>,
    workflow_id: &str,
    nodes: &[String],
) -> ExecutionId {
    let eid = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: eid.clone(),
            workflow_id: workflow_id.into(),
            workflow_version: "0.1.0".into(),
            status: WorkflowStatus::Running,
            initial_input: json!({}),
            current_state: json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
            parent_execution_id: None,
            segment_number: 0,
        })
        .await
        .unwrap();
    backend
        .append_event(Event::new(
            eid.clone(),
            0,
            EventKind::WorkflowStarted {
                workflow_id: workflow_id.into(),
                workflow_version: "0.1.0".into(),
                initial_input: json!({}),
                initiated_by: Some("token:the-initiator".into()),
                run_key: None,
            },
        ))
        .await
        .unwrap();
    for node in nodes {
        backend
            .append_event(Event::new(
                eid.clone(),
                0,
                EventKind::ToolApprovalRequired {
                    node_id: node.clone(),
                    tool_name: format!("tool_{node}"),
                    approver: "human".into(),
                    context: json!({ "action_digest": format!("digest-{node}") }),
                },
            ))
            .await
            .unwrap();
    }
    eid
}

#[tokio::test]
async fn pending_approvals_are_listed_across_runs() {
    for env in envs().await {
        register(
            &env,
            ir(
                "wf-a",
                gate(json!({ "approval_timeout_secs": 600, "escalate_after_secs": 60 })),
            ),
        )
        .await;
        register(&env, ir("wf-b", gate(json!({})))).await;
        register(&env, support::python_fn_start_ir("wf-c")).await;
        let a = start_and_request(&env, "wf-a").await;
        let b = start_and_request(&env, "wf-b").await;
        // Its first step waits for a worker; nothing asks for approval.
        let c = start(&env, "wf-c").await;
        project(&env.backend).await;

        let page = list(&env, &env.bob, "").await;
        let listed = rows(&page);
        assert_eq!(
            keys(&listed),
            BTreeSet::from([
                (exec_path(&a), "pay".to_string()),
                (exec_path(&b), "pay".to_string()),
            ]),
            "{}: the two open requests, and nothing for {}: {page}",
            env.name,
            exec_path(&c)
        );
        assert_eq!(
            page.get("next_after"),
            Some(&Value::Null),
            "{}: a page that is not full has no cursor: {page}",
            env.name
        );

        for (eid, workflow_id) in [(&a, "wf-a"), (&b, "wf-b")] {
            let row = listed
                .iter()
                .find(|r| r["execution_id"] == exec_path(eid))
                .unwrap();
            let events = env.backend.get_events(eid).await.unwrap();
            let (sequence, tool_name, approver, context) = request_of(&events, "pay");
            assert!(context["action_digest"].is_string(), "{context}");
            assert_eq!(row["workflow_id"], workflow_id, "{}: {row}", env.name);
            assert_eq!(row["workflow_version"], "0.1.0", "{}: {row}", env.name);
            assert_eq!(row["node_id"], "pay", "{}: {row}", env.name);
            assert_eq!(row["tool_name"], tool_name.as_str(), "{}: {row}", env.name);
            assert_eq!(row["approver"], approver.as_str(), "{}: {row}", env.name);
            assert_eq!(row["request_sequence"], sequence, "{}: {row}", env.name);
            assert_eq!(
                row["action_digest"], context["action_digest"],
                "{}: {row}",
                env.name
            );
            assert_eq!(row["context"], context, "{}: {row}", env.name);
            // The deadline keys are always there, null when the policy set none.
            for key in ["expires_at", "escalate_at"] {
                assert_eq!(
                    row.get(key),
                    Some(context.get(key).unwrap_or(&Value::Null)),
                    "{}: {key}: {row}",
                    env.name
                );
            }
        }
        let row_a = listed
            .iter()
            .find(|r| r["execution_id"] == exec_path(&a))
            .unwrap();
        assert!(row_a["expires_at"].is_string(), "{}: {row_a}", env.name);
        assert!(row_a["escalate_at"].is_string(), "{}: {row_a}", env.name);
    }
}

#[tokio::test]
async fn a_decided_request_leaves_the_list_after_the_projector_runs() {
    for env in envs().await {
        register(&env, ir("wf-a", gate(json!({})))).await;
        let eid = start_and_request(&env, "wf-a").await;
        project(&env.backend).await;
        assert_eq!(
            rows(&list(&env, &env.bob, "").await).len(),
            1,
            "{}",
            env.name
        );

        let digest = digest_of(&env, &eid).await;
        let (st, body) = decide(&env, &env.bob, &exec_path(&eid), &digest).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        project(&env.backend).await;

        let page = list(&env, &env.bob, "").await;
        assert!(rows(&page).is_empty(), "{}: {page}", env.name);
    }
}

#[tokio::test]
async fn an_approval_on_a_cancelled_run_is_not_listed() {
    for env in envs().await {
        register(&env, ir("wf-a", gate(json!({})))).await;
        let eid = start_and_request(&env, "wf-a").await;
        project(&env.backend).await;
        assert_eq!(
            rows(&list(&env, &env.bob, "").await).len(),
            1,
            "{}",
            env.name
        );

        let (st, body) = call(
            &env.app,
            "POST",
            &format!("/executions/{}/cancel", exec_path(&eid)),
            &env.alice,
            Some(json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);

        // The projector visits live runs only, so the cancelled run's row
        // stays `pending` in the read model. The list must not show it.
        let page = list(&env, &env.bob, "").await;
        assert!(rows(&page).is_empty(), "{}: {page}", env.name);
        project(&env.backend).await;
        let page = list(&env, &env.bob, "").await;
        assert!(rows(&page).is_empty(), "{}: {page}", env.name);
    }
}

/// The API admits `default` tokens only, so the other tenants' runs are
/// written at store level, and each router reads through one tenant's scoped
/// backend over one SQLite file, as `main.rs` wires `backend_for`. The
/// projector runs on the base backend and stamps every row `default`, so the
/// list cannot trust the read model's own tenant column.
#[tokio::test]
async fn one_tenant_never_lists_another_tenants_approvals() {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "jamjet_approvals_tenants_{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let sqlite = Arc::new(
        SqliteBackend::open(&format!("sqlite://{}", path.display()))
            .await
            .unwrap(),
    );
    let now = chrono::Utc::now();
    for tenant in ["acme", "globex"] {
        sqlite
            .for_tenant(TenantId::default())
            .create_tenant(Tenant {
                id: TenantId::from(tenant),
                name: tenant.into(),
                status: TenantStatus::Active,
                policy: None,
                limits: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
    }
    let mut runs = Vec::new();
    for tenant in ["default", "acme", "globex"] {
        let scoped: Arc<dyn StateBackend> = Arc::new(sqlite.for_tenant(TenantId::from(tenant)));
        let eid = seed_requests(&scoped, &format!("wf-{tenant}"), &["pay".to_string()]).await;
        runs.push((tenant, eid));
    }
    let base: Arc<dyn StateBackend> = sqlite.clone();
    assert_eq!(project(&base).await, 3, "one pending row per run");

    for (tenant, eid) in &runs {
        let mut state = support::make_state(base.clone());
        let scoped = sqlite.clone();
        let t = TenantId::from(*tenant);
        state.backend_for_fn = Arc::new(move |_caller: &TenantId| {
            Arc::new(scoped.for_tenant(t.clone())) as Arc<dyn StateBackend>
        });
        let app = build_router_with_opts(state, true);
        let resp = app
            .oneshot(Request::get("/approvals").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let page: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        assert_eq!(status, StatusCode::OK, "{tenant}: {page}");
        assert!(
            page["approvals"].is_array(),
            "{tenant}: the answer is not a list of approvals: {page}"
        );
        assert_eq!(
            keys(&rows(&page)),
            BTreeSet::from([(exec_path(eid), "pay".to_string())]),
            "{tenant} lists its own approvals and no other tenant's: {page}"
        );
    }
}

#[tokio::test]
async fn the_listed_digest_approves() {
    for env in envs().await {
        register(&env, ir("wf-a", gate(json!({})))).await;
        let eid = start_and_request(&env, "wf-a").await;
        project(&env.backend).await;
        let row = rows(&list(&env, &env.bob, "").await).remove(0);
        assert!(row["action_digest"].is_string(), "{}: {row}", env.name);

        let (st, body) = decide(
            &env,
            &env.bob,
            row["execution_id"].as_str().unwrap(),
            &row["action_digest"],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);

        // The decision answers the listed request.
        let events = env.backend.get_events(&eid).await.unwrap();
        let answered = events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::ApprovalReceived {
                    request_sequence,
                    approved_digest,
                    ..
                } => Some((*request_sequence, approved_digest.clone())),
                _ => None,
            })
            .expect("a decision");
        assert_eq!(
            json!(answered.0),
            row["request_sequence"],
            "{}: {row}",
            env.name
        );
        assert_eq!(json!(answered.1), row["action_digest"], "{}", env.name);
    }
}

/// A row's `action_digest` is the echo the approve route accepts: the
/// request's `action_digest`, else the `calls_hash` of an older agent tool
/// dispatch request, else null for a legacy request that binds nothing.
#[tokio::test]
async fn the_listed_digest_is_the_requests_binding() {
    for env in envs().await {
        let eid = seed_requests(&env.backend, "wf-bind", &[]).await;
        for (node, context) in [
            (
                "both",
                json!({ "action_digest": "d-both", "calls_hash": "c-both" }),
            ),
            ("calls", json!({ "calls_hash": "c-calls" })),
            ("legacy", json!({})),
        ] {
            env.backend
                .append_event(Event::new(
                    eid.clone(),
                    0,
                    EventKind::ToolApprovalRequired {
                        node_id: node.into(),
                        tool_name: "tool".into(),
                        approver: "human".into(),
                        context,
                    },
                ))
                .await
                .unwrap();
        }
        project(&env.backend).await;

        let page = list(&env, &env.bob, "").await;
        let listed = rows(&page);
        let digest = |node: &str| {
            listed
                .iter()
                .find(|r| r["node_id"] == node)
                .map(|r| r["action_digest"].clone())
        };
        assert_eq!(
            digest("both"),
            Some(json!("d-both")),
            "{}: {page}",
            env.name
        );
        assert_eq!(
            digest("calls"),
            Some(json!("c-calls")),
            "{}: {page}",
            env.name
        );
        assert_eq!(digest("legacy"), Some(Value::Null), "{}: {page}", env.name);
    }
}

#[tokio::test]
async fn a_row_decided_after_listing_is_refused_by_the_approve_route() {
    for env in envs().await {
        register(&env, ir("wf-a", gate(json!({})))).await;
        let eid = start_and_request(&env, "wf-a").await;
        project(&env.backend).await;
        let row = rows(&list(&env, &env.carol, "").await).remove(0);
        let id = row["execution_id"].as_str().unwrap().to_string();

        // Bob decides after carol listed the request.
        let (st, body) = decide(&env, &env.bob, &id, &row["action_digest"]).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let before = env.backend.get_events(&eid).await.unwrap().len();

        let (st, body) = decide(&env, &env.carol, &id, &row["action_digest"]).await;
        assert_eq!(st, StatusCode::CONFLICT, "{}: {body}", env.name);
        let events = env.backend.get_events(&eid).await.unwrap();
        assert_eq!(events.len(), before, "{}: nothing recorded", env.name);
        assert_eq!(decisions(&events), 1, "{}", env.name);
    }
}

#[tokio::test]
async fn a_malformed_cursor_is_400() {
    for env in envs().await {
        // Not hex; odd length; hex of bytes that are not JSON; hex of JSON
        // that is not a cursor (`{}`).
        for after in ["zz", "abc", "ff", "7b7d"] {
            let (st, body) = call(
                &env.app,
                "GET",
                &format!("/approvals?after={after}"),
                &env.bob,
                None,
            )
            .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{}: {after}: {body}", env.name);
            assert!(body["error"].is_string(), "{}: {after}: {body}", env.name);
        }
    }
}

#[tokio::test]
async fn status_other_than_pending_is_400() {
    for env in envs().await {
        register(&env, ir("wf-a", gate(json!({})))).await;
        start_and_request(&env, "wf-a").await;
        project(&env.backend).await;

        for status in ["approved", "rejected", "all"] {
            let (st, body) = call(
                &env.app,
                "GET",
                &format!("/approvals?status={status}"),
                &env.bob,
                None,
            )
            .await;
            assert_eq!(
                st,
                StatusCode::BAD_REQUEST,
                "{}: {status}: {body}",
                env.name
            );
            assert!(body["error"].is_string(), "{}: {status}: {body}", env.name);
        }
        // `pending` and no status at all list the same rows.
        assert_eq!(
            rows(&list(&env, &env.bob, "?status=pending").await).len(),
            1,
            "{}",
            env.name
        );
        assert_eq!(
            rows(&list(&env, &env.bob, "").await).len(),
            1,
            "{}",
            env.name
        );
    }
}

#[tokio::test]
async fn limit_is_clamped_to_200() {
    for env in envs().await {
        let nodes: Vec<String> = (0..201).map(|i| format!("n{i:03}")).collect();
        let eid = seed_requests(&env.backend, "wf-many", &nodes).await;
        project(&env.backend).await;

        let first = list(&env, &env.bob, "?limit=500").await;
        assert_eq!(rows(&first).len(), 200, "{}: at most 200 rows", env.name);
        let cursor = first["next_after"]
            .as_str()
            .unwrap_or_else(|| panic!("{}: a full page has a cursor: {first}", env.name))
            .to_string();

        let second = list(&env, &env.bob, &format!("?limit=500&after={cursor}")).await;
        assert_eq!(rows(&second).len(), 1, "{}: {second}", env.name);
        assert_eq!(
            second.get("next_after"),
            Some(&Value::Null),
            "{}: the last page has no cursor: {second}",
            env.name
        );

        // Each row once, none missed.
        let mut all = rows(&first);
        all.extend(rows(&second));
        let expected: BTreeSet<(String, String)> =
            nodes.iter().map(|n| (exec_path(&eid), n.clone())).collect();
        assert_eq!(keys(&all), expected, "{}", env.name);
        assert_eq!(all.len(), 201, "{}", env.name);

        // 0 is raised to 1; no limit means 50.
        assert_eq!(
            rows(&list(&env, &env.bob, "?limit=0").await).len(),
            1,
            "{}",
            env.name
        );
        assert_eq!(
            rows(&list(&env, &env.bob, "").await).len(),
            50,
            "{}",
            env.name
        );

        // A limit that is not a number is refused.
        let (st, body) = call(&env.app, "GET", "/approvals?limit=lots", &env.bob, None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{}: {body}", env.name);
    }
}

#[tokio::test]
async fn a_viewer_token_may_list_but_not_approve() {
    for env in envs().await {
        register(&env, ir("wf-a", gate(json!({})))).await;
        let eid = start_and_request(&env, "wf-a").await;
        project(&env.backend).await;

        let row = rows(&list(&env, &env.vic, "").await).remove(0);
        assert_eq!(row["execution_id"], exec_path(&eid), "{}: {row}", env.name);

        let (st, body) = decide(
            &env,
            &env.vic,
            row["execution_id"].as_str().unwrap(),
            &row["action_digest"],
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{}: {body}", env.name);
        let events = env.backend.get_events(&eid).await.unwrap();
        assert_eq!(decisions(&events), 0, "{}", env.name);
    }
}

/// The list reads the read model, which catches up once per projector tick.
#[tokio::test]
async fn a_request_raised_after_the_last_tick_appears_after_the_next() {
    for env in envs().await {
        register(&env, ir("wf-a", gate(json!({})))).await;
        project(&env.backend).await;

        let eid = start_and_request(&env, "wf-a").await;
        let page = list(&env, &env.bob, "").await;
        assert!(
            rows(&page).is_empty(),
            "{}: not projected yet: {page}",
            env.name
        );

        project(&env.backend).await;
        let page = list(&env, &env.bob, "").await;
        assert_eq!(
            keys(&rows(&page)),
            BTreeSet::from([(exec_path(&eid), "pay".to_string())]),
            "{}: {page}",
            env.name
        );
    }
}
