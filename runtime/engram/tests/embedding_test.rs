//! Tests for `EmbeddingProvider` trait and `MockEmbeddingProvider`.

use engram::embedding::{EmbeddingProvider, MockEmbeddingProvider};

// ---------------------------------------------------------------------------
// 1. mock_produces_correct_dimensions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mock_produces_correct_dimensions() {
    let provider = MockEmbeddingProvider::new(64);
    assert_eq!(provider.dimensions(), 64);

    let texts = ["hello world", "foo bar baz"];
    let embeddings = provider.embed(&texts).await.expect("embed should succeed");

    assert_eq!(embeddings.len(), 2, "one embedding per input text");
    for emb in &embeddings {
        assert_eq!(emb.len(), 64, "each embedding must have 64 dimensions");
    }
}

// ---------------------------------------------------------------------------
// 2. mock_is_deterministic
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mock_is_deterministic() {
    let provider = MockEmbeddingProvider::new(32);
    let text = "the quick brown fox jumps over the lazy dog";

    let first = provider
        .embed(&[text])
        .await
        .expect("embed should succeed");
    let second = provider
        .embed(&[text])
        .await
        .expect("embed should succeed");

    assert_eq!(
        first, second,
        "same text must produce the same embedding every time"
    );
}

// ---------------------------------------------------------------------------
// 3. mock_different_texts_different_embeddings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mock_different_texts_different_embeddings() {
    let provider = MockEmbeddingProvider::new(16);

    let embeddings = provider
        .embed(&["apple", "zebra"])
        .await
        .expect("embed should succeed");

    assert_eq!(embeddings.len(), 2);
    assert_ne!(
        embeddings[0], embeddings[1],
        "different texts must produce different embeddings"
    );
}
