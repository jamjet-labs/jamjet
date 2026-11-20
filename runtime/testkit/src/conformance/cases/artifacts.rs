//! The content-addressed artifact store: put and get, dedupe, tenant
//! isolation, and resolving spilled values.
//!
//! The cases named `tenant_scoped_*` and `tenant_cross_*` need tenant views
//! and run on the scoped stores only. The pure helpers (`ArtifactRef`
//! sentinels, `spill_bytes`) stay in `jamjet-state`'s `tests/artifacts.rs`.

use crate::conformance::cases::support::{in_tenant, tenant_view, without_tenant_views};
use crate::conformance::Fixture;
use jamjet_state::{
    artifact::{resolve_value, spill_bytes, ArtifactRef},
    hashing::sha256_hex,
};
use serde_json::json;

// ── Put and get ───────────────────────────────────────────────────────────────

pub async fn put_returns_stable_hash_and_size(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let bytes = b"hello world";
    let artifact_ref = db.put_artifact(bytes, Some("text/plain")).await.unwrap();

    assert_eq!(artifact_ref.size, 11);
    assert_eq!(artifact_ref.hash.len(), 64);
    assert!(artifact_ref.hash.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(artifact_ref.media_type, Some("text/plain".to_string()));

    // Hash must equal sha256_hex of the raw bytes.
    assert_eq!(artifact_ref.hash, sha256_hex(bytes));
}

pub async fn get_returns_original_bytes(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let bytes = b"hello world";
    let artifact_ref = db.put_artifact(bytes, Some("text/plain")).await.unwrap();

    let retrieved = db.get_artifact(&artifact_ref.hash).await.unwrap();
    assert_eq!(retrieved, Some(bytes.to_vec()));
}

pub async fn put_same_bytes_twice_dedupes(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let bytes = b"deduplication test payload";

    let ref1 = db.put_artifact(bytes, None).await.unwrap();
    let ref2 = db.put_artifact(bytes, None).await.unwrap();

    // Same hash returned both times.
    assert_eq!(ref1.hash, ref2.hash);

    // Only one row in the table: verify via a raw count.
    let rows = fx.hooks.artifact_rows(&ref1.hash, None).await;
    assert_eq!(rows, 1, "expected exactly one row after two identical puts");
}

pub async fn get_missing_hash_returns_none(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let result = db
        .get_artifact("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        .await
        .unwrap();
    assert_eq!(result, None);
}

// ── Tenant views: isolation + per-tenant dedupe ──────────────────────────────

pub async fn tenant_scoped_isolation_a_cannot_read_b_artifact(fx: &Fixture) {
    let (Some(backend_a), Some(backend_b)) = (
        tenant_view(fx, "tenant-a").await,
        tenant_view(fx, "tenant-b").await,
    ) else {
        return without_tenant_views(fx);
    };

    let bytes = b"secret payload for A only";
    let artifact_ref = backend_a.put_artifact(bytes, None).await.unwrap();

    // tenant-B cannot read tenant-A's artifact.
    let result_b = backend_b.get_artifact(&artifact_ref.hash).await.unwrap();
    assert_eq!(result_b, None, "tenant-B must not see tenant-A's artifact");

    // tenant-A can still read it.
    let result_a = backend_a.get_artifact(&artifact_ref.hash).await.unwrap();
    assert_eq!(result_a, Some(bytes.to_vec()));
}

pub async fn tenant_scoped_identical_content_stored_twice(fx: &Fixture) {
    let (Some(backend_a), Some(backend_b)) = (
        tenant_view(fx, "tenant-a").await,
        tenant_view(fx, "tenant-b").await,
    ) else {
        return without_tenant_views(fx);
    };

    let bytes = b"common template value";
    let ref_a = backend_a.put_artifact(bytes, None).await.unwrap();
    let ref_b = backend_b.put_artifact(bytes, None).await.unwrap();

    // Same hash (same content).
    assert_eq!(ref_a.hash, ref_b.hash);

    // But two distinct rows (different tenant_id).
    let rows = fx.hooks.artifact_rows(&ref_a.hash, None).await;
    assert_eq!(
        rows, 2,
        "identical content for two tenants must produce two rows"
    );
}

pub async fn tenant_scoped_put_dedupes_within_same_tenant(fx: &Fixture) {
    let Some(backend) = tenant_view(fx, "tenant-c").await else {
        return without_tenant_views(fx);
    };

    let bytes = b"tenant-c payload";
    let ref1 = backend.put_artifact(bytes, None).await.unwrap();
    let ref2 = backend.put_artifact(bytes, None).await.unwrap();
    assert_eq!(ref1.hash, ref2.hash);

    let rows = fx.hooks.artifact_rows(&ref1.hash, Some("tenant-c")).await;
    assert_eq!(rows, 1, "one row per (tenant, hash)");
}

// ── resolve_value ────────────────────────────────────────────────────────────

/// Finding 1: a non-conforming `$artifact` object must be returned UNCHANGED
/// by `resolve_value`, NOT treated as `unresolved` or yielding a NotFound error.
/// This covers the case where a model emits an object with a `$artifact` key
/// that does not satisfy the strict-shape check (missing size, short hash, etc.).
pub async fn resolve_value_non_conforming_artifact_key_returns_unchanged(fx: &Fixture) {
    let backend = fx.backend.as_ref();

    // Short hash + size: currently would error without the strict check.
    let v = json!({"$artifact": {"hash": "tooshort", "size": 10}});
    let result = resolve_value(&v, backend).await.unwrap();
    assert_eq!(
        result, v,
        "non-conforming $artifact must pass through unchanged"
    );

    // Object with $artifact key but no size: also non-conforming.
    let v2 = json!({"$artifact": {"not": "a ref"}});
    let result2 = resolve_value(&v2, backend).await.unwrap();
    assert_eq!(
        result2, v2,
        "missing-fields $artifact must pass through unchanged"
    );

    // Plain string inside $artifact: non-conforming.
    let v3 = json!({"$artifact": "just a string"});
    let result3 = resolve_value(&v3, backend).await.unwrap();
    assert_eq!(
        result3, v3,
        "string-valued $artifact must pass through unchanged"
    );
}

// ── spill_bytes threshold behaviour ──────────────────────────────────────────

// ── resolve_value round-trip ─────────────────────────────────────────────────

pub async fn resolve_value_round_trip(fx: &Fixture) {
    let db = fx.backend.as_ref();

    let original = json!({
        "content": "a model response that is large enough to be interesting",
        "model": "claude-3-5-sonnet",
        "finish_reason": "end_turn",
    });

    // An array too, as the in-memory round trip used.
    let array = json!(["item1", "item2", {"nested": true}]);

    for original in [original, array] {
        // Spill it (use threshold 8 to force spill).
        let bytes = spill_bytes(&original, 8).expect("value should exceed 8-byte threshold");
        let artifact_ref = db
            .put_artifact(&bytes, Some("application/json"))
            .await
            .unwrap();
        let sentinel = artifact_ref.to_sentinel();

        // Resolve back.
        let resolved = resolve_value(&sentinel, db).await.unwrap();
        assert_eq!(resolved, original);
    }
}

pub async fn resolve_value_non_sentinel_passes_through(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    let plain = json!({"not": "a sentinel"});
    let resolved = resolve_value(&plain, backend).await.unwrap();
    assert_eq!(resolved, plain);
}

pub async fn resolve_value_missing_artifact_returns_error(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let dangling_ref = ArtifactRef {
        hash: "a".repeat(64),
        size: 42,
        media_type: None,
    };
    let sentinel = dangling_ref.to_sentinel();
    let result = resolve_value(&sentinel, db).await;
    assert!(
        result.is_err(),
        "dangling ref must return an error, not panic"
    );
}

pub async fn resolve_value_walks_object_one_level(fx: &Fixture) {
    let db = fx.backend.as_ref();

    let inner = json!({"big": "payload content here"});
    let bytes = spill_bytes(&inner, 8).unwrap();
    let artifact_ref = db
        .put_artifact(&bytes, Some("application/json"))
        .await
        .unwrap();
    let sentinel = artifact_ref.to_sentinel();

    // Wrap in an outer object.
    let outer = json!({ "output": sentinel, "meta": "unchanged" });
    let resolved = resolve_value(&outer, db).await.unwrap();

    assert_eq!(resolved["output"], inner);
    assert_eq!(resolved["meta"], json!("unchanged"));
}

// ─────────────────────────────────────────────────────────────────────────────
// Task 2i-4: new coverage only; does NOT duplicate any 2i-1/2/3 tests.
//
// Not re-tested here (already covered):
//   - put/get basics + basic 2-put dedupe (2i-1: put_*/get_*)
//   - get_artifact tenant isolation (2i-1: tenant_scoped_isolation_*)
//   - sentinel helpers (2i-1: artifact_ref_*)
//   - spill_bytes threshold logic incl. boundary (2i-1: spill_bytes_*)
//   - resolve round-trips (2i-1: resolve_value_round_trip)
//   - dangling-ref (2i-1: resolve_value_missing_artifact_returns_error)
//   - object-level walk (2i-1: resolve_value_walks_object_one_level)
//   - API endpoint resolve (2i-3: artifact_resolve.rs)
//   - worker spill path (2i-2: worker.rs)
// ─────────────────────────────────────────────────────────────────────────────

// == Group A: Round-trip fidelity, four value shapes ==========================
//
// Each shape is exercised through the full chain:
//   spill_bytes → put_artifact → to_sentinel → resolve_value → original value
// All use threshold=8 to force spilling even for tiny payloads.

/// Plain JSON string, well above threshold.
pub async fn round_trip_string_value_above_threshold(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let original = json!("The quick brown fox jumps over the lazy dog. ".repeat(20));

    let bytes = spill_bytes(&original, 8).expect("long string must exceed 8-byte threshold");
    let aref = db
        .put_artifact(&bytes, Some("application/json"))
        .await
        .unwrap();
    let sentinel = aref.to_sentinel();

    let resolved = resolve_value(&sentinel, db).await.unwrap();
    assert_eq!(resolved, original, "string round-trip must be byte-perfect");
}

/// Five-level-deep JSON object, above threshold.
pub async fn round_trip_deeply_nested_object_above_threshold(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let data = "a".repeat(200);
    let original = json!({
        "level1": {
            "level2": {
                "level3": {
                    "level4": {
                        "level5": {
                            "data": data,
                            "tags": ["alpha", "beta", "gamma"],
                            "count": 42
                        }
                    },
                    "sibling": "value at l3"
                }
            },
            "meta": {"created": "2026-06-26", "version": 3}
        }
    });

    let bytes =
        spill_bytes(&original, 8).expect("deeply-nested object must exceed 8-byte threshold");
    let aref = db
        .put_artifact(&bytes, Some("application/json"))
        .await
        .unwrap();
    let sentinel = aref.to_sentinel();

    let resolved = resolve_value(&sentinel, db).await.unwrap();
    assert_eq!(
        resolved, original,
        "deeply-nested object round-trip must be byte-perfect"
    );
}

/// Array of heterogeneous objects, above threshold.
pub async fn round_trip_array_of_objects_above_threshold(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let long_payload = "x".repeat(300);
    let original = json!([
        {"id": 1, "name": "alpha", "tags": ["a", "b"], "active": true},
        {"id": 2, "name": "beta", "scores": [1.1, 2.2, 3.3], "meta": null},
        {"id": 3, "name": "gamma", "nested": {"x": 10, "y": 20}},
        {"id": 4, "payload": long_payload}
    ]);

    let bytes = spill_bytes(&original, 8).expect("array of objects must exceed 8-byte threshold");
    let aref = db
        .put_artifact(&bytes, Some("application/json"))
        .await
        .unwrap();
    let sentinel = aref.to_sentinel();

    let resolved = resolve_value(&sentinel, db).await.unwrap();
    assert_eq!(
        resolved, original,
        "array-of-objects round-trip must be byte-perfect"
    );
}

/// Object containing multi-script unicode, emojis, and JSON escape sequences.
pub async fn round_trip_unicode_and_special_chars_above_threshold(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let long_unicode = "音楽".repeat(100);
    let original = json!({
        "emoji": "🚀🌍🤖🦀",
        "cjk": "日本語テスト中文测试한국어",
        "arabic": "مرحبا بالعالم",
        "escapes": "tab:\there\nnewline and \"quotes\"",
        "long_unicode": long_unicode
    });

    let bytes = spill_bytes(&original, 8).expect("unicode object must exceed 8-byte threshold");
    let aref = db
        .put_artifact(&bytes, Some("application/json"))
        .await
        .unwrap();
    let sentinel = aref.to_sentinel();

    let resolved = resolve_value(&sentinel, db).await.unwrap();
    assert_eq!(
        resolved, original,
        "unicode round-trip must preserve all multi-byte code points exactly"
    );
}

// == Group B: Below-threshold stays inline ====================================

// == Group C: Dedupe at scale + hash stability ================================

/// The same 50 KB blob put 10 times must produce exactly ONE row (INSERT OR
/// IGNORE) and an identical hash on every call (hash stability / content-
/// addressed guarantee). Supplements the existing 2-put test.
pub async fn dedupe_at_scale_fifty_kb_blob_put_ten_times(fx: &Fixture) {
    let db = fx.backend.as_ref();

    // 50 KB of repeating bytes: realistic large model-output size.
    let blob: Vec<u8> = (0u8..=255).cycle().take(50 * 1024).collect();
    let mut reference_hash = String::new();

    for i in 0..10usize {
        let aref = db
            .put_artifact(&blob, Some("application/octet-stream"))
            .await
            .unwrap();
        if i == 0 {
            reference_hash = aref.hash.clone();
        } else {
            assert_eq!(
                aref.hash, reference_hash,
                "hash must be identical on every put (hash stability, call {i})"
            );
        }
    }

    // Exactly one row: INSERT OR IGNORE deduped all subsequent writes.
    let rows = fx.hooks.artifact_rows(&reference_hash, None).await;
    assert_eq!(
        rows, 1,
        "10 identical puts must produce exactly one row (dedupe at scale)"
    );
}

/// Two different 50 KB blobs must each land in their own row with distinct hashes.
pub async fn dedupe_two_distinct_blobs_produce_two_rows_and_different_hashes(fx: &Fixture) {
    let db = fx.backend.as_ref();

    let blob_a: Vec<u8> = vec![0xAA; 50 * 1024];
    let blob_b: Vec<u8> = vec![0xBB; 50 * 1024];

    let ref_a = db.put_artifact(&blob_a, None).await.unwrap();
    let ref_b = db.put_artifact(&blob_b, None).await.unwrap();

    assert_ne!(
        ref_a.hash, ref_b.hash,
        "distinct blobs must hash to distinct values"
    );

    let rows = fx.hooks.artifact_rows(&ref_a.hash, None).await
        + fx.hooks.artifact_rows(&ref_b.hash, None).await;
    assert_eq!(rows, 2, "two distinct blobs must occupy two separate rows");
}

// == Group D: Tenant isolation hard case, resolve_value cross-tenant ==========
//
// 2i-1 tested get_artifact isolation. 2i-4 adds the resolve_value path:
// tenant-B resolving tenant-A's sentinel must fail (no cross-tenant read).

pub async fn tenant_cross_resolve_value_returns_error_no_cross_tenant_read(fx: &Fixture) {
    let (Some(backend_a), Some(backend_b)) = (
        tenant_view(fx, "tenant-a-xr").await,
        tenant_view(fx, "tenant-b-xr").await,
    ) else {
        return without_tenant_views(fx);
    };

    let original = json!({"secret": "tenant-A-only", "data": "x".repeat(200)});
    let bytes = spill_bytes(&original, 8).unwrap();
    let aref = backend_a
        .put_artifact(&bytes, Some("application/json"))
        .await
        .unwrap();
    // B gets A's sentinel but tries to resolve it through B's backend.
    let sentinel_a = aref.to_sentinel();

    let result = resolve_value(&sentinel_a, &*backend_b).await;
    assert!(
        result.is_err(),
        "tenant-B must not resolve tenant-A's sentinel (no cross-tenant read); got Ok"
    );
}

// == Group E: A tenant view's full round-trip =================================
//
// 2i-1 proved put/get per-tenant; 2i-4 proves the full
// spill → put → sentinel → resolve cycle through a tenant view.

pub async fn tenant_scoped_full_spill_resolve_round_trip(fx: &Fixture) {
    let Some(backend) = tenant_view(fx, "tenant-rtt").await else {
        return without_tenant_views(fx);
    };

    let original = json!({
        "tenant": "tenant-rtt",
        "payload": "z".repeat(300),
        "tags": ["tag1", "tag2", "tag3"]
    });

    let bytes = spill_bytes(&original, 8).unwrap();
    let aref = backend
        .put_artifact(&bytes, Some("application/json"))
        .await
        .unwrap();
    let sentinel = aref.to_sentinel();

    let resolved = resolve_value(&sentinel, &*backend).await.unwrap();
    assert_eq!(
        resolved, original,
        "tenant-scoped spill+resolve must be byte-perfect"
    );
}

/// Dangling ref through a tenant view must return Err, not panic.
/// Complements `resolve_value_missing_artifact_returns_error`.
pub async fn tenant_scoped_dangling_ref_returns_error_no_panic(fx: &Fixture) {
    let Some(backend) = tenant_view(fx, "tenant-dangling").await else {
        return without_tenant_views(fx);
    };

    let dangling = ArtifactRef {
        hash: "b".repeat(64),
        size: 100,
        media_type: Some("application/json".to_string()),
    };
    let sentinel = dangling.to_sentinel();

    let result = resolve_value(&sentinel, &*backend).await;
    assert!(
        result.is_err(),
        "dangling ref via a tenant view must return Err, never panic"
    );
}

// == Group F: Canonical media_type on a deduped put ===========================
//
// put_artifact is INSERT OR IGNORE on (tenant_id, hash): a second put of the
// SAME bytes with a DIFFERENT media_type keeps the original row, so the
// returned ArtifactRef must carry the FIRST (stored) media_type, never the
// new request's value that was never persisted. Covers all three backends.

pub async fn deduped_put_returns_stored_media_type(fx: &Fixture) {
    let db = &*in_tenant(fx, "tenant-mt").await;
    let bytes = b"canonical media type payload";

    let ref1 = db.put_artifact(bytes, Some("text/plain")).await.unwrap();
    assert_eq!(ref1.media_type, Some("text/plain".to_string()));

    // Second put of identical bytes with a DIFFERENT media type.
    let ref2 = db
        .put_artifact(bytes, Some("application/json"))
        .await
        .unwrap();
    assert_eq!(ref1.hash, ref2.hash, "same bytes must dedupe to one hash");
    assert_eq!(
        ref2.media_type,
        Some("text/plain".to_string()),
        "deduped put must return the stored media type, not the new request's"
    );
}

/// First store with NO media_type, then re-put the same bytes WITH one: the
/// stored value (NULL) wins, so the returned ref must be None. It exercises the
/// nullable read-back path.
pub async fn deduped_put_returns_stored_null_media_type(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let bytes = b"null media type payload";

    let ref1 = db.put_artifact(bytes, None).await.unwrap();
    assert_eq!(ref1.media_type, None);

    let ref2 = db.put_artifact(bytes, Some("text/plain")).await.unwrap();
    assert_eq!(
        ref2.media_type, None,
        "stored NULL media type must win over a later request's value"
    );
}
