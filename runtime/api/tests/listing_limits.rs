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

/// A sentinel whose `size` understates the stored bytes stays a reference:
/// an external worker writes the sentinel, so its size is a claim.
#[tokio::test]
async fn a_sentinel_that_understates_its_size_stays_a_reference() {
    let backend = Arc::new(InMemoryBackend::new());
    let e = execution(WorkflowStatus::Running);
    let id = e.execution_id.clone();
    backend.create_execution(e).await.unwrap();
    let large = json!({ "content": "l".repeat(MAX_INLINE_ARTIFACT_BYTES as usize + 1) });
    let mut r = backend
        .put_artifact(
            &serde_json::to_vec(&large).unwrap(),
            Some("application/json"),
        )
        .await
        .unwrap();
    r.size = 10;
    backend
        .append_event(Event::new(
            id.clone(),
            1,
            completed("liar", r.to_sentinel()),
        ))
        .await
        .unwrap();
    let state = make_state(backend);
    let (_, body) = get(&state, &format!("/executions/{id}/events")).await;
    let out = &body["events"][0]["kind"]["output"];
    assert!(
        out.get(jamjet_state::ARTIFACT_SENTINEL_KEY).is_some(),
        "the stored bytes decide, not the claim"
    );
}

/// A page inlines at most `INLINE_PAGE_BUDGET` of spilled output in all;
/// the per-output cap alone let a page of small spills grow without bound.
#[tokio::test]
async fn a_page_inlines_at_most_its_budget() {
    use jamjet_api::routes::INLINE_PAGE_BUDGET;
    let backend = Arc::new(InMemoryBackend::new());
    let e = execution(WorkflowStatus::Running);
    let id = e.execution_id.clone();
    backend.create_execution(e).await.unwrap();
    // Each just under the per-output cap; together past the page budget.
    let each = (MAX_INLINE_ARTIFACT_BYTES - 1024) as usize;
    let n = (INLINE_PAGE_BUDGET as usize / each) + 3;
    for i in 0..n {
        let output = json!({ "i": i, "content": "x".repeat(each - 32) });
        let r = backend
            .put_artifact(
                &serde_json::to_vec(&output).unwrap(),
                Some("application/json"),
            )
            .await
            .unwrap();
        backend
            .append_event(Event::new(
                id.clone(),
                i as i64 + 1,
                completed("n", r.to_sentinel()),
            ))
            .await
            .unwrap();
    }
    let state = make_state(backend);
    let (_, body) = get(&state, &format!("/executions/{id}/events")).await;
    let outputs: Vec<&Value> = body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["kind"]["output"])
        .collect();
    let refs = outputs
        .iter()
        .filter(|o| o.get(jamjet_state::ARTIFACT_SENTINEL_KEY).is_some())
        .count();
    let inlined = outputs.len() - refs;
    assert!(refs >= 1, "past the budget, outputs stay references");
    assert!(
        (inlined as u64) * (each as u64) <= INLINE_PAGE_BUDGET,
        "{inlined} inlined"
    );
    assert!(inlined > 0, "outputs within the budget are inlined");
}

/// `?limit=` above the cap returns the cap and a cursor.
#[tokio::test]
async fn an_events_limit_above_the_cap_returns_the_cap() {
    let backend = Arc::new(InMemoryBackend::new());
    let id = run_with_events(&backend, MAX_EVENTS_PAGE as i64 + 1).await;
    let state = make_state(backend);
    let (_, body) = get(
        &state,
        &format!("/executions/{id}/events?limit={}", MAX_EVENTS_PAGE * 2),
    )
    .await;
    assert_eq!(
        body["events"].as_array().unwrap().len(),
        MAX_EVENTS_PAGE as usize
    );
    assert_eq!(body["next_after"], MAX_EVENTS_PAGE as i64);
}

