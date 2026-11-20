//! `POST /executions` with a caller's `run_key`: a start that is retried
//! returns the run the first one started instead of starting a second, and
//! `GET /executions?run_key=` finds it.
//!
//! A caller's job that dies after the engine started its run, and before it
//! saw the answer, retries the start. Without a key every retry is a new run
//! that does the same work again.
//!
//! The rules under test: a key names one run, per tenant. The same key with
//! the same workflow, version and input answers 200 with that run, whatever
//! its status. `reuse: "failed_only"` starts a new run once the latest
//! segment of the old one ended without success. Another workflow, version or
//! input under the same key is 409 in every mode. A key is 1 to 200 bytes of
//! `[A-Za-z0-9._:/@+=-]`, compared byte for byte.
mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_api::state::AppState;
use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_ir::workflow::WorkflowIr;
use jamjet_state::backend::StateBackend;
use jamjet_state::{
    Event, EventKind, ExecutionFinish, SqliteBackend, Tenant, TenantId, TenantStatus,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use support::hooked::{AfterWrite, HookedBackend, Write};
use tower::ServiceExt;

// ── Helpers ─────────────────────────────────────────────────────────────────

/// A one-node `python_fn` workflow at `version`.
fn ir(workflow_id: &str, version: &str) -> Value {
    let mut ir = support::python_fn_start_ir(workflow_id);
    ir["version"] = json!(version);
    ir
}

/// A fresh memory backend and a fresh SQLite file, each with its own
/// dev-mode state.
async fn envs() -> Vec<(&'static str, Arc<dyn StateBackend>, AppState)> {
    support::backends()
        .await
        .into_iter()
        .map(|b| {
            let backend = b.dyn_backend();
            (b.name(), backend.clone(), support::make_state(backend))
        })
        .collect()
}

/// Counts the keyed starts that found their key taken: the starts that lost
/// a race with another start of the same key.
///
/// While `hold` is set, each key lookup waits until `parties` lookups have
/// returned, so every racing start judges the same record before any of them
/// writes.
struct RaceHook {
    taken: AtomicUsize,
    hold: AtomicBool,
    reads: tokio::sync::Barrier,
}

impl RaceHook {
    fn new(parties: usize) -> Self {
        Self {
            taken: AtomicUsize::new(0),
            hold: AtomicBool::new(false),
            reads: tokio::sync::Barrier::new(parties),
        }
    }
}

#[async_trait::async_trait]
impl AfterWrite for RaceHook {
    async fn after(&self, write: Write) {
        match write {
            Write::StartKeyed { taken: true, .. } => {
                self.taken.fetch_add(1, Ordering::SeqCst);
            }
            Write::GetRunKey if self.hold.load(Ordering::SeqCst) => {
                tokio::time::timeout(std::time::Duration::from_secs(10), self.reads.wait())
                    .await
                    .expect("the racing starts did not all read the key");
            }
            _ => {}
        }
    }
}

/// [`envs`] with every request going through a [`HookedBackend`] with a
/// [`RaceHook`] for `parties` racing starts. The backend returned is the
/// inner one.
async fn hooked_envs(
    parties: usize,
) -> Vec<(&'static str, Arc<dyn StateBackend>, AppState, Arc<RaceHook>)> {
    support::backends()
        .await
        .into_iter()
        .map(|b| {
            let backend = b.dyn_backend();
            let hook = Arc::new(RaceHook::new(parties));
            let hooked: Arc<dyn StateBackend> =
                Arc::new(HookedBackend::new(backend.clone(), hook.clone()));
            (b.name(), backend, support::make_state(hooked), hook)
        })
        .collect()
}

/// A request in dev mode with `body` sent as these exact bytes, so a test can
/// choose the key order of the JSON it sends.
async fn send_raw(state: &AppState, method: &str, path: &str, body: &str) -> (StatusCode, Value) {
    let resp = build_router_with_opts(state.clone(), true)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
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

async fn register(state: &AppState, workflow_id: &str, version: &str) {
    let (st, body) = support::post(
        state,
        "/workflows",
        json!({ "ir": ir(workflow_id, version) }),
    )
    .await;
    assert!(
        st.is_success(),
        "register {workflow_id} v{version}: {st} {body}"
    );
}

async fn start(state: &AppState, body: Value) -> (StatusCode, Value) {
    send_raw(state, "POST", "/executions", &body.to_string()).await
}

/// A start of `workflow_id` v1.0.0 with `input` under `key`, `reuse` sent
/// when given.
fn keyed(workflow_id: &str, input: Value, key: &str, reuse: Option<&str>) -> Value {
    let mut body = json!({
        "workflow_id": workflow_id,
        "workflow_version": "1.0.0",
        "input": input,
        "run_key": key,
    });
    if let Some(r) = reuse {
        body["reuse"] = json!(r);
    }
    body
}

/// Percent-encode a query value: `+` would otherwise read as a space.
fn query_value(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn lookup(state: &AppState, key: &str) -> (StatusCode, Value) {
    support::get(state, &format!("/executions?run_key={}", query_value(key))).await
}

/// `exec_` and 32 lowercase hex digits: the id `POST /executions` hands out.
fn is_exec_id(s: &str) -> bool {
    s.strip_prefix("exec_").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// The uuid of an execution id in either form the API writes: `exec_<hex>`
/// or a hyphenated uuid (a listed execution row).
fn uuid_of(v: &Value) -> uuid::Uuid {
    let s = v
        .as_str()
        .unwrap_or_else(|| panic!("not an execution id: {v}"));
    uuid::Uuid::parse_str(s.strip_prefix("exec_").unwrap_or(s))
        .unwrap_or_else(|_| panic!("not an execution id: {s}"))
}

fn exec_id(s: &str) -> ExecutionId {
    ExecutionId(uuid_of(&json!(s)))
}

/// The answer for a start that made a new run under `key`: 201 and exactly
/// `{execution_id, run_key, created: true}`. Returns the id.
fn assert_created(what: &str, st: StatusCode, body: &Value, key: &str) -> String {
    assert_eq!(st, StatusCode::CREATED, "{what}: {body}");
    let id = body["execution_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(is_exec_id(&id), "{what}: {body}");
    assert_eq!(
        body,
        &json!({ "execution_id": id, "run_key": key, "created": true }),
        "{what}"
    );
    id
}

/// The answer for a start that found the run `key` names: 200 and exactly
/// `{execution_id, current_execution_id, run_key, created: false, status}`.
fn assert_existing(
    what: &str,
    st: StatusCode,
    body: &Value,
    key: &str,
    id: &str,
    current: &str,
    status: &str,
) {
    assert_eq!(st, StatusCode::OK, "{what}: {body}");
    assert_eq!(
        body,
        &json!({
            "execution_id": id,
            "current_execution_id": current,
            "run_key": key,
            "created": false,
            "status": status,
        }),
        "{what}"
    );
}

/// 409 with the refusal's exact text.
fn assert_other_run(what: &str, st: StatusCode, body: &Value, key: &str) {
    assert_eq!(st, StatusCode::CONFLICT, "{what}: {body}");
    assert_eq!(
        body,
        &json!({
            "error": format!("run_key '{key}' names a run of another workflow, version or input")
        }),
        "{what}"
    );
}

/// 400 whose message names `field`.
fn assert_refused(what: &str, st: StatusCode, body: &Value, field: &str) {
    assert_eq!(st, StatusCode::BAD_REQUEST, "{what}: {body}");
    let message = body["error"].as_str().unwrap_or_default();
    assert!(message.contains(field), "{what}: {body}");
}

async fn runs(backend: &Arc<dyn StateBackend>) -> usize {
    backend.list_executions(None, 200, 0).await.unwrap().len()
}

async fn status_of(backend: &Arc<dyn StateBackend>, id: &str) -> WorkflowStatus {
    backend
        .get_execution(&exec_id(id))
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no execution {id}"))
        .status
}

/// End a live run with `status` and its terminal event, in one write, as the
/// engine ends one.
async fn finish(backend: &Arc<dyn StateBackend>, id: &str, status: WorkflowStatus) {
    let kind = match status {
        WorkflowStatus::Completed => EventKind::WorkflowCompleted {
            final_state: json!({}),
        },
        WorkflowStatus::Failed => EventKind::WorkflowFailed {
            error: "the step failed".into(),
        },
        WorkflowStatus::Cancelled => EventKind::WorkflowCancelled { reason: None },
        other => panic!("not a status this helper ends a run with: {other:?}"),
    };
    let done = backend
        .finish_execution(Event::new(exec_id(id), 0, kind), status)
        .await
        .unwrap();
    assert!(matches!(done, ExecutionFinish::Finished(_)), "{done:?}");
}

// ── The same key ────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_same_key_and_input_returns_the_same_run() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-key", "1.0.0").await;
        let body = keyed("wf-key", json!({ "order": 42 }), "order-42:v1", None);
        let (st1, first) = start(&state, body.clone()).await;
        let (st2, second) = start(&state, body).await;
        assert_eq!(st1, StatusCode::CREATED, "{name}: {first}");
        assert_eq!(
            st2,
            StatusCode::OK,
            "{name}: a retried start made a second run: {second}"
        );
        let id = assert_created(name, st1, &first, "order-42:v1");
        assert_existing(name, st2, &second, "order-42:v1", &id, &id, "running");
        assert_eq!(runs(&backend).await, 1, "{name}");
    }
}

#[tokio::test]
async fn the_same_key_with_other_input_or_version_or_workflow_is_409() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-a", "1.0.0").await;
        register(&state, "wf-a", "2.0.0").await;
        register(&state, "wf-b", "1.0.0").await;
        for reuse in [None, Some("never"), Some("failed_only")] {
            let key = format!("job-7:{}", reuse.unwrap_or("default"));
            let what = format!("{name} reuse {reuse:?}");
            let (st, body) = start(&state, keyed("wf-a", json!({ "n": 1 }), &key, reuse)).await;
            assert_created(&what, st, &body, &key);

            let (st, body) = start(&state, keyed("wf-a", json!({ "n": 2 }), &key, reuse)).await;
            assert_other_run(&format!("{what}: other input"), st, &body, &key);

            let mut other_version = keyed("wf-a", json!({ "n": 1 }), &key, reuse);
            other_version["workflow_version"] = json!("2.0.0");
            let (st, body) = start(&state, other_version).await;
            assert_other_run(&format!("{what}: other version"), st, &body, &key);

            let (st, body) = start(&state, keyed("wf-b", json!({ "n": 1 }), &key, reuse)).await;
            assert_other_run(&format!("{what}: other workflow"), st, &body, &key);
        }
        assert_eq!(
            runs(&backend).await,
            3,
            "{name}: one run per key, none for a 409"
        );
    }
}

#[tokio::test]
async fn input_key_order_does_not_change_the_hash() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-order", "1.0.0").await;
        let (st, first) = send_raw(
            &state,
            "POST",
            "/executions",
            r#"{"workflow_id":"wf-order","workflow_version":"1.0.0","run_key":"ord:1","input":{"a":1,"b":{"x":true,"y":[1,2]},"c":"z"}}"#,
        )
        .await;
        let id = assert_created(name, st, &first, "ord:1");
        let (st, again) = send_raw(
            &state,
            "POST",
            "/executions",
            r#"{"input":{"c":"z","b":{"y":[1,2],"x":true},"a":1},"run_key":"ord:1","workflow_version":"1.0.0","workflow_id":"wf-order"}"#,
        )
        .await;
        assert_existing(name, st, &again, "ord:1", &id, &id, "running");
        // Array order is data, not key order.
        let (st, body) = send_raw(
            &state,
            "POST",
            "/executions",
            r#"{"workflow_id":"wf-order","workflow_version":"1.0.0","run_key":"ord:1","input":{"a":1,"b":{"x":true,"y":[2,1]},"c":"z"}}"#,
        )
        .await;
        assert_other_run(name, st, &body, "ord:1");
        assert_eq!(runs(&backend).await, 1, "{name}");
    }
}

