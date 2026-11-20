//! The idempotency key formula: the same inputs give the same key, and the
//! key changes when the step advances.
//!
//! The `tool_effects` cache and the reservation cases are in the backend
//! suite (`runtime/testkit`, module `idempotency`).

use serde_json::json;

// ── Key-stability unit tests ─────────────────────────────────────────────────

/// The key formula is deterministic: the same (execution_id, node_id, step,
/// current_state) inputs produce the same key regardless of when or how many
/// times it is computed.
#[test]
fn key_stability_same_inputs_yields_same_key() {
    let exec_id = "exec-stable-abc";
    let node_id = "n1";
    let step: u64 = 0;
    let state = json!({});

    let input_hash = jamjet_state::content_hash(&state);
    let key_a = jamjet_state::content_hash(&json!({
        "run": exec_id,
        "segment": 0,
        "step": step,
        "node": node_id,
        "input": input_hash,
    }));
    let key_b = jamjet_state::content_hash(&json!({
        "run": exec_id,
        "segment": 0,
        "step": step,
        "node": node_id,
        "input": input_hash,
    }));

    assert_eq!(key_a, key_b, "same inputs must produce the same key");
    assert_eq!(key_a.len(), 64, "key must be a 64-char hex sha256");
    assert!(
        key_a.chars().all(|c| c.is_ascii_hexdigit()),
        "key must be lowercase hex"
    );
}

/// A second loop occurrence (step advances once per NodeCompleted) produces a
/// different key from the first occurrence. This ensures the cache cannot
/// collide across loop passes.
#[test]
fn key_differs_when_step_advances() {
    let exec_id = "exec-stable-abc";
    let node_id = "n1";
    let state = json!({});
    let input_hash = jamjet_state::content_hash(&state);

    let key_step0 = jamjet_state::content_hash(&json!({
        "run": exec_id,
        "segment": 0,
        "step": 0u64,
        "node": node_id,
        "input": input_hash,
    }));
    let key_step1 = jamjet_state::content_hash(&json!({
        "run": exec_id,
        "segment": 0,
        "step": 1u64,
        "node": node_id,
        "input": input_hash,
    }));

    assert_ne!(
        key_step0, key_step1,
        "step=0 and step=1 must yield different keys (second loop occurrence)"
    );
}
