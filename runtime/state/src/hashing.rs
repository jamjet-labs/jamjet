//! Deterministic, cross-binary-stable hashing for idempotency keys and receipts.
//!
//! `canonical_json` recursively sorts object keys and serializes compactly, so two
//! values that differ only in key order hash identically. This is stable across
//! binaries for object/array/string/bool/integer JSON (the shape idempotency keys
//! hash over). Full RFC-8785 ECMAScript number canonicalization (a JCS crate) is a
//! drop-in hardening for receipt material (2i); it is not needed here.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Recursively canonicalize: object keys sorted lexicographically, no whitespace.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                // serde_json::to_string on a string Value escapes correctly.
                out.push_str(&Value::String((*k).clone()).to_string());
                out.push(':');
                write_canonical(&map[*k], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        // Scalars: serde_json's Display is deterministic for these.
        other => out.push_str(&other.to_string()),
    }
}

/// Lowercase hex SHA-256 of the canonical JSON bytes.
pub fn content_hash(value: &Value) -> String {
    let canonical = canonical_json(value);
    let digest = Sha256::digest(canonical.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Lowercase hex SHA-256 of raw bytes. Used by the artifact store to hash
/// the stored blob directly (not canonical JSON), so the hash matches the
/// stored bytes exactly regardless of JSON key ordering.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The deterministic idempotency key for one node occurrence.
///
/// ONE definition, because two transports compute it: `Worker::execute_item`
/// for an in-process node, and `POST /work-items/claim` for a node handed to an
/// external tool worker. If the two ever disagreed the replay guard would look
/// present and do nothing — the recorded effect would be filed under a key no
/// reader ever asks for.
///
/// `step` is the number of `NodeCompleted` events already logged for this node.
/// Retries do not complete, so it is stable across them and advances exactly
/// once per successful loop occurrence.
///
/// `input_hash` is `content_hash` of the execution state at FIRE time — claim
/// time for the external tier, which is the same moment for that transport.
pub fn idempotency_key(run: &str, segment: u64, step: u64, node: &str, input_hash: &str) -> String {
    content_hash(&serde_json::json!({
        "run": run,
        "segment": segment,
        "step": step,
        "node": node,
        // "input", not "input_hash" — the field name is part of the hashed
        // shape, so renaming it silently changes every key ever computed.
        "input": input_hash,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_json_sorts_object_keys() {
        let a = json!({ "b": 1, "a": 2, "c": { "y": 1, "x": 2 } });
        assert_eq!(canonical_json(&a), r#"{"a":2,"b":1,"c":{"x":2,"y":1}}"#);
    }

    #[test]
    fn canonical_json_is_key_order_independent() {
        let a = json!({ "x": 1, "y": [1, 2, { "p": 1, "q": 2 }] });
        let b = json!({ "y": [1, 2, { "q": 2, "p": 1 }], "x": 1 });
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }

    #[test]
    fn content_hash_is_stable_and_order_independent() {
        let a = json!({ "run": "r1", "segment": 0, "step": 3 });
        let b = json!({ "step": 3, "run": "r1", "segment": 0 });
        let h = content_hash(&a);
        assert_eq!(h, content_hash(&b));
        assert_eq!(h.len(), 64); // sha256 hex
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn content_hash_differs_on_value_change() {
        let a = json!({ "step": 3 });
        let b = json!({ "step": 4 });
        assert_ne!(content_hash(&a), content_hash(&b));
    }
}

/// The idempotency key for the NEXT occurrence of `node_id` in this run.
///
/// Both enforcement transports call this: `Worker::execute_item` for the
/// in-process tier and the claim route for the external one. It exists as one
/// function because a key is only useful if the writer and the reader agree —
/// two call sites deriving `step` or `input_hash` slightly differently would
/// file effects under a key no reader asks for, leaving a replay guard that
/// looks present and does nothing.
///
/// `step` counts this node's prior `NodeCompleted` events, so a retry of the
/// same occurrence hashes the same and a genuine second visit (a loop) does
/// not. `input_hash` covers the accumulated state the node will read, so a
/// node whose input changed is a different effect.
///
/// `segment` is passed as 0 deliberately, NOT because the segment is unknown.
/// `start_next_segment` derives each continuation's execution id from
/// `{parent}:{n}` (`runtime/state/src/segment.rs`), so every segment already
/// has a distinct id and `run` alone separates them — passing
/// `segment_number` here would add a component that `run` fully determines.
/// The field stays in the hashed shape because the key is persisted: changing
/// what is hashed makes every effect recorded under the old shape unreadable,
/// and its tool re-fires. `a_later_segment_derives_a_different_key` pins the
/// separation that makes the constant safe.
pub async fn derive_idempotency_key(
    backend: &dyn crate::backend::StateBackend,
    execution_id: &jamjet_core::workflow::ExecutionId,
    node_id: &str,
) -> crate::backend::BackendResult<String> {
    let events = backend.get_events(execution_id).await?;
    let step = events
        .iter()
        .filter(|e| {
            matches!(
                &e.kind,
                crate::event::EventKind::NodeCompleted { node_id: nid, .. } if nid == node_id
            )
        })
        .count() as u64;
    let current_state = backend
        .get_execution(execution_id)
        .await?
        .map(|e| e.current_state)
        .unwrap_or_else(|| serde_json::json!({}));
    Ok(idempotency_key(
        &execution_id.to_string(),
        0,
        step,
        node_id,
        &content_hash(&current_state),
    ))
}