#[tokio::test]
async fn engine_keys_in_the_input_do_not_change_the_hash() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-engine", "1.0.0").await;
        let (st, body) = start(&state, keyed("wf-engine", json!({ "q": 1 }), "eng:1", None)).await;
        let id = assert_created(name, st, &body, "eng:1");
        for input in [
            json!({ "q": 1, "__budget": { "total_tokens": 900 } }),
            json!({ "q": 1, "__approvals": { "pay": { "decision": "approved" } } }),
        ] {
            let (st, body) = start(&state, keyed("wf-engine", input.clone(), "eng:1", None)).await;
            assert_existing(
                &format!("{name}: {input}"),
                st,
                &body,
                "eng:1",
                &id,
                &id,
                "running",
            );
        }
        // Only the engine's own keys: a caller's key with the same prefix is input.
        let (st, body) = start(
            &state,
            keyed(
                "wf-engine",
                json!({ "q": 1, "__note": "mine" }),
                "eng:1",
                None,
            ),
        )
        .await;
        assert_other_run(name, st, &body, "eng:1");

        // Sent first with an engine key, then without.
        let (st, body) = start(
            &state,
            keyed(
                "wf-engine",
                json!({ "q": 2, "__budget": { "x": 1 } }),
                "eng:2",
                None,
            ),
        )
        .await;
        let id2 = assert_created(name, st, &body, "eng:2");
        let (st, body) = start(&state, keyed("wf-engine", json!({ "q": 2 }), "eng:2", None)).await;
        assert_existing(name, st, &body, "eng:2", &id2, &id2, "running");
        assert_eq!(runs(&backend).await, 2, "{name}");
    }
}

