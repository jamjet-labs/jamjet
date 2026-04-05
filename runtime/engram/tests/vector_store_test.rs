//! Integration tests for `EmbeddedVectorStore`.
//!
//! Uses deterministic pseudo-embeddings so results are reproducible across
//! runs without any external dependencies.

use engram::vector::{VectorFilter, VectorStore};
use engram::vector_embedded::EmbeddedVectorStore;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Deterministic pseudo-random embedding of `dims` dimensions.
fn random_embedding(dims: usize) -> Vec<f32> {
    (0..dims)
        .map(|i| ((i * 7 + 3) % 100) as f32 / 100.0)
        .collect()
}

/// Perturb `base` by adding `noise` to every component.
fn similar_embedding(base: &[f32], noise: f32) -> Vec<f32> {
    base.iter().map(|v| v + noise).collect()
}

// ---------------------------------------------------------------------------
// 1. upsert_and_search_returns_matches
// ---------------------------------------------------------------------------

/// Insert three vectors — two that are very similar and one that is different.
/// Querying with the first vector should return the exact match first and the
/// similar vector second.
#[tokio::test]
async fn upsert_and_search_returns_matches() {
    let store = EmbeddedVectorStore::new(128);
    let filter = VectorFilter::default();

    let base = random_embedding(128);
    let similar = similar_embedding(&base, 0.01);
    // Build a clearly different vector by reversing and scaling
    let different: Vec<f32> = (0..128).map(|i| ((i * 3 + 97) % 100) as f32 / 100.0).collect();

    let id_base = Uuid::new_v4();
    let id_similar = Uuid::new_v4();
    let id_different = Uuid::new_v4();

    store
        .upsert(id_base, base.clone(), serde_json::json!({"label": "base"}))
        .await
        .expect("upsert base");
    store
        .upsert(
            id_similar,
            similar,
            serde_json::json!({"label": "similar"}),
        )
        .await
        .expect("upsert similar");
    store
        .upsert(
            id_different,
            different,
            serde_json::json!({"label": "different"}),
        )
        .await
        .expect("upsert different");

    let results = store
        .search(&base, &filter, 3)
        .await
        .expect("search");

    assert_eq!(results.len(), 3, "should return 3 results");

    // The exact-match vector must be the top result.
    assert_eq!(
        results[0].id, id_base,
        "exact match should be ranked first"
    );
    // The similar vector should be ranked before the different one.
    assert_eq!(
        results[1].id, id_similar,
        "similar vector should be ranked second"
    );
    // Scores must be in descending order.
    assert!(
        results[0].score >= results[1].score,
        "scores must be non-increasing"
    );
    assert!(
        results[1].score >= results[2].score,
        "scores must be non-increasing"
    );
}

// ---------------------------------------------------------------------------
// 2. delete_removes_from_index
// ---------------------------------------------------------------------------

/// After deleting a vector it must not appear in search results.
#[tokio::test]
async fn delete_removes_from_index() {
    let store = EmbeddedVectorStore::new(128);
    let filter = VectorFilter::default();

    let embedding = random_embedding(128);
    let id = Uuid::new_v4();

    store
        .upsert(id, embedding.clone(), serde_json::json!({}))
        .await
        .expect("upsert");

    // Sanity: it appears before deletion.
    let before = store
        .search(&embedding, &filter, 10)
        .await
        .expect("search before delete");
    assert!(
        before.iter().any(|m| m.id == id),
        "entry should be present before deletion"
    );

    store.delete(id).await.expect("delete");

    let after = store
        .search(&embedding, &filter, 10)
        .await
        .expect("search after delete");
    assert!(
        !after.iter().any(|m| m.id == id),
        "entry must not appear after deletion"
    );
}

// ---------------------------------------------------------------------------
// 3. search_respects_top_k
// ---------------------------------------------------------------------------

/// Inserting 10 vectors and searching with top_k=3 must return at most 3 results.
#[tokio::test]
async fn search_respects_top_k() {
    let store = EmbeddedVectorStore::new(128);
    let filter = VectorFilter::default();

    let query = random_embedding(128);

    for i in 0..10_usize {
        let id = Uuid::new_v4();
        // Each vector is a deterministic variation on `i`
        let embedding: Vec<f32> = (0..128)
            .map(|j| ((j * 7 + i * 13 + 3) % 100) as f32 / 100.0)
            .collect();
        store
            .upsert(id, embedding, serde_json::json!({"index": i}))
            .await
            .expect("upsert");
    }

    let results = store
        .search(&query, &filter, 3)
        .await
        .expect("search");

    assert!(
        results.len() <= 3,
        "top_k=3 must return at most 3 results, got {}",
        results.len()
    );
}