/// A sentinel claiming `size: 0` for a large stored artifact: the page is
/// charged the stored bytes, per reference. Charged by the claim, once per
/// event, one event holding 100 such references inlined all of them.
#[tokio::test]
async fn a_page_charges_stored_bytes_not_the_claimed_size() {
    use jamjet_api::routes::INLINE_PAGE_BUDGET;
    let backend = Arc::new(InMemoryBackend::new());
    let e = execution(WorkflowStatus::Running);
    let id = e.execution_id.clone();
    backend.create_execution(e).await.unwrap();
    // About 250 KiB, under the per-output cap.
    let each = (MAX_INLINE_ARTIFACT_BYTES - 6 * 1024) as usize;
    let output = json!({ "content": "x".repeat(each - 32) });
    let stored = serde_json::to_vec(&output).unwrap();
    let r = backend
        .put_artifact(&stored, Some("application/json"))
        .await
        .unwrap();
    let mut sentinel = r.to_sentinel();
    sentinel[jamjet_state::ARTIFACT_SENTINEL_KEY]["size"] = json!(0);
    backend
        .append_event(Event::new(
            id.clone(),
            1,
            completed("n", json!(vec![sentinel.clone(); 100])),
        ))
        .await
        .unwrap();
    // And across events, each claiming 0.
    for i in 0..80 {
        backend
            .append_event(Event::new(
                id.clone(),
                i + 2,
                completed("m", sentinel.clone()),
            ))
            .await
            .unwrap();
    }
    let state = make_state(backend);
    let (_, body) = get(&state, &format!("/executions/{id}/events")).await;
    let events = body["events"].as_array().unwrap();
    let is_ref = |v: &Value| v.get(jamjet_state::ARTIFACT_SENTINEL_KEY).is_some();
    let in_array = events[0]["kind"]["output"].as_array().unwrap();
    let inlined_in_array = in_array.iter().filter(|v| !is_ref(v)).count();
    let inlined_after = events[1..]
        .iter()
        .filter(|e| !is_ref(&e["kind"]["output"]))
        .count();
    let inlined = (inlined_in_array + inlined_after) as u64;
    assert!(
        inlined * stored.len() as u64 <= INLINE_PAGE_BUDGET,
        "{inlined_in_array} in the array and {inlined_after} after inlined"
    );
    assert!(
        inlined_in_array > 0,
        "references within the budget are inlined"
    );
    assert!(inlined_in_array < 100, "the array inlined every reference");
}

/// Events under the spill threshold are never spilled, so a page of them
/// had no byte bound. It ends at [`MAX_EVENTS_PAGE_BYTES`] with a cursor, and
/// the next page carries on.
#[tokio::test]
async fn a_page_ends_at_its_byte_budget() {
    use jamjet_api::routes::MAX_EVENTS_PAGE_BYTES;
    let backend = Arc::new(InMemoryBackend::new());
    let e = execution(WorkflowStatus::Running);
    let id = e.execution_id.clone();
    backend.create_execution(e).await.unwrap();
    let patch = "y".repeat(7 * 1024);
    let n = (MAX_EVENTS_PAGE_BYTES / (7 * 1024)) as i64 + 50;
    for seq in 1..=n {
        let mut kind = completed(&format!("n{seq}"), json!({}));
        if let EventKind::NodeCompleted { state_patch, .. } = &mut kind {
            *state_patch = json!({ "p": patch });
        }
        backend
            .append_event(Event::new(id.clone(), seq, kind))
            .await
            .unwrap();
    }
    let state = make_state(backend);
    let (_, first) = get(&state, &format!("/executions/{id}/events")).await;
    let got = first["events"].as_array().unwrap().len() as i64;
    assert!(got < n, "one page held all {n}");
    assert!(
        serde_json::to_vec(&first["events"]).unwrap().len() <= MAX_EVENTS_PAGE_BYTES + 16 * 1024,
        "the page is past its byte budget"
    );
    assert_eq!(first["next_after"], json!(got), "{}", first["next_after"]);
    let (_, second) = get(&state, &format!("/executions/{id}/events?after={got}")).await;
    assert_eq!(
        got + second["events"].as_array().unwrap().len() as i64,
        n,
        "the rest is on the next page"
    );
    assert!(second["next_after"].is_null());
}

/// Counts artifact reads.
struct CountReads(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl support::hooked::AfterWrite for CountReads {
    async fn after(&self, write: support::hooked::Write) {
        if let support::hooked::Write::GetArtifact(_) = write {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// A reference past the per-output cap is not read: its stored size, looked
/// up first, decides. A sentinel claiming `size: 0` used to get its whole blob
/// read before the length was checked.
#[tokio::test]
async fn a_reference_past_the_cap_is_not_read() {
    let inner = Arc::new(InMemoryBackend::new());
    let reads = Arc::new(CountReads(std::sync::atomic::AtomicUsize::new(0)));
    let backend: Arc<dyn StateBackend> = Arc::new(support::hooked::HookedBackend::new(
        inner.clone(),
        reads.clone(),
    ));
    let e = execution(WorkflowStatus::Running);
    let id = e.execution_id.clone();
    inner.create_execution(e).await.unwrap();
    let big = json!({ "content": "z".repeat(MAX_INLINE_ARTIFACT_BYTES as usize + 1024) });
    let r = inner
        .put_artifact(&serde_json::to_vec(&big).unwrap(), Some("application/json"))
        .await
        .unwrap();
    let mut sentinel = r.to_sentinel();
    sentinel[jamjet_state::ARTIFACT_SENTINEL_KEY]["size"] = json!(0);
    inner
        .append_event(Event::new(id.clone(), 1, completed("n", sentinel)))
        .await
        .unwrap();
    let state = make_state(backend);
    let (_, body) = get(&state, &format!("/executions/{id}/events")).await;
    let output = &body["events"][0]["kind"]["output"];
    assert!(
        output.get(jamjet_state::ARTIFACT_SENTINEL_KEY).is_some(),
        "{output}"
    );
    assert_eq!(reads.0.load(std::sync::atomic::Ordering::SeqCst), 0);
}
