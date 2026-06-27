//! TDD tests for the content-addressed artifact store (Task 2i-1).
//!
//! Written RED (failing) before implementing artifact.rs + backend methods;
//! turned GREEN after implementing put_artifact / get_artifact + ArtifactRef
//! + spill_bytes + resolve_value in all three backends.
//!
//! Coverage:
//! - SQLite backend (unscoped, "default" tenant)
//! - TenantScopedSqliteBackend (two tenants: isolation + per-tenant dedupe)
//! - InMemoryBackend
//! - ArtifactRef sentinel round-trip
//! - spill_bytes threshold behaviour
//! - resolve_value happy-path + missing-artifact error path

use jamjet_state::{
    artifact::{resolve_value, spill_bytes, ArtifactRef, ARTIFACT_SENTINEL_KEY},
    backend::StateBackend,
    hashing::sha256_hex,
    InMemoryBackend, SqliteBackend, TenantId,
};
use serde_json::json;

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn open_db() -> SqliteBackend {
    SqliteBackend::open("sqlite::memory:")
        .await
        .expect("failed to open in-memory SQLite for artifact tests")
}

// ── SQLite backend (unscoped / "default" tenant) ──────────────────────────────

#[tokio::test]
async fn sqlite_put_returns_stable_hash_and_size() {
    let db = open_db().await;
    let bytes = b"hello world";
    let artifact_ref = db.put_artifact(bytes, Some("text/plain")).await.unwrap();

    assert_eq!(artifact_ref.size, 11);
    assert_eq!(artifact_ref.hash.len(), 64);
    assert!(artifact_ref.hash.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(artifact_ref.media_type, Some("text/plain".to_string()));

    // Hash must equal sha256_hex of the raw bytes.
    assert_eq!(artifact_ref.hash, sha256_hex(bytes));
}

#[tokio::test]
async fn sqlite_get_returns_original_bytes() {
    let db = open_db().await;
    let bytes = b"hello world";
    let artifact_ref = db.put_artifact(bytes, Some("text/plain")).await.unwrap();

    let retrieved = db.get_artifact(&artifact_ref.hash).await.unwrap();
    assert_eq!(retrieved, Some(bytes.to_vec()));
}

#[tokio::test]
async fn sqlite_put_same_bytes_twice_dedupes() {
    let db = open_db().await;
    let bytes = b"deduplication test payload";

    let ref1 = db.put_artifact(bytes, None).await.unwrap();
    let ref2 = db.put_artifact(bytes, None).await.unwrap();

    // Same hash returned both times.
    assert_eq!(ref1.hash, ref2.hash);

    // Only one row in the table — verify via a raw count.
    let pool = db.pool();
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM artifacts WHERE hash = ?")
        .bind(&ref1.hash)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.0, 1,
        "expected exactly one row after two identical puts"
    );
}

#[tokio::test]
async fn sqlite_get_missing_hash_returns_none() {
    let db = open_db().await;
    let result = db
        .get_artifact("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        .await
        .unwrap();
    assert_eq!(result, None);
}

// ── TenantScopedSqliteBackend — isolation + per-tenant dedupe ────────────────

#[tokio::test]
async fn tenant_scoped_isolation_a_cannot_read_b_artifact() {
    let db = open_db().await;
    let backend_a = db.for_tenant(TenantId::from("tenant-a"));
    let backend_b = db.for_tenant(TenantId::from("tenant-b"));

    let bytes = b"secret payload for A only";
    let artifact_ref = backend_a.put_artifact(bytes, None).await.unwrap();

    // tenant-B cannot read tenant-A's artifact.
    let result_b = backend_b.get_artifact(&artifact_ref.hash).await.unwrap();
    assert_eq!(result_b, None, "tenant-B must not see tenant-A's artifact");

    // tenant-A can still read it.
    let result_a = backend_a.get_artifact(&artifact_ref.hash).await.unwrap();
    assert_eq!(result_a, Some(bytes.to_vec()));
}

#[tokio::test]
async fn tenant_scoped_identical_content_stored_twice() {
    let db = open_db().await;
    let backend_a = db.for_tenant(TenantId::from("tenant-a"));
    let backend_b = db.for_tenant(TenantId::from("tenant-b"));

    let bytes = b"common template value";
    let ref_a = backend_a.put_artifact(bytes, None).await.unwrap();
    let ref_b = backend_b.put_artifact(bytes, None).await.unwrap();

    // Same hash (same content).
    assert_eq!(ref_a.hash, ref_b.hash);

    // But two distinct rows (different tenant_id).
    let pool = db.pool();
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM artifacts WHERE hash = ?")
        .bind(&ref_a.hash)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.0, 2,
        "identical content for two tenants must produce two rows"
    );
}

