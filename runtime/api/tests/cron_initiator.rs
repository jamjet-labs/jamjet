//! The embedded cron scheduler starts runs in-process.
//!
//! It used to POST to the server's own `/executions` with no token, which
//! outside dev mode is a 401, so `JAMJET_EMBED_CRON` never started a run on an
//! authenticated server. A run takes the job's creator as its initiator (see
//! `cron_creator_sod.rs`). A job from before creators were recorded has none,
//! so its runs have no initiator and cannot be approved outside dev mode.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::cron::InProcessCronLauncher;
use jamjet_api::routes::build_router_with_opts;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::{Event, SqliteBackend};
use jamjet_timers::{CronJob, CronScheduler, CronStore};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

mod support;

#[tokio::test]
async fn a_job_with_no_recorded_creator_starts_a_run_nobody_can_approve() {
    let mut path = std::env::temp_dir();
    path.push(format!("jamjet_cron_{}.db", uuid::Uuid::new_v4().simple()));
    let sqlite = Arc::new(
        SqliteBackend::open(&format!("sqlite://{}", path.display()))
            .await
            .unwrap(),
    );
    let backend: Arc<dyn StateBackend> = sqlite.clone();
    let state = support::make_state(backend.clone());
    let (st, body) = support::post(
        &state,
        "/workflows",
        json!({ "ir": support::python_fn_start_ir("wf-cron") }),
    )
    .await;
    assert!(st.is_success(), "{body}");

    let now = chrono::Utc::now();
    CronStore::new(sqlite.pool())
        .upsert(&CronJob {
            id: uuid::Uuid::new_v4(),
            name: "nightly".into(),
            cron_expression: "0 2 * * *".into(),
            workflow_id: "wf-cron".into(),
            workflow_version: "0.1.0".into(),
            input: json!({}),
            enabled: true,
            last_run_at: None,
            next_run_at: now - chrono::Duration::minutes(1),
            created_at: now,
            created_by: None,
            tenant_id: "default".into(),
        })
        .await
        .unwrap();

    CronScheduler::with_launcher(
        sqlite.pool(),
        Arc::new(InProcessCronLauncher::new(state.clone())),
    )
    .tick()
    .await
    .unwrap();

    let runs = backend.list_executions(None, 10, 0).await.unwrap();
    assert_eq!(runs.len(), 1, "the due job started one run");
    let eid = runs[0].execution_id.clone();
    let events = backend.get_events(&eid).await.unwrap();
    assert_eq!(
        jamjet_state::approvals::run_initiator(&events),
        None,
        "a pre-upgrade job has no creator to record"
    );

    // With no initiator, separation of duties has nothing to compare, so the
    // approval is refused outside dev mode (the unknown-initiator rule).
    let seq = backend.latest_sequence(&eid).await.unwrap() + 1;
    backend
        .append_event(Event::new(
            eid.clone(),
            seq,
            EventKind::ToolApprovalRequired {
                node_id: "py_start".into(),
                tool_name: "t".into(),
                approver: "human".into(),
                context: json!({ "node_id": "py_start" }),
            },
        ))
        .await
        .unwrap();
    let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
    let resp = build_router_with_opts(state, false)
        .oneshot(
            Request::post(format!("/executions/{eid}/approve"))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {bob}"))
                .body(Body::from(json!({ "decision": "approved" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
}
