//! Bounds on the listing routes: `GET /executions` caps its page and refuses
//! an unknown status, and `GET /executions/:id/events` returns the log in
//! pages and inlines only spilled outputs that are small.

mod support;

use jamjet_api::routes::{MAX_EVENTS_PAGE, MAX_EXECUTIONS_PAGE, MAX_INLINE_ARTIFACT_BYTES};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::StateBackend;
use jamjet_state::{Event, EventKind, InMemoryBackend};
use serde_json::{json, Value};
use std::sync::Arc;
use support::{get, make_state};

fn execution(status: WorkflowStatus) -> WorkflowExecution {
    let now = chrono::Utc::now();
    WorkflowExecution {
        execution_id: ExecutionId::new(),
        workflow_id: "wf".into(),
        workflow_version: "1".into(),
        status,
        initial_input: json!({}),
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    }
}

fn completed(node_id: &str, output: Value) -> EventKind {
    EventKind::NodeCompleted {
        node_id: node_id.into(),
        output,
        state_patch: json!({}),
        duration_ms: 1,
        gen_ai_system: None,
        gen_ai_model: None,
        input_tokens: None,
        output_tokens: None,
        finish_reason: None,
        cost_usd: None,
        provenance: None,
        idempotency_key: None,
    }
}

/// `?limit=` asked for more than the cap gets the cap. It had none, so one
/// request could load every run in the database.
#[tokio::test]
async fn get_executions_caps_its_page() {
    let backend = Arc::new(InMemoryBackend::new());
    for _ in 0..(MAX_EXECUTIONS_PAGE + 50) {
        backend
            .create_execution(execution(WorkflowStatus::Running))
            .await
            .unwrap();
    }
    let state = make_state(backend);
    let (status, body) = get(&state, "/executions?limit=100000").await;
    assert_eq!(status, 200);
    assert_eq!(
        body["executions"].as_array().unwrap().len(),
        MAX_EXECUTIONS_PAGE as usize
    );
    let (_, body) = get(&state, "/executions?limit=7").await;
    assert_eq!(body["executions"].as_array().unwrap().len(), 7);
}

/// Every status filters, and an unknown one is refused. `cancelled` (and
/// any status but four) used to mean no filter at all.
#[tokio::test]
async fn get_executions_filters_every_status_and_refuses_unknown_ones() {
    let backend = Arc::new(InMemoryBackend::new());
    for s in [
        WorkflowStatus::Running,
        WorkflowStatus::Cancelled,
        WorkflowStatus::LimitExceeded,
    ] {
        backend.create_execution(execution(s)).await.unwrap();
    }
    let state = make_state(backend);
    for (filter, want) in [
        ("cancelled", "cancelled"),
        ("limit_exceeded", "limit_exceeded"),
    ] {
        let (status, body) = get(&state, &format!("/executions?status={filter}")).await;
        assert_eq!(status, 200);
        let runs = body["executions"].as_array().unwrap();
        assert_eq!(runs.len(), 1, "{filter}: {body}");
        assert_eq!(runs[0]["status"], want);
    }
    let (status, _) = get(&state, "/executions?status=bogus").await;
    assert_eq!(status, 400);
}

async fn run_with_events(backend: &Arc<InMemoryBackend>, n: i64) -> ExecutionId {
    let e = execution(WorkflowStatus::Running);
    let id = e.execution_id.clone();
    backend.create_execution(e).await.unwrap();
    for seq in 1..=n {
        backend
            .append_event(Event::new(
                id.clone(),
                seq,
                completed(&format!("n{seq}"), json!({ "i": seq })),
            ))
            .await
            .unwrap();
    }
    id
}

fn sequences(body: &Value) -> Vec<i64> {
    body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["sequence"].as_i64().unwrap())
        .collect()
}

/// The log comes back in pages joined by `next_after`. The route read and
/// returned the whole log, however long.
#[tokio::test]
async fn events_come_back_in_pages() {
    let backend = Arc::new(InMemoryBackend::new());
    let id = run_with_events(&backend, 25).await;
    let state = make_state(backend);
    let path = format!("/executions/{id}/events");

    let (status, first) = get(&state, &format!("{path}?limit=10")).await;
    assert_eq!(status, 200);
    assert_eq!(sequences(&first), (1..=10).collect::<Vec<_>>());
    assert_eq!(first["next_after"], 10);

    let (_, second) = get(&state, &format!("{path}?after=10&limit=10")).await;
    assert_eq!(sequences(&second), (11..=20).collect::<Vec<_>>());
    assert_eq!(second["next_after"], 20);

    let (_, last) = get(&state, &format!("{path}?after=20&limit=10")).await;
    assert_eq!(sequences(&last), (21..=25).collect::<Vec<_>>());
    assert_eq!(last["next_after"], Value::Null);

    // With no limit, a short log is one page, as before.
    let (_, all) = get(&state, &path).await;
    assert_eq!(sequences(&all).len(), 25);
    assert_eq!(all["next_after"], Value::Null);

    // A limit above the cap is the cap.
    let (_, capped) = get(&state, &format!("{path}?limit={}", MAX_EVENTS_PAGE + 1)).await;
    assert_eq!(sequences(&capped).len(), 25);
}

/// A spilled output past the inline cap stays a reference unless asked for;
/// a small one is still inlined. Every spilled output used to be inlined, so
/// one request could pull every artifact of a run into memory.
#[tokio::test]
async fn a_large_spilled_output_stays_a_reference_by_default() {
    let backend = Arc::new(InMemoryBackend::new());
    let e = execution(WorkflowStatus::Running);
    let id = e.execution_id.clone();
    backend.create_execution(e).await.unwrap();
    let small = json!({ "content": "s".repeat(10_000) });
    let large = json!({ "content": "l".repeat(MAX_INLINE_ARTIFACT_BYTES as usize + 1) });
    for (seq, (node, output)) in [("small", &small), ("large", &large)]
        .into_iter()
        .enumerate()
    {
        let r = backend
            .put_artifact(
                &serde_json::to_vec(output).unwrap(),
                Some("application/json"),
            )
            .await
            .unwrap();
        backend
            .append_event(Event::new(
                id.clone(),
                seq as i64 + 1,
                completed(node, r.to_sentinel()),
            ))
            .await
            .unwrap();
    }
    let state = make_state(backend);
    let output = |body: &Value, i: usize| body["events"][i]["kind"]["output"].clone();

    let (status, body) = get(&state, &format!("/executions/{id}/events")).await;
    assert_eq!(status, 200);
    assert_eq!(output(&body, 0), small, "a small spilled output is inlined");
    let large_out = output(&body, 1);
    assert!(
        large_out.get(jamjet_state::ARTIFACT_SENTINEL_KEY).is_some(),
        "the large one stays a reference"
    );
    assert!(large_out.to_string().len() < 1024);

    let (_, all) = get(
        &state,
        &format!("/executions/{id}/events?inline_artifacts=all"),
    )
    .await;
    assert_eq!(output(&all, 1), large);

    let (_, none) = get(
        &state,
        &format!("/executions/{id}/events?inline_artifacts=none"),
    )
    .await;
    assert!(output(&none, 0)
        .get(jamjet_state::ARTIFACT_SENTINEL_KEY)
        .is_some());

    let (status, _) = get(
        &state,
        &format!("/executions/{id}/events?inline_artifacts=some"),
    )
    .await;
    assert_eq!(status, 400);
}
