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
            // A claim that names an execution answers with a reason, where an
            // engine without the feature answers 400.
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
            other => panic!("feature {other} has no guard arm in tests/health.rs"),
        }
    }
}
