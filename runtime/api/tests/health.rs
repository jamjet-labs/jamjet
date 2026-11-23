//! `GET /health`: status and version, the build's commit and the feature list.

mod support;

use jamjet_api::features::FEATURES;
use jamjet_state::InMemoryBackend;
use serde_json::{json, Value};
use std::sync::Arc;
use support::{get, make_state, post};

fn state() -> jamjet_api::state::AppState {
    make_state(Arc::new(InMemoryBackend::new()))
}

#[tokio::test]
async fn status_version_commit_and_features() {
    let (st, body) = get(&state(), "/health").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    match &body["commit"] {
        Value::Null => {}
        Value::String(c) => assert!(
            c.len() == 40 && c.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "commit {c}"
        ),
        other => panic!("commit is {other}"),
    }
    let source = body["commit_source"].as_str().unwrap();
    assert!(
        ["build-env", "crate-package", "git", "none"].contains(&source),
        "{source}"
    );
    assert_eq!(body["commit"].is_null(), source == "none");
    // A stranger sees these five keys and no more.
    let mut keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        ["commit", "commit_source", "features", "status", "version"]
    );
    assert_eq!(body["features"], json!(FEATURES));
}

#[tokio::test]
async fn the_feature_list_is_sorted_unique_and_well_formed() {
    assert!(!FEATURES.is_empty());
    for pair in FEATURES.windows(2) {
        assert!(pair[0] < pair[1], "{} is not before {}", pair[0], pair[1]);
    }
    for f in FEATURES {
        assert!(
            !f.is_empty()
                && f.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "bad feature name {f:?}"
        );
    }
}

/// Every name in `FEATURES` needs an arm here that exercises the capability.
#[tokio::test]
async fn every_feature_has_a_guard() {
    let (_, body) = get(&state(), "/health").await;
    for name in body["features"].as_array().unwrap() {
        match name.as_str().unwrap() {
            // A claim that names an execution answers with a reason. An
            // engine without the feature ignores `execution_id` and answers
            // with no reason.
            "claim_by_execution" => {
                let (st, body) = post(
                    &state(),
                    "/work-items/claim",
                    json!({
                        "worker_id": "w",
                        "queue_types": ["q"],
                        "execution_id": "exec_00000000000000000000000000000000",
                    }),
                )
                .await;
                assert_eq!(st, 200, "{body}");
                assert_eq!(body["claimed"], false, "{body}");
                assert!(body["reason"].is_string(), "{body}");
            }
            // Two starts with one key give one run, and the key is echoed.
            // An engine without the feature starts a second run.
            "run_key" => {
                let st = state();
                let ir = support::python_fn_start_ir("wf-health-key");
                let (code, body) = post(&st, "/workflows", json!({ "ir": ir })).await;
                assert!(code.is_success(), "{code} {body}");
                let start = json!({
                    "workflow_id": "wf-health-key",
                    "workflow_version": "0.1.0",
                    "input": {},
                    "run_key": "health:1",
                });
                let (_, first) = post(&st, "/executions", start.clone()).await;
                let (_, second) = post(&st, "/executions", start).await;
                assert!(first["execution_id"].is_string(), "{first}");
                assert_eq!(first["execution_id"], second["execution_id"], "{second}");
                assert_eq!(first["run_key"], "health:1", "{first}");
                assert_eq!(second["run_key"], "health:1", "{second}");
            }
            // A `python_fn` node may opt in to being claimed by its own run:
            // the oldest-first claim leaves its item alone, and the claim
            // that names the run takes it.
            "python_fn_by_execution" => {
                let st = state();
                let mut ir = support::python_fn_start_ir("wf-health-claim");
                ir["nodes"]["py_start"]["kind"]["claim"] = json!("by_execution");
                let (code, body) = post(&st, "/workflows", json!({ "ir": ir })).await;
                assert!(code.is_success(), "{code} {body}");
                let (code, started) = post(
                    &st,
                    "/executions",
                    json!({ "workflow_id": "wf-health-claim", "workflow_version": "0.1.0", "input": {} }),
                )
                .await;
                assert!(code.is_success(), "{code} {started}");
                let execution_id = started["execution_id"].as_str().unwrap();
                let (_, any) = post(
                    &st,
                    "/work-items/claim",
                    json!({ "worker_id": "w", "queue_types": ["python_tool"] }),
                )
                .await;
                assert_eq!(any["claimed"], false, "{any}");
                let (_, own) = post(
                    &st,
                    "/work-items/claim",
                    json!({
                        "worker_id": "w",
                        "queue_types": ["python_tool"],
                        "execution_id": execution_id,
                    }),
                )
                .await;
                assert_eq!(own["claimed"], true, "{own}");
            }
            // A `model` node may be run by its run's own worker: its item
            // goes to the `model_worker` queue with the request the engine
            // built. An engine without the feature refuses `executor`, or
            // runs the step itself, so this claim gets nothing.
            "worker_model_steps" => {
                let st = state();
                let mut ir = support::python_fn_start_ir("wf-health-model");
                ir["nodes"]["py_start"]["kind"] = json!({
                    "type": "model",
                    "model_ref": "openai/gpt-test",
                    "prompt_ref": "Say hello",
                    "output_schema": "",
                    "system_prompt": null,
                    "executor": "worker",
                });
                let (code, body) = post(&st, "/workflows", json!({ "ir": ir })).await;
                assert!(code.is_success(), "{code} {body}");
                let (code, started) = post(
                    &st,
                    "/executions",
                    json!({ "workflow_id": "wf-health-model", "workflow_version": "0.1.0", "input": {} }),
                )
                .await;
                assert!(code.is_success(), "{code} {started}");
                let (_, own) = post(
                    &st,
                    "/work-items/claim",
                    json!({
                        "worker_id": "w",
                        "queue_types": ["model_worker"],
                        "execution_id": started["execution_id"],
                    }),
                )
                .await;
                assert_eq!(own["claimed"], true, "{own}");
                assert_eq!(own["work_item"]["queue_type"], "model_worker", "{own}");
                assert_eq!(own["work_item"]["payload"]["prompt"], "Say hello", "{own}");
            }
            other => panic!("feature {other} has no guard arm in tests/health.rs"),
        }
    }
}
