//! The artifact helpers that need no store: `ArtifactRef` sentinels and the
//! `spill_bytes` threshold.
//!
//! The cases that put, get and resolve artifacts are in the backend suite
//! (`runtime/testkit`, module `artifacts`).

use jamjet_state::artifact::{spill_bytes, ArtifactRef, ARTIFACT_SENTINEL_KEY};
use serde_json::json;

#[test]
fn artifact_ref_sentinel_round_trip() {
    let artifact_ref = ArtifactRef {
        hash: "abc123".repeat(10) + "abcd", // 64 chars
        size: 16384,
        media_type: Some("application/json".to_string()),
    };
    let sentinel = artifact_ref.to_sentinel();
    assert!(sentinel.get(ARTIFACT_SENTINEL_KEY).is_some());

    let parsed = ArtifactRef::from_sentinel(&sentinel).unwrap();
    assert_eq!(parsed, artifact_ref);
}

/// Finding 1 — strict sentinel validation: `from_sentinel` must return `None`
/// for any `$artifact` payload that does not match the exact expected shape.
/// This prevents model-emitted objects that happen to contain `"$artifact"` from
/// being mis-identified as refs and triggering spurious CAS lookups.
#[test]
fn artifact_ref_from_sentinel_returns_none_for_non_sentinel() {
    // Plain objects and scalars without the $artifact key.
    assert!(ArtifactRef::from_sentinel(&json!({"foo": "bar"})).is_none());
    assert!(ArtifactRef::from_sentinel(&json!("just a string")).is_none());
    assert!(ArtifactRef::from_sentinel(&json!(42)).is_none());

    // $artifact present but inner value is not an object.
    assert!(ArtifactRef::from_sentinel(&json!({"$artifact": "foo"})).is_none());

    // $artifact is an object but missing required fields.
    assert!(ArtifactRef::from_sentinel(&json!({"$artifact": {}})).is_none());
    assert!(ArtifactRef::from_sentinel(&json!({"$artifact": {"hash": "short"}})).is_none());

    // $artifact has hash + size but hash is too short (not 64 hex chars).
    // Without the strict-shape check this previously returned Some — the key
    // regression case that motivated Finding 1.
    assert!(
        ArtifactRef::from_sentinel(&json!({"$artifact": {"hash": "tooshort", "size": 10}}))
            .is_none(),
        "short hash with size must not parse as an ArtifactRef"
    );

    // 64-char uppercase hex — fails because sentinel requires lowercase.
    let upper_hash = "A".repeat(64);
    assert!(
        ArtifactRef::from_sentinel(&json!({"$artifact": {"hash": upper_hash, "size": 10}}))
            .is_none(),
        "uppercase hex hash must not parse as an ArtifactRef"
    );
}

#[test]
fn spill_bytes_small_value_stays_inline() {
    let small = json!({"key": "short value"});
    // 1 MiB threshold — tiny value must stay inline.
    assert!(spill_bytes(&small, 1 << 20).is_none());
}

#[test]
fn spill_bytes_large_value_returns_bytes() {
    // A string value serialized to JSON will exceed 8 bytes easily.
    let long_str: String = "x".repeat(100);
    let large = json!(long_str);
    let bytes = spill_bytes(&large, 8).expect("expected Some(bytes) for large value");
    // Bytes must be valid JSON that deserializes back to the same value.
    let roundtripped: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(roundtripped, large);
}

#[test]
fn spill_bytes_at_threshold_boundary() {
    // A value whose serialized form is exactly `threshold` bytes stays inline
    // (not strictly greater than).
    let s = "a".repeat(8); // JSON: "aaaaaaaa" = 10 bytes (with quotes)
    let v = json!(s);
    let serialized = serde_json::to_vec(&v).unwrap();
    let threshold = serialized.len();

    // Exactly at threshold: no spill (> not >=).
    assert!(spill_bytes(&v, threshold).is_none());
    // One byte below: also no spill.
    assert!(spill_bytes(&v, threshold + 1).is_none());
    // One byte above threshold: spill.
    assert!(spill_bytes(&v, threshold - 1).is_some());
}

/// For each of the four shape categories, a value whose serialized size is
/// BELOW the threshold must produce `None` from `spill_bytes` — the value
/// stays inline and no artifact store write is needed.
#[test]
fn below_threshold_all_four_shapes_stay_inline() {
    let large_threshold = 1 << 20; // 1 MiB — all values here fit comfortably

    assert!(
        spill_bytes(&json!("short string"), large_threshold).is_none(),
        "small string must stay inline"
    );
    assert!(
        spill_bytes(&json!({"level1": {"level2": "v"}}), large_threshold).is_none(),
        "small nested object must stay inline"
    );
    assert!(
        spill_bytes(&json!([{"a": 1}, {"b": 2}]), large_threshold).is_none(),
        "small array of objects must stay inline"
    );
    assert!(
        spill_bytes(&json!({"emoji": "🦀"}), large_threshold).is_none(),
        "small unicode value must stay inline"
    );
}