#[tokio::test]
async fn omitting_the_version_equals_sending_1_0_0() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-ver", "1.0.0").await;
        let mut no_version = keyed("wf-ver", json!({ "v": 1 }), "ver:1", None);
        no_version
            .as_object_mut()
            .unwrap()
            .remove("workflow_version");

        let (st, body) = start(&state, no_version.clone()).await;
        let id = assert_created(name, st, &body, "ver:1");
        let (st, body) = start(&state, keyed("wf-ver", json!({ "v": 1 }), "ver:1", None)).await;
        assert_existing(name, st, &body, "ver:1", &id, &id, "running");

        // And the other way round.
        let (st, body) = start(&state, keyed("wf-ver", json!({ "v": 2 }), "ver:2", None)).await;
        let id2 = assert_created(name, st, &body, "ver:2");
        let mut again = no_version.clone();
        again["input"] = json!({ "v": 2 });
        again["run_key"] = json!("ver:2");
        let (st, body) = start(&state, again).await;
        assert_existing(name, st, &body, "ver:2", &id2, &id2, "running");
        assert_eq!(runs(&backend).await, 2, "{name}");
    }
}

/// The key stores the SHA-256 of the input's canonical JSON (keys sorted, no
/// whitespace, UTF-8 as is) without the engine's keys. The value is stored,
/// so a release that hashed differently would turn every retry across the
/// upgrade into a 409. The literal was computed outside the engine:
/// `printf '%s' '{"note":"café","order":{"id":7,"lines":[{"qty":2,"sku":"x-1"}]}}' | shasum -a 256`.
#[tokio::test]
async fn the_key_stores_the_hash_of_the_input_without_the_engine_keys() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-hash", "1.0.0").await;
        let mut body = keyed(
            "wf-hash",
            json!({
                "order": { "lines": [{ "sku": "x-1", "qty": 2 }], "id": 7 },
                "note": "caf\u{e9}",
                "__budget": { "total_tokens": 10 },
                "__approvals": {},
            }),
            "hash:1",
            None,
        );
        body.as_object_mut().unwrap().remove("workflow_version");
        let (st, answer) = start(&state, body).await;
        let id = assert_created(name, st, &answer, "hash:1");
        let held = backend
            .get_run_key("hash:1")
            .await
            .unwrap()
            .expect("the key is stored");
        assert_eq!(
            held,
            jamjet_state::RunKeyRecord {
                execution_id: exec_id(&id),
                workflow_id: "wf-hash".into(),
                workflow_version: "1.0.0".into(),
                input_hash: "6ed7f11dbed9cd291fa18c6b279e83369ab10bb303f9d2b6211a214c0086f6a3"
                    .into(),
            },
            "{name}"
        );
    }
}

// ── Finished runs and `reuse` ───────────────────────────────────────────────

#[tokio::test]
async fn a_finished_run_answers_200_under_never() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-done", "1.0.0").await;
        for (status, text) in [
            (WorkflowStatus::Completed, "completed"),
            (WorkflowStatus::Failed, "failed"),
            (WorkflowStatus::Cancelled, "cancelled"),
        ] {
            let key = format!("done:{text}");
            let what = format!("{name} {text}");
            let (st, body) = start(&state, keyed("wf-done", json!({}), &key, None)).await;
            let id = assert_created(&what, st, &body, &key);
            finish(&backend, &id, status).await;
            for reuse in [None, Some("never")] {
                let (st, body) = start(&state, keyed("wf-done", json!({}), &key, reuse)).await;
                assert_existing(
                    &format!("{what} reuse {reuse:?}"),
                    st,
                    &body,
                    &key,
                    &id,
                    &id,
                    text,
                );
            }
        }
        assert_eq!(runs(&backend).await, 3, "{name}");
    }
}

