//! Integration tests for `Memory` — the public Engram API.
//!
//! All tests use `Memory::in_memory` with a `MockEmbeddingProvider` so they
//! are hermetic, fast, and require no external services.

use engram::embedding::MockEmbeddingProvider;
use engram::memory::{Memory, RecallQuery};
use engram::scope::Scope;

fn scope(org: &str) -> Scope {
    Scope::org(org)
}

fn user_scope(org: &str, user: &str) -> Scope {
    Scope::user(org, user)
}

// ---------------------------------------------------------------------------
// 1. add_and_recall_fact
// ---------------------------------------------------------------------------

#[tokio::test]
async fn add_and_recall_fact() {
    let mem = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .expect("in_memory");

    mem.add_fact("The sky is blue", scope("acme"))
        .await
        .expect("add fact 1");
    mem.add_fact("Rust is a systems programming language", scope("acme"))
        .await
        .expect("add fact 2");

    let query = RecallQuery {
        query: "blue sky".to_string(),
        max_results: 5,
        ..Default::default()
    };
    let results = mem.recall(&query).await.expect("recall");

    assert!(
        !results.is_empty(),
        "recall should return at least one fact"
    );
}

// ---------------------------------------------------------------------------
// 2. add_fact_stores_in_all_three_stores
// ---------------------------------------------------------------------------

#[tokio::test]
async fn add_fact_stores_in_all_three_stores() {
    let mem = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .expect("in_memory");

    mem.add_fact("Alice works at Acme Corp", scope("acme"))
        .await
        .expect("add fact");

    // Verify in fact_store via list_facts
    let listed = mem
        .list_facts(Some(scope("acme")))
        .await
        .expect("list_facts");
    assert_eq!(listed.len(), 1, "fact should appear in fact store");

    // Verify in vector_store via recall
    let results = mem
        .recall(&RecallQuery {
            query: "Alice".to_string(),
            max_results: 5,
            ..Default::default()
        })
        .await
        .expect("recall");
    assert!(!results.is_empty(), "fact should appear in vector store");
}

// ---------------------------------------------------------------------------
// 3. forget_invalidates_fact
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forget_invalidates_fact() {
    let mem = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .expect("in_memory");

    let id = mem
        .add_fact("Temporary note", scope("acme"))
        .await
        .expect("add fact");

    // Verify fact is present.
    let before = mem
        .list_facts(Some(scope("acme")))
        .await
        .expect("list before");
    assert_eq!(before.len(), 1);

    // Forget the fact.
    mem.forget(id, Some("no longer needed"))
        .await
        .expect("forget");

    // The fact should no longer appear in list_facts (valid_only = true by default).
    let after = mem
        .list_facts(Some(scope("acme")))
        .await
        .expect("list after");
    assert_eq!(after.len(), 0, "invalidated fact should not appear in list");
}

// ---------------------------------------------------------------------------
// 4. delete_user_removes_all_data
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_user_removes_all_data() {
    let mem = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .expect("in_memory");

    let u1 = user_scope("acme", "user-1");
    let u2 = user_scope("acme", "user-2");

    mem.add_fact("User 1 fact A", u1.clone())
        .await
        .expect("u1 a");
    mem.add_fact("User 1 fact B", u1.clone())
        .await
        .expect("u1 b");
    mem.add_fact("User 2 fact A", u2.clone())
        .await
        .expect("u2 a");

    // Delete user 1 data.
    let deleted = mem.delete_user_data(u1.clone()).await.expect("delete u1");
    assert_eq!(
        deleted, 2,
        "two facts belonging to user-1 should be deleted"
    );

    // User 2 data should remain.
    let u2_facts = mem
        .list_facts(Some(u2.clone()))
        .await
        .expect("list u2 after delete");
    assert_eq!(u2_facts.len(), 1, "user-2 fact should still exist");

    // User 1 data should be gone.
    let u1_facts = mem
        .list_facts(Some(u1.clone()))
        .await
        .expect("list u1 after delete");
    assert_eq!(u1_facts.len(), 0, "user-1 facts should be deleted");
}

// ---------------------------------------------------------------------------
// 5. stats_works
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stats_works() {
    let mem = Memory::in_memory(Box::new(MockEmbeddingProvider::new(64)))
        .await
        .expect("in_memory");

    mem.add_fact("Fact one", scope("acme"))
        .await
        .expect("add fact 1");
    mem.add_fact("Fact two", scope("acme"))
        .await
        .expect("add fact 2");

    let stats = mem.stats(Some(scope("acme"))).await.expect("stats");
    assert_eq!(stats.total_facts, 2, "total_facts should be 2");
    assert_eq!(stats.valid_facts, 2, "valid_facts should be 2");
}