#[tokio::test]
async fn tenant_scoped_put_dedupes_within_same_tenant() {
    let db = open_db().await;
    let backend = db.for_tenant(TenantId::from("tenant-c"));

    let bytes = b"tenant-c payload";
    let ref1 = backend.put_artifact(bytes, None).await.unwrap();
    let ref2 = backend.put_artifact(bytes, None).await.unwrap();
    assert_eq!(ref1.hash, ref2.hash);

    let pool = db.pool();
    let row: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM artifacts WHERE hash = ? AND tenant_id = 'tenant-c'")
            .bind(&ref1.hash)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0, 1, "one row per (tenant, hash)");
}

// ── InMemoryBackend ───────────────────────────────────────────────────────────

#[tokio::test]
async fn memory_put_returns_stable_hash_and_size() {
    let backend = InMemoryBackend::new();
    let bytes = b"hello world";
    let artifact_ref = backend
        .put_artifact(bytes, Some("text/plain"))
        .await
        .unwrap();

    assert_eq!(artifact_ref.size, 11);
    assert_eq!(artifact_ref.hash, sha256_hex(bytes));
    assert_eq!(artifact_ref.media_type, Some("text/plain".to_string()));
}

#[tokio::test]
async fn memory_get_returns_original_bytes() {
    let backend = InMemoryBackend::new();
    let bytes = b"memory backend payload";
    let artifact_ref = backend.put_artifact(bytes, None).await.unwrap();

    let retrieved = backend.get_artifact(&artifact_ref.hash).await.unwrap();
    assert_eq!(retrieved, Some(bytes.to_vec()));
}

#[tokio::test]
async fn memory_put_same_bytes_twice_dedupes() {
    let backend = InMemoryBackend::new();
    let bytes = b"dedupe in memory";
    let ref1 = backend.put_artifact(bytes, None).await.unwrap();
    let ref2 = backend.put_artifact(bytes, None).await.unwrap();
    assert_eq!(ref1.hash, ref2.hash);
    // No assertion on row count (DashMap entry = 1 by construction via or_insert_with).
}

#[tokio::test]
async fn memory_get_missing_returns_none() {
    let backend = InMemoryBackend::new();
    let result = backend
        .get_artifact("0000000000000000000000000000000000000000000000000000000000000000")
        .await
        .unwrap();
    assert_eq!(result, None);
}

// ── ArtifactRef sentinel helpers ──────────────────────────────────────────────

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

#[test]
fn artifact_ref_from_sentinel_returns_none_for_non_sentinel() {
    assert!(ArtifactRef::from_sentinel(&json!({"foo": "bar"})).is_none());
    assert!(ArtifactRef::from_sentinel(&json!("just a string")).is_none());
    assert!(ArtifactRef::from_sentinel(&json!(42)).is_none());
}

// ── spill_bytes threshold behaviour ──────────────────────────────────────────

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

// ── resolve_value round-trip ─────────────────────────────────────────────────

#[tokio::test]
async fn sqlite_resolve_value_round_trip() {
    let db = open_db().await;

    let original = json!({
        "content": "a model response that is large enough to be interesting",
        "model": "claude-3-5-sonnet",
        "finish_reason": "end_turn",
    });

    // Spill it (use threshold 8 to force spill).
    let bytes = spill_bytes(&original, 8).expect("value should exceed 8-byte threshold");
    let artifact_ref = db
        .put_artifact(&bytes, Some("application/json"))
        .await
        .unwrap();
    let sentinel = artifact_ref.to_sentinel();

    // Resolve back.
    let resolved = resolve_value(&sentinel, &db).await.unwrap();
    assert_eq!(resolved, original);
}

#[tokio::test]
async fn memory_resolve_value_round_trip() {
    let backend = InMemoryBackend::new();

    let original = json!(["item1", "item2", {"nested": true}]);
    let bytes = spill_bytes(&original, 8).expect("array should exceed 8-byte threshold");
    let artifact_ref = backend
        .put_artifact(&bytes, Some("application/json"))
        .await
        .unwrap();
    let sentinel = artifact_ref.to_sentinel();

    let resolved = resolve_value(&sentinel, &backend).await.unwrap();
    assert_eq!(resolved, original);
}