#[tokio::test]
async fn failed_only_restarts_after_failure_and_after_cancel() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-retry", "1.0.0").await;
        for ending in ["failed", "cancelled"] {
            let key = format!("retry:{ending}");
            let what = format!("{name} {ending}");
            let (st, body) = start(&state, keyed("wf-retry", json!({ "n": 1 }), &key, None)).await;
            let old = assert_created(&what, st, &body, &key);
            if ending == "failed" {
                finish(&backend, &old, WorkflowStatus::Failed).await;
            } else {
                let (st, body) =
                    support::post(&state, &format!("/executions/{old}/cancel"), json!({})).await;
                assert_eq!(st, StatusCode::OK, "{what}: {body}");
            }

            let (st, body) = start(
                &state,
                keyed("wf-retry", json!({ "n": 1 }), &key, Some("failed_only")),
            )
            .await;
            let new = assert_created(&format!("{what}: restart"), st, &body, &key);
            assert_ne!(new, old, "{what}");

            // The key names the new run from now on, in every mode.
            for reuse in [None, Some("failed_only")] {
                let (st, body) =
                    start(&state, keyed("wf-retry", json!({ "n": 1 }), &key, reuse)).await;
                assert_existing(&what, st, &body, &key, &new, &new, "running");
            }
            let (_, found) = lookup(&state, &key).await;
            let listed = found["executions"].as_array().cloned().unwrap_or_default();
            assert_eq!(listed.len(), 1, "{what}: {found}");
            assert_eq!(
                uuid_of(&listed[0]["execution_id"]),
                exec_id(&new).0,
                "{what}: {found}"
            );

            // The old run keeps its id, its status and its log.
            let ended = if ending == "failed" {
                WorkflowStatus::Failed
            } else {
                WorkflowStatus::Cancelled
            };
            assert_eq!(status_of(&backend, &old).await, ended, "{what}");
            assert!(
                !backend.get_events(&exec_id(&old)).await.unwrap().is_empty(),
                "{what}"
            );
        }
        assert_eq!(runs(&backend).await, 4, "{name}");
    }
}

/// `limit_exceeded` with no next segment ended without success too; a run
/// that rolled over to a next segment did not end (see
/// `a_rolled_over_run_answers_its_latest_segment`).
#[tokio::test]
async fn failed_only_restarts_after_a_limit_with_no_next_segment() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-limit", "1.0.0").await;
        let (st, body) = start(&state, keyed("wf-limit", json!({}), "limit:1", None)).await;
        let old = assert_created(name, st, &body, "limit:1");
        let eid = exec_id(&old);
        let seq = backend.latest_sequence(&eid).await.unwrap() + 1;
        backend
            .append_event(Event::new(
                eid.clone(),
                seq,
                EventKind::StrategyLimitHit {
                    limit_type: "max_iterations".into(),
                    limit_value: json!(3),
                    actual_value: json!(3),
                },
            ))
            .await
            .unwrap();
        backend
            .update_execution_status(&eid, WorkflowStatus::LimitExceeded)
            .await
            .unwrap();

        let (st, body) = start(&state, keyed("wf-limit", json!({}), "limit:1", None)).await;
        assert_existing(name, st, &body, "limit:1", &old, &old, "limit_exceeded");
        let (st, body) = start(
            &state,
            keyed("wf-limit", json!({}), "limit:1", Some("failed_only")),
        )
        .await;
        let new = assert_created(name, st, &body, "limit:1");
        assert_ne!(new, old, "{name}");
        assert_eq!(runs(&backend).await, 2, "{name}");
    }
}

#[tokio::test]
async fn failed_only_on_a_completed_or_running_run_is_200() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-keep", "1.0.0").await;
        let (st, body) = start(&state, keyed("wf-keep", json!({}), "keep:running", None)).await;
        let running = assert_created(name, st, &body, "keep:running");
        let (st, body) = start(
            &state,
            keyed("wf-keep", json!({}), "keep:running", Some("failed_only")),
        )
        .await;
        assert_existing(
            name,
            st,
            &body,
            "keep:running",
            &running,
            &running,
            "running",
        );

        let (st, body) = start(&state, keyed("wf-keep", json!({}), "keep:done", None)).await;
        let done = assert_created(name, st, &body, "keep:done");
        finish(&backend, &done, WorkflowStatus::Completed).await;
        let (st, body) = start(
            &state,
            keyed("wf-keep", json!({}), "keep:done", Some("failed_only")),
        )
        .await;
        assert_existing(name, st, &body, "keep:done", &done, &done, "completed");
        assert_eq!(runs(&backend).await, 2, "{name}");
    }
}

#[tokio::test]
async fn failed_only_with_other_input_after_failure_is_409() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-other", "1.0.0").await;
        let (st, body) = start(
            &state,
            keyed("wf-other", json!({ "n": 1 }), "other:1", None),
        )
        .await;
        let old = assert_created(name, st, &body, "other:1");
        finish(&backend, &old, WorkflowStatus::Failed).await;
        let (st, body) = start(
            &state,
            keyed(
                "wf-other",
                json!({ "n": 2 }),
                "other:1",
                Some("failed_only"),
            ),
        )
        .await;
        assert_other_run(name, st, &body, "other:1");
        assert_eq!(runs(&backend).await, 1, "{name}");
    }
}

// ── At once ─────────────────────────────────────────────────────────────────

/// `n` starts with `body`, released together.
async fn at_once(state: &AppState, body: Value, n: usize) -> Vec<(StatusCode, Value)> {
    at_once_each(state, vec![body; n]).await
}

