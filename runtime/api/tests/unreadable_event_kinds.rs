//! A log holding an event kind this build cannot read fails closed.
//!
//! `EventKind::Unknown` keeps such a log loadable and listable, but its kind
//! and node are unknown: it could be a revocation or a denial written by a
//! newer release. So no approval is treated as covering anything, no
//! decision is accepted, and the scheduler does not advance the run (it
//! records `run_stalled` once). Listing still works.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_scheduler::Scheduler;
use jamjet_state::event::EventKind;
use jamjet_state::Event;
use serde_json::{json, Value};
use tower::ServiceExt;

mod support;

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let resp = app
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

fn ir() -> Value {
    json!({
        "workflow_id": "wf-unknown", "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "pay",
        "nodes": { "pay": {
            "id": "pay",
            "kind": { "type": "python_fn", "module": "myapp.payments", "function": "transfer", "output_schema": "" },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {},
            "policy": { "require_approval_for": ["*"] }
        } },
        "edges": [ { "from": "pay", "to": "end", "condition": null } ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

fn count(events: &[Event], pred: impl Fn(&EventKind) -> bool) -> usize {
    events.iter().filter(|e| pred(&e.kind)).count()
}

#[tokio::test]
async fn an_unreadable_event_blocks_release_and_decisions_and_stalls_the_run() {
    let mut failures = Vec::new();
    for tb in support::backends().await {
        let name = tb.name();
        let backend = tb.dyn_backend();
        let app = build_router_with_opts(support::make_state(backend.clone()), false);
        let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
        let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
        let (st, body) = call(&app, "POST", "/workflows", &alice, json!({ "ir": ir() })).await;
        assert!(st.is_success(), "{body}");
        let (st, body) = call(
            &app,
            "POST",
            "/executions",
            &alice,
            json!({ "workflow_id": "wf-unknown", "workflow_version": "0.1.0", "input": {} }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{body}");
        let id = body["execution_id"].as_str().unwrap().to_string();
        let eid = ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap());
        let claim = json!({ "worker_id": "w", "queue_types": ["python_tool"] });
        let (_, claimed) = call(&app, "POST", "/work-items/claim", &alice, claim.clone()).await;
        assert_eq!(
            claimed,
            json!({ "claimed": false }),
            "{name}: held for approval"
        );
        let digest = backend
            .get_events(&eid)
            .await
            .unwrap()
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::ToolApprovalRequired { context, .. } => {
                    context["action_digest"].as_str().map(str::to_owned)
                }
                _ => None,
            })
            .unwrap();
        let (st, body) = call(
            &app,
            "POST",
            &format!("/executions/{id}/approve"),
            &bob,
            json!({ "decision": "approved", "node_id": "pay", "action_digest": digest }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{name}: {body}");

        // The scheduler re-dispatches the approved node: its item is waiting.
        Scheduler::new(backend.clone()).tick().await.unwrap();

        // A newer release wrote an event this build cannot read.
        backend
            .append_event(Event::new(eid.clone(), 0, EventKind::Unknown))
            .await
            .unwrap();

        // The approved node is NOT released, and no new request is raised.
        let requests = |events: &[Event]| {
            count(events, |k| {
                matches!(k, EventKind::ToolApprovalRequired { .. })
            })
        };
        let before = requests(&backend.get_events(&eid).await.unwrap());
        let (st, claimed) = call(&app, "POST", "/work-items/claim", &alice, claim.clone()).await;
        if st != StatusCode::OK || claimed != json!({ "claimed": false }) {
            failures.push(format!(
                "{name}: approved node released over an unreadable log: {claimed}"
            ));
        }
        let after = requests(&backend.get_events(&eid).await.unwrap());
        if after != before {
            failures.push(format!(
                "{name}: a request was raised over an unreadable log"
            ));
        }
        // No decision is accepted.
        let (st, body) = call(
            &app,
            "POST",
            &format!("/executions/{id}/approve"),
            &bob,
            json!({ "decision": "rejected", "node_id": "pay" }),
        )
        .await;
        if st != StatusCode::CONFLICT || !body.to_string().contains("cannot read") {
            failures.push(format!(
                "{name}: decision on an unreadable log: {st} {body}"
            ));
        }
        // The scheduler does not advance the run; it records the stall once.
        let scheduler = Scheduler::new(backend.clone());
        for _ in 0..3 {
            scheduler.tick().await.unwrap();
        }
        let events = backend.get_events(&eid).await.unwrap();
        let stalled = count(&events, |k| matches!(k, EventKind::RunStalled { .. }));
        let advanced = count(&events, |k| {
            matches!(
                k,
                EventKind::NodeCompleted { .. }
                    | EventKind::NodeFailed { .. }
                    | EventKind::WorkflowCompleted { .. }
                    | EventKind::WorkflowFailed { .. }
            )
        });
        if stalled != 1 || advanced != 0 {
            failures.push(format!(
                "{name}: {stalled} stall record(s), {advanced} advancing event(s)"
            ));
        }
        let status = backend.get_execution(&eid).await.unwrap().unwrap().status;
        if status != WorkflowStatus::Running {
            failures.push(format!("{name}: run moved to {status:?}"));
        }

        // Listing still works, and shows the event as unknown.
        let (st, listing) = call(
            &app,
            "GET",
            &format!("/executions/{id}/approvals"),
            &bob,
            Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{name}: {listing}");
        let (st, evs) = call(
            &app,
            "GET",
            &format!("/executions/{id}/events"),
            &bob,
            Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{name}: {evs}");
        assert!(
            evs["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"]["type"] == "unknown"),
            "{name}: {evs}"
        );
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