#[tokio::test]
async fn resolve_value_non_sentinel_passes_through() {
    let backend = InMemoryBackend::new();
    let plain = json!({"not": "a sentinel"});
    let resolved = resolve_value(&plain, &backend).await.unwrap();
    assert_eq!(resolved, plain);
}

#[tokio::test]
async fn resolve_value_missing_artifact_returns_error() {
    let db = open_db().await;
    let dangling_ref = ArtifactRef {
        hash: "a".repeat(64),
        size: 42,
        media_type: None,
    };
    let sentinel = dangling_ref.to_sentinel();
    let result = resolve_value(&sentinel, &db).await;
    assert!(
        result.is_err(),
        "dangling ref must return an error, not panic"
    );
}

#[tokio::test]
async fn resolve_value_walks_object_one_level() {
    let db = open_db().await;

    let inner = json!({"big": "payload content here"});
    let bytes = spill_bytes(&inner, 8).unwrap();
    let artifact_ref = db
        .put_artifact(&bytes, Some("application/json"))
        .await
        .unwrap();
    let sentinel = artifact_ref.to_sentinel();

    // Wrap in an outer object.
    let outer = json!({ "output": sentinel, "meta": "unchanged" });
    let resolved = resolve_value(&outer, &db).await.unwrap();

    assert_eq!(resolved["output"], inner);
    assert_eq!(resolved["meta"], json!("unchanged"));
}

// ─────────────────────────────────────────────────────────────────────────────
// Task 2i-4 — new coverage only; does NOT duplicate any 2i-1/2/3 tests.
//
// Not re-tested here (already covered):
//   - put/get basics + basic 2-put dedupe (2i-1: sqlite_put_*/sqlite_get_*/memory_*)
//   - get_artifact tenant isolation (2i-1: tenant_scoped_isolation_*)
//   - sentinel helpers (2i-1: artifact_ref_*)
//   - spill_bytes threshold logic incl. boundary (2i-1: spill_bytes_*)
//   - sqlite + memory resolve round-trips (2i-1: sqlite_resolve_*/memory_resolve_*)
//   - dangling-ref via unscoped SQLite (2i-1: resolve_value_missing_artifact_returns_error)
//   - object-level walk (2i-1: resolve_value_walks_object_one_level)
//   - API endpoint resolve (2i-3: artifact_resolve.rs)
//   - worker spill path (2i-2: worker.rs)
// ─────────────────────────────────────────────────────────────────────────────

// == Group A: Round-trip fidelity — four value shapes =========================
//
// Each shape is exercised through the full chain:
//   spill_bytes → put_artifact → to_sentinel → resolve_value → original value
// All use threshold=8 to force spilling even for tiny payloads.

/// Plain JSON string, well above threshold.
#[tokio::test]
async fn round_trip_string_value_above_threshold() {
    let db = open_db().await;
    let original = json!("The quick brown fox jumps over the lazy dog. ".repeat(20));

    let bytes = spill_bytes(&original, 8).expect("long string must exceed 8-byte threshold");
    let aref = db
        .put_artifact(&bytes, Some("application/json"))
        .await
        .unwrap();
    let sentinel = aref.to_sentinel();

    let resolved = resolve_value(&sentinel, &db).await.unwrap();
    assert_eq!(resolved, original, "string round-trip must be byte-perfect");
}

/// Five-level-deep JSON object, above threshold.
#[tokio::test]
async fn round_trip_deeply_nested_object_above_threshold() {
    let db = open_db().await;
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

    let resolved = resolve_value(&sentinel, &db).await.unwrap();
    assert_eq!(
        resolved, original,
        "deeply-nested object round-trip must be byte-perfect"
    );
}

/// Array of heterogeneous objects, above threshold.
#[tokio::test]
async fn round_trip_array_of_objects_above_threshold() {
    let db = open_db().await;
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

    let resolved = resolve_value(&sentinel, &db).await.unwrap();
    assert_eq!(
        resolved, original,
        "array-of-objects round-trip must be byte-perfect"
    );
}

/// Object containing multi-script unicode, emojis, and JSON escape sequences.
#[tokio::test]
async fn round_trip_unicode_and_special_chars_above_threshold() {
    let db = open_db().await;
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

    let resolved = resolve_value(&sentinel, &db).await.unwrap();
    assert_eq!(
        resolved, original,
        "unicode round-trip must preserve all multi-byte code points exactly"
    );
}