/// One start with each of `bodies`, released together; the answers in the
/// same order.
async fn at_once_each(state: &AppState, bodies: Vec<Value>) -> Vec<(StatusCode, Value)> {
    let barrier = Arc::new(tokio::sync::Barrier::new(bodies.len()));
    let tasks: Vec<_> = bodies
        .into_iter()
        .map(|body| {
            let state = state.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                start(&state, body).await
            })
        })
        .collect();
    let mut answers = Vec::new();
    for t in tasks {
        answers.push(t.await.unwrap());
    }
    answers
}

/// The eight callers are left to race as they will, so a round mixes starts
/// that read the key already held (answered at once) and starts that read it
/// free and found it taken at their write (`KeyedStart::Taken`). At least one
/// of the latter per backend shows the race opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn eight_callers_at_once_make_one_run() {
    for (name, backend, state, hook) in hooked_envs(8).await {
        register(&state, "wf-race", "1.0.0").await;
        for round in 0..50 {
            let key = format!("race:{round}");
            let what = format!("{name} round {round}");
            let answers = at_once(
                &state,
                keyed("wf-race", json!({ "round": round }), &key, None),
                8,
            )
            .await;
            let created: Vec<_> = answers
                .iter()
                .filter(|(st, _)| *st == StatusCode::CREATED)
                .collect();
            assert_eq!(created.len(), 1, "{what}: {answers:?}");
            let id = assert_created(&what, created[0].0, &created[0].1, &key);
            let others: Vec<_> = answers
                .iter()
                .filter(|(st, _)| *st != StatusCode::CREATED)
                .collect();
            assert_eq!(others.len(), 7, "{what}: {answers:?}");
            for (st, body) in others {
                assert_existing(&what, *st, body, &key, &id, &id, "running");
            }
        }
        assert_eq!(runs(&backend).await, 50, "{name}");
        let taken = hook.taken.load(Ordering::SeqCst);
        eprintln!("{name}: {taken} of 350 losing starts found the key taken at their write");
        assert!(
            taken > 0,
            "{name}: no start found the key taken, so the race never opened"
        );
    }
}

/// Both restarts read the key while it still names the failed run (each
/// lookup waits for the other), so both choose to restart and both write
/// with `replaces` naming it. The key moves once; the other start finds it
/// taken and answers with the new run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_failed_only_restarts_at_once_make_one_new_run() {
    for (name, backend, state, hook) in hooked_envs(2).await {
        register(&state, "wf-twice", "1.0.0").await;
        for round in 0..25 {
            let key = format!("twice:{round}");
            let what = format!("{name} round {round}");
            let input = json!({ "round": round });
            let (st, body) = start(&state, keyed("wf-twice", input.clone(), &key, None)).await;
            let old = assert_created(&what, st, &body, &key);
            finish(&backend, &old, WorkflowStatus::Failed).await;

            let taken_before = hook.taken.load(Ordering::SeqCst);
            hook.hold.store(true, Ordering::SeqCst);
            let answers = at_once(
                &state,
                keyed("wf-twice", input, &key, Some("failed_only")),
                2,
            )
            .await;
            hook.hold.store(false, Ordering::SeqCst);
            assert_eq!(
                hook.taken.load(Ordering::SeqCst) - taken_before,
                1,
                "{what}: one of the two restarts must find the key taken: {answers:?}"
            );
            let created: Vec<_> = answers
                .iter()
                .filter(|(st, _)| *st == StatusCode::CREATED)
                .collect();
            assert_eq!(created.len(), 1, "{what}: {answers:?}");
            let new = assert_created(&what, created[0].0, &created[0].1, &key);
            assert_ne!(new, old, "{what}");
            let (st, body) = answers
                .iter()
                .find(|(st, _)| *st != StatusCode::CREATED)
                .unwrap();
            assert_existing(&what, *st, body, &key, &new, &new, "running");
        }
        assert_eq!(
            runs(&backend).await,
            50,
            "{name}: one old and one new run per round"
        );
    }
}

/// Two starts with one key and different input, both reading the key free
/// (each lookup waits for the other). One makes the run. The other finds the
/// key taken at its write and is judged against the run that won, so it is
/// 409, never a 200 naming a run of other input.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_starts_at_once_with_other_input_make_one_run_and_one_409() {
    for (name, backend, state, hook) in hooked_envs(2).await {
        register(&state, "wf-split", "1.0.0").await;
        for round in 0..10 {
            let key = format!("split:{round}");
            let what = format!("{name} round {round}");
            let taken_before = hook.taken.load(Ordering::SeqCst);
            hook.hold.store(true, Ordering::SeqCst);
            let answers = at_once_each(
                &state,
                vec![
                    keyed("wf-split", json!({ "side": "a" }), &key, None),
                    keyed("wf-split", json!({ "side": "b" }), &key, None),
                ],
            )
            .await;
            hook.hold.store(false, Ordering::SeqCst);
            assert_eq!(
                hook.taken.load(Ordering::SeqCst) - taken_before,
                1,
                "{what}: both starts must read the key free: {answers:?}"
            );
            let created: Vec<_> = answers
                .iter()
                .filter(|(st, _)| *st == StatusCode::CREATED)
                .collect();
            assert_eq!(created.len(), 1, "{what}: {answers:?}");
            let id = assert_created(&what, created[0].0, &created[0].1, &key);
            let (st, body) = answers
                .iter()
                .find(|(st, _)| *st != StatusCode::CREATED)
                .unwrap();
            assert_other_run(&what, *st, body, &key);
            let (_, found) = lookup(&state, &key).await;
            let listed = found["executions"].as_array().cloned().unwrap_or_default();
            assert_eq!(listed.len(), 1, "{what}: {found}");
            assert_eq!(
                uuid_of(&listed[0]["execution_id"]),
                exec_id(&id).0,
                "{what}"
            );
        }
        assert_eq!(runs(&backend).await, 10, "{name}");
    }
}

// ── The key itself ──────────────────────────────────────────────────────────

