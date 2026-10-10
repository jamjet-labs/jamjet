//! Cron jobs belong to a tenant.
//!
//! Jobs were keyed by name alone, `GET /cron` listed every tenant's jobs with
//! their input and creator, `DELETE /cron/:name` deleted any of them, and the
//! launcher started every run in `default`. A developer in tenant `acme` could
//! read a default-tenant job, replace it by name with her own input, and have
//! that input run in `default` on the next tick (round-3 review, I1).
//!
//! The API refuses tokens of any tenant but `default` while the engine is
//! single-tenant (see `tenant_tokens_refused.rs`), so the other tenant's job
//! is written at the store level here; the routes and the launcher are what
//! is under test.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::cron::InProcessCronLauncher;
use jamjet_api::routes::build_router_with_opts;
use jamjet_api::state::AppState;
use jamjet_state::backend::StateBackend;
use jamjet_state::{SqliteBackend, Tenant, TenantId, TenantStatus};
use jamjet_timers::{CronScheduler, CronStore};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

mod support;

struct Env {
    sqlite: Arc<SqliteBackend>,
    state: AppState,
    app: axum::Router,
    alice: String,
}

/// SQLite with per-tenant backends, wired as `main.rs` wires them.
async fn env() -> Env {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "jamjet_cron_tenant_{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let sqlite = Arc::new(
        SqliteBackend::open(&format!("sqlite://{}", path.display()))
            .await
            .unwrap(),
    );
    let now = chrono::Utc::now();
    sqlite
        .for_tenant(TenantId::default())
        .create_tenant(Tenant {
            id: TenantId::from("acme"),
            name: "Acme".into(),
            status: TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let mut state = support::make_state(sqlite.clone() as Arc<dyn StateBackend>);
    let scoped = sqlite.clone();
    state.backend_for_fn = Arc::new(move |t: &TenantId| {
        Arc::new(scoped.for_tenant(t.clone())) as Arc<dyn StateBackend>
    });
    state.cron_store = Some(Arc::new(CronStore::new(sqlite.pool())));
    let app = build_router_with_opts(state.clone(), false);
    let (alice, _) = sqlite.create_token("alice", "developer").await.unwrap();
    Env {
        sqlite,
        state,
        app,
        alice,
    }
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

async fn register(env: &Env, token: &str) {
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        token,
        Some(json!({ "ir": support::python_fn_start_ir("wf-pay") })),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
}

async fn schedule(env: &Env, token: &str, to: &str) -> StatusCode {
    let (st, _) = call(
        &env.app,
        "POST",
        "/cron",
        token,
        Some(json!({
            "name": "nightly",
            "cron_expression": "0 2 * * *",
            "workflow_id": "wf-pay",
            "workflow_version": "0.1.0",
            "input": { "to": to },
        })),
    )
    .await;
    st
}

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

/// The `to` of every run in `tenant`.
async fn run_inputs(env: &Env, tenant: &str) -> Vec<Value> {
    env.sqlite
        .for_tenant(TenantId::from(tenant))
        .list_executions(None, 50, 0)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.initial_input["to"].clone())
        .collect()
}

/// Another tenant's job of the same name, as the store would hold it.
async fn acme_job(env: &Env) {
    let store = CronStore::new(env.sqlite.pool());
    let mut job = store.list_for_tenant("default").await.unwrap().remove(0);
    job.id = uuid::Uuid::new_v4();
    job.tenant_id = "acme".into();
    job.input = json!({ "to": "mallory" });
    job.created_by = Some("token:mallory".into());
    store.upsert(&job).await.unwrap();
}

/// The reviewer's demonstration: a job of the same name in another tenant
/// is neither shown nor replaced, deleted or run in place of this one.
#[tokio::test]
async fn a_job_in_another_tenant_is_separate_from_this_one() {
    let env = env().await;
    register(&env, &env.alice).await;
    assert_eq!(
        schedule(&env, &env.alice, "vendor").await,
        StatusCode::CREATED
    );
    acme_job(&env).await;

    // The default tenant's listing shows only its own job.
    let (st, listed) = call(&env.app, "GET", "/cron", &env.alice, None).await;
    assert_eq!(st, StatusCode::OK, "{listed}");
    assert_eq!(listed["cron_jobs"].as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed["cron_jobs"][0]["input"]["to"], "vendor");

    // The default job runs in default. The other tenant's job starts no run
    // at all: the engine is single-tenant, so the launcher refuses it, exactly
    // as the API refuses that tenant's tokens.
    register_in_acme(&env).await;
    fire_due_jobs(&env).await;
    assert_eq!(run_inputs(&env, "default").await, vec![json!("vendor")]);
    assert!(
        run_inputs(&env, "acme").await.is_empty(),
        "a non-default job ran"
    );

    // Deleting by name deletes only the caller's tenant's job.
    let (st, body) = call(&env.app, "DELETE", "/cron/nightly", &env.alice, None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let store = CronStore::new(env.sqlite.pool());
    assert!(store.list_for_tenant("default").await.unwrap().is_empty());
    assert_eq!(store.list_for_tenant("acme").await.unwrap().len(), 1);
}

/// The acme copy of the workflow, as the store would hold it.
async fn register_in_acme(env: &Env) {
    env.sqlite
        .for_tenant(TenantId::from("acme"))
        .store_workflow(jamjet_state::WorkflowDefinition {
            workflow_id: "wf-pay".into(),
            version: "0.1.0".into(),
            ir: support::python_fn_start_ir("wf-pay"),
            created_at: chrono::Utc::now(),
            tenant_id: "acme".into(),
        })
        .await
        .unwrap();
}