// == Group B: Below-threshold stays inline ====================================

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

// == Group C: Dedupe at scale + hash stability ================================

/// The same 50 KB blob put 10 times must produce exactly ONE row (INSERT OR
/// IGNORE) and an identical hash on every call (hash stability / content-
/// addressed guarantee). Supplements the existing 2-put test.
#[tokio::test]
async fn dedupe_at_scale_fifty_kb_blob_put_ten_times() {
    let db = open_db().await;

    // 50 KB of repeating bytes — realistic large model-output size.
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

    // Exactly one row — INSERT OR IGNORE deduped all subsequent writes.
    let pool = db.pool();
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM artifacts WHERE hash = ?")
        .bind(&reference_hash)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.0, 1,
        "10 identical puts must produce exactly one row (dedupe at scale)"
    );
}

/// Two different 50 KB blobs must each land in their own row with distinct hashes.
#[tokio::test]
async fn dedupe_two_distinct_blobs_produce_two_rows_and_different_hashes() {
    let db = open_db().await;

    let blob_a: Vec<u8> = vec![0xAA; 50 * 1024];
    let blob_b: Vec<u8> = vec![0xBB; 50 * 1024];

    let ref_a = db.put_artifact(&blob_a, None).await.unwrap();
    let ref_b = db.put_artifact(&blob_b, None).await.unwrap();

    assert_ne!(
        ref_a.hash, ref_b.hash,
        "distinct blobs must hash to distinct values"
    );

    let pool = db.pool();
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM artifacts WHERE hash IN (?, ?)")
        .bind(&ref_a.hash)
        .bind(&ref_b.hash)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, 2, "two distinct blobs must occupy two separate rows");
}

// == Group D: Tenant isolation hard case — resolve_value cross-tenant =========
//
// 2i-1 tested get_artifact isolation. 2i-4 adds the resolve_value path:
// tenant-B resolving tenant-A's sentinel must fail (no cross-tenant read).

#[tokio::test]
async fn tenant_cross_resolve_value_returns_error_no_cross_tenant_read() {
    let db = open_db().await;
    let backend_a = db.for_tenant(TenantId::from("tenant-a-xr"));
    let backend_b = db.for_tenant(TenantId::from("tenant-b-xr"));

    let original = json!({"secret": "tenant-A-only", "data": "x".repeat(200)});
    let bytes = spill_bytes(&original, 8).unwrap();
    let aref = backend_a
        .put_artifact(&bytes, Some("application/json"))
        .await
        .unwrap();
    // B gets A's sentinel but tries to resolve it through B's backend.
    let sentinel_a = aref.to_sentinel();

    let result = resolve_value(&sentinel_a, &backend_b).await;
    assert!(
        result.is_err(),
        "tenant-B must not resolve tenant-A's sentinel (no cross-tenant read); got Ok"
    );
}

// == Group E: Cross-backend — TenantScopedSqliteBackend full round-trip =======
//
// 2i-1 proved put/get per-tenant; 2i-4 proves the full
// spill → put → sentinel → resolve cycle through TenantScopedSqliteBackend.

#[tokio::test]
async fn tenant_scoped_full_spill_resolve_round_trip() {
    let db = open_db().await;
    let backend = db.for_tenant(TenantId::from("tenant-rtt"));

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

    let resolved = resolve_value(&sentinel, &backend).await.unwrap();
    assert_eq!(
        resolved, original,
        "tenant-scoped spill+resolve must be byte-perfect"
    );
}

/// Dangling ref through a TenantScopedSqliteBackend must return Err, not panic.
/// Complements `resolve_value_missing_artifact_returns_error` (unscoped SQLite).
#[tokio::test]
async fn tenant_scoped_dangling_ref_returns_error_no_panic() {
    let db = open_db().await;
    let backend = db.for_tenant(TenantId::from("tenant-dangling"));

    let dangling = ArtifactRef {
        hash: "b".repeat(64),
        size: 100,
        media_type: Some("application/json".to_string()),
    };
    let sentinel = dangling.to_sentinel();

    let result = resolve_value(&sentinel, &backend).await;
    assert!(
        result.is_err(),
        "dangling ref via TenantScopedSqliteBackend must return Err, never panic"
    );
}