#[tokio::test]
async fn key_length_boundaries() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-len", "1.0.0").await;
        for key in ["k".to_string(), "k".repeat(200)] {
            let (st, body) = start(&state, keyed("wf-len", json!({}), &key, None)).await;
            assert_created(&format!("{name}: {} bytes", key.len()), st, &body, &key);
        }
        for key in [String::new(), "k".repeat(201)] {
            let (st, body) = start(&state, keyed("wf-len", json!({}), &key, None)).await;
            assert_refused(
                &format!("{name}: {} bytes", key.len()),
                st,
                &body,
                "run_key",
            );
        }
        // Bytes, not characters: 100 two-byte characters are 200 bytes, and
        // not of the alphabet either.
        let (st, body) = start(&state, keyed("wf-len", json!({}), &"é".repeat(100), None)).await;
        assert_refused(&format!("{name}: 200 bytes of é"), st, &body, "run_key");
        assert_eq!(runs(&backend).await, 2, "{name}");
    }
}

#[tokio::test]
async fn keys_with_odd_characters_are_400() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-odd", "1.0.0").await;
        for key in [
            "a b",
            "a\tb",
            "a\nb",
            "a\u{0}b",
            "a%b",
            "a\"b",
            "a\\b",
            "caf\u{e9}",
            "a\u{1F600}b",
            "a\u{202E}b",
            " ab",
            "ab ",
        ] {
            let (st, body) = start(&state, keyed("wf-odd", json!({}), key, None)).await;
            assert_refused(&format!("{name}: {key:?}"), st, &body, "run_key");
        }
        assert_eq!(
            runs(&backend).await,
            0,
            "{name}: a refused key starts nothing"
        );

        // Every character of the alphabet is a key character.
        let all = "ABCXYZabcxyz0189._:/@+=-";
        let (st, body) = start(&state, keyed("wf-odd", json!({}), all, None)).await;
        let id = assert_created(name, st, &body, all);
        let (st, body) = start(&state, keyed("wf-odd", json!({}), all, None)).await;
        assert_existing(name, st, &body, all, &id, &id, "running");
        assert_eq!(runs(&backend).await, 1, "{name}");
    }
}

#[tokio::test]
async fn keys_are_case_sensitive() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-case", "1.0.0").await;
        let (st, body) = start(&state, keyed("wf-case", json!({}), "A", None)).await;
        let upper = assert_created(name, st, &body, "A");
        let (st, body) = start(&state, keyed("wf-case", json!({}), "a", None)).await;
        let lower = assert_created(name, st, &body, "a");
        assert_ne!(upper, lower, "{name}");
        let (st, body) = start(&state, keyed("wf-case", json!({}), "A", None)).await;
        assert_existing(name, st, &body, "A", &upper, &upper, "running");
        let (st, body) = start(&state, keyed("wf-case", json!({}), "a", None)).await;
        assert_existing(name, st, &body, "a", &lower, &lower, "running");
        assert_eq!(runs(&backend).await, 2, "{name}");
    }
}

#[tokio::test]
async fn reuse_without_a_key_and_unknown_reuse_are_400() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-reuse", "1.0.0").await;
        for reuse in ["never", "failed_only"] {
            let (st, body) = start(
                &state,
                json!({ "workflow_id": "wf-reuse", "workflow_version": "1.0.0", "input": {}, "reuse": reuse }),
            )
            .await;
            assert_refused(
                &format!("{name}: {reuse} without a key"),
                st,
                &body,
                "reuse",
            );
        }
        for reuse in ["always", "Never", "FAILED_ONLY", "failed-only", ""] {
            let (st, body) =
                start(&state, keyed("wf-reuse", json!({}), "reuse:1", Some(reuse))).await;
            assert_refused(&format!("{name}: reuse {reuse:?}"), st, &body, "reuse");
        }
        assert_eq!(runs(&backend).await, 0, "{name}");
    }
}

