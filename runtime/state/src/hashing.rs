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