// ── Lookup ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn lookup_by_key_returns_the_run_or_nothing() {
    for (name, _backend, state) in envs().await {
        register(&state, "wf-find", "1.0.0").await;
        let (st, body) = start(&state, keyed("wf-find", json!({}), "find+me:1", None)).await;
        let id = assert_created(name, st, &body, "find+me:1");
        // A run under another key, and one without a key: neither is listed.
        let (st, body) = start(&state, keyed("wf-find", json!({}), "find:2", None)).await;
        assert_created(name, st, &body, "find:2");
        let (st, _) = start(
            &state,
            json!({ "workflow_id": "wf-find", "workflow_version": "1.0.0", "input": {} }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{name}");

        let (st, found) = lookup(&state, "find+me:1").await;
        assert_eq!(st, StatusCode::OK, "{name}: {found}");
        assert_eq!(found["run_key"], "find+me:1", "{name}: {found}");
        let listed = found["executions"].as_array().cloned().unwrap_or_default();
        assert_eq!(listed.len(), 1, "{name}: {found}");
        assert_eq!(
            uuid_of(&listed[0]["execution_id"]),
            exec_id(&id).0,
            "{name}: {found}"
        );
        assert_eq!(
            uuid_of(&listed[0]["current_execution_id"]),
            exec_id(&id).0,
            "{name}: {found}"
        );
        assert_eq!(listed[0]["workflow_id"], "wf-find", "{name}: {found}");
        assert_eq!(listed[0]["status"], "running", "{name}: {found}");
        assert_eq!(listed[0]["current_status"], "running", "{name}: {found}");

        let (st, none) = lookup(&state, "nobody:1").await;
        assert_eq!(st, StatusCode::OK, "{name}: {none}");
        assert_eq!(
            none,
            json!({ "run_key": "nobody:1", "executions": [] }),
            "{name}"
        );

        // A key is a lookup, not one more filter: nothing beside it is
        // silently ignored.
        for other in ["status=running", "limit=10", "offset=0"] {
            let (st, body) =
                support::get(&state, &format!("/executions?run_key=find%3A2&{other}")).await;
            assert_refused(
                &format!("{name}: run_key and {other}"),
                st,
                &body,
                "run_key",
            );
        }
        // The lookup holds a key to the start's rule.
        for bad in ["", "a%20b", "%20find%3A2", &"k".repeat(201)] {
            let (st, body) = support::get(&state, &format!("/executions?run_key={bad}")).await;
            assert_refused(&format!("{name}: lookup of {bad:?}"), st, &body, "run_key");
        }
    }
}

/// The API admits `default` tokens only, so each router here reads through
/// one tenant's scoped backend over one SQLite file, as `main.rs` wires
/// `backend_for`.
#[tokio::test]
async fn lookup_of_another_tenants_key_is_empty() {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "jamjet_run_key_tenants_{}.db",
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
    let state_of = |tenant: &str| {
        let mut state = support::make_state(sqlite.clone() as Arc<dyn StateBackend>);
        let scoped = sqlite.clone();
        let t = TenantId::from(tenant);
        state.backend_for_fn = Arc::new(move |_caller: &TenantId| {
            Arc::new(scoped.for_tenant(t.clone())) as Arc<dyn StateBackend>
        });
        state
    };
    let (acme, globex, default) = (state_of("acme"), state_of("globex"), state_of("default"));
    for state in [&acme, &globex, &default] {
        register(state, "wf-t", "1.0.0").await;
    }

    let (st, body) = start(&acme, keyed("wf-t", json!({}), "shared:1", None)).await;
    let acme_run = assert_created("acme", st, &body, "shared:1");
    // globex has a run of its own, under another key.
    let (st, body) = start(&globex, keyed("wf-t", json!({}), "globex:own", None)).await;
    assert_created("globex", st, &body, "globex:own");

    for (tenant, state) in [("globex", &globex), ("default", &default)] {
        let (st, body) = lookup(state, "shared:1").await;
        assert_eq!(st, StatusCode::OK, "{tenant}: {body}");
        assert_eq!(
            body,
            json!({ "run_key": "shared:1", "executions": [] }),
            "{tenant} sees another tenant's run"
        );
    }
    let (_, body) = lookup(&acme, "shared:1").await;
    let listed = body["executions"].as_array().cloned().unwrap_or_default();
    assert_eq!(listed.len(), 1, "acme: {body}");
    assert_eq!(
        uuid_of(&listed[0]["execution_id"]),
        exec_id(&acme_run).0,
        "acme: {body}"
    );

    // Keys are per tenant: the same key in globex names a run of its own.
    let (st, body) = start(&globex, keyed("wf-t", json!({}), "shared:1", None)).await;
    let globex_run = assert_created("globex, same key", st, &body, "shared:1");
    assert_ne!(globex_run, acme_run);
    let (st, body) = start(&acme, keyed("wf-t", json!({}), "shared:1", None)).await;
    assert_existing(
        "acme again",
        st,
        &body,
        "shared:1",
        &acme_run,
        &acme_run,
        "running",
    );
    let (_, body) = lookup(&globex, "shared:1").await;
    let listed = body["executions"].as_array().cloned().unwrap_or_default();
    assert_eq!(listed.len(), 1, "globex: {body}");
    assert_eq!(
        uuid_of(&listed[0]["execution_id"]),
        exec_id(&globex_run).0,
        "globex: {body}"
    );
    let _ = std::fs::remove_file(path);
}

// ── Continue-as-new ─────────────────────────────────────────────────────────

/// A run that rolled over is one run: the key names its first segment, and
/// the answer points at the segment running now. The root ended
/// `limit_exceeded` when it rolled over, which is not a failure of the run.
#[tokio::test]
async fn a_rolled_over_run_answers_its_latest_segment() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-roll", "1.0.0").await;
        let (st, body) = start(&state, keyed("wf-roll", json!({ "n": 1 }), "roll:1", None)).await;
        let root_id = assert_created(name, st, &body, "roll:1");
        let root = exec_id(&root_id);

        // Roll the run over as the worker does at a strategy ceiling: the next
        // segment first, then `SegmentBoundary` on the root, then the root
        // ends `limit_exceeded` with its item settled.
        let item = backend
            .claim_work_item("w", &["python_tool"])
            .await
            .unwrap()
            .expect("the start item");
        assert_eq!(item.execution_id, root, "{name}");
        let workflow: WorkflowIr = serde_json::from_value(ir("wf-roll", "1.0.0")).unwrap();
        let start_node = workflow.node("py_start").unwrap();
        let carried = jamjet_state::materialize(backend.as_ref(), &root)
            .await
            .unwrap();
        let payload = jamjet_scheduler::build_work_item_payload(
            &workflow,
            "py_start",
            start_node,
            &carried
                .current_state
                .as_object()
                .cloned()
                .unwrap_or_default(),
            "wf-roll",
            "1.0.0",
        );
        let segment = jamjet_state::start_next_segment(
            backend.as_ref(),
            &root,
            &carried,
            "wf-roll",
            "1.0.0",
            1,
            "py_start",
            "python_tool",
            payload,
            3,
            jamjet_state::DEFAULT_TENANT,
        )
        .await
        .unwrap();
        let seq = backend.latest_sequence(&root).await.unwrap() + 1;
        backend
            .append_event(Event::new(
                root.clone(),
                seq,
                EventKind::SegmentBoundary {
                    segment_number: 1,
                    next_execution_id: segment.to_string(),
                },
            ))
            .await
            .unwrap();
        assert!(backend
            .finalize_rollover_fenced(&root, item.id, item.lease_fence)
            .await
            .unwrap());
        assert_eq!(
            status_of(&backend, &root_id).await,
            WorkflowStatus::LimitExceeded,
            "{name}"
        );
        let segment_id = segment.to_string();

        for reuse in [None, Some("never"), Some("failed_only")] {
            let (st, body) =
                start(&state, keyed("wf-roll", json!({ "n": 1 }), "roll:1", reuse)).await;
            assert_existing(
                &format!("{name} reuse {reuse:?}"),
                st,
                &body,
                "roll:1",
                &root_id,
                &segment_id,
                "running",
            );
        }
        let (_, found) = lookup(&state, "roll:1").await;
        let listed = found["executions"].as_array().cloned().unwrap_or_default();
        assert_eq!(listed.len(), 1, "{name}: {found}");
        assert_eq!(
            uuid_of(&listed[0]["execution_id"]),
            root.0,
            "{name}: {found}"
        );
        assert_eq!(
            uuid_of(&listed[0]["current_execution_id"]),
            segment.0,
            "{name}: {found}"
        );
        // The row is the first segment's, as stored; the run's own status is
        // the latest segment's.
        assert_eq!(listed[0]["status"], "limit_exceeded", "{name}: {found}");
        assert_eq!(listed[0]["current_status"], "running", "{name}: {found}");

        // Once the latest segment fails, the run has failed.
        finish(&backend, &segment_id, WorkflowStatus::Failed).await;
        let (st, body) = start(&state, keyed("wf-roll", json!({ "n": 1 }), "roll:1", None)).await;
        assert_existing(name, st, &body, "roll:1", &root_id, &segment_id, "failed");
        let (st, body) = start(
            &state,
            keyed("wf-roll", json!({ "n": 1 }), "roll:1", Some("failed_only")),
        )
        .await;
        let new = assert_created(name, st, &body, "roll:1");
        assert_ne!(new, root_id, "{name}");
        assert_ne!(new, segment_id, "{name}");
        let (st, body) = start(&state, keyed("wf-roll", json!({ "n": 1 }), "roll:1", None)).await;
        assert_existing(name, st, &body, "roll:1", &new, &new, "running");
    }
}

/// An event this build cannot read (`Unknown`, from a later release) could
/// be the link to a next segment, so the segment running now is not known.
/// The key's answers fail closed: no restart, no guessed segment.
#[tokio::test]
async fn a_log_this_build_cannot_read_answers_an_error_and_starts_nothing() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-unread", "1.0.0").await;
        let (st, body) = start(&state, keyed("wf-unread", json!({}), "unread:1", None)).await;
        let id = assert_created(name, st, &body, "unread:1");
        let eid = exec_id(&id);
        let seq = backend.latest_sequence(&eid).await.unwrap() + 1;
        backend
            .append_event(Event::new(eid, seq, EventKind::Unknown))
            .await
            .unwrap();
        finish(&backend, &id, WorkflowStatus::Failed).await;

        for reuse in [None, Some("failed_only")] {
            let (st, body) = start(&state, keyed("wf-unread", json!({}), "unread:1", reuse)).await;
            assert_eq!(
                st,
                StatusCode::INTERNAL_SERVER_ERROR,
                "{name} reuse {reuse:?}: {body}"
            );
            let message = body["error"].as_str().unwrap_or_default();
            assert!(message.contains("cannot read"), "{name}: {body}");
        }
        let (st, body) = lookup(&state, "unread:1").await;
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{name}: {body}");
        assert_eq!(runs(&backend).await, 1, "{name}: a restart was made");
    }
}

/// A `SegmentBoundary` is followed only to the run that is its segment's
/// next one (the run's `parent_execution_id` names the segment linking to
/// it), so a stray link never makes the key answer with an unrelated run.
#[tokio::test]
async fn a_link_to_a_run_that_is_not_the_next_segment_is_refused() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-link", "1.0.0").await;
        let (st, body) = start(&state, keyed("wf-link", json!({}), "link:1", None)).await;
        let id = assert_created(name, st, &body, "link:1");
        let (st, other) = start(
            &state,
            json!({ "workflow_id": "wf-link", "workflow_version": "1.0.0", "input": {} }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{name}: {other}");
        let eid = exec_id(&id);
        let seq = backend.latest_sequence(&eid).await.unwrap() + 1;
        backend
            .append_event(Event::new(
                eid,
                seq,
                EventKind::SegmentBoundary {
                    segment_number: 1,
                    next_execution_id: other["execution_id"].as_str().unwrap().to_string(),
                },
            ))
            .await
            .unwrap();

        let (st, body) = start(&state, keyed("wf-link", json!({}), "link:1", None)).await;
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{name}: {body}");
        let message = body["error"].as_str().unwrap_or_default();
        assert!(message.contains("not one"), "{name}: {body}");
        let (st, body) = lookup(&state, "link:1").await;
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{name}: {body}");
    }
}

// ── Without a key ───────────────────────────────────────────────────────────

/// GUARD: passes before the key exists. A start without `run_key` answers as
/// it always did, and two such starts are two runs.
#[tokio::test]
async fn a_start_without_a_key_is_unchanged() {
    for (name, backend, state) in envs().await {
        register(&state, "wf-plain", "1.0.0").await;
        let body =
            json!({ "workflow_id": "wf-plain", "workflow_version": "1.0.0", "input": { "n": 1 } });
        let (st1, first) = start(&state, body.clone()).await;
        let (st2, second) = start(&state, body).await;
        for (st, answer) in [(st1, &first), (st2, &second)] {
            assert_eq!(st, StatusCode::CREATED, "{name}: {answer}");
            let id = answer["execution_id"].as_str().unwrap_or_default();
            assert!(is_exec_id(id), "{name}: {answer}");
            assert_eq!(
                answer,
                &json!({ "execution_id": id }),
                "{name}: nothing else"
            );
        }
        assert_ne!(first["execution_id"], second["execution_id"], "{name}");
        assert_eq!(runs(&backend).await, 2, "{name}");
    }
}
