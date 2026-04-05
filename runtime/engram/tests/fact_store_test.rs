//! Integration tests for `SqliteFactStore`.
//!
//! All tests use an in-memory SQLite database so they are hermetic and fast.

use engram::fact::{Fact, FactFilter, FactPatch, MemoryTier};
use engram::scope::Scope;
use engram::store::FactStore;
use engram::store_sqlite::SqliteFactStore;

async fn new_store() -> SqliteFactStore {
    let store = SqliteFactStore::open("sqlite::memory:")
        .await
        .expect("open in-memory db");
    store.migrate().await.expect("migrate");
    store
}

// ---------------------------------------------------------------------------
// 1. insert_and_get_fact
// ---------------------------------------------------------------------------

#[tokio::test]
async fn insert_and_get_fact() {
    let store = new_store().await;

    let mut fact = Fact::new("Alice works at Acme", Scope::user("acme", "alice"));
    fact.tier = MemoryTier::Knowledge;
    fact.category = Some("employment".to_string());
    fact.source = Some("hr-system".to_string());
    fact.confidence = Some(0.95);

    let id = store.insert_fact(fact.clone()).await.expect("insert");
    assert_eq!(id, fact.id);

    let retrieved = store.get_fact(id).await.expect("get");
    assert_eq!(retrieved.id, fact.id);
    assert_eq!(retrieved.text, "Alice works at Acme");
    assert_eq!(retrieved.scope.org_id, "acme");
    assert_eq!(retrieved.scope.user_id.as_deref(), Some("alice"));
    assert_eq!(retrieved.tier, MemoryTier::Knowledge);
    assert_eq!(retrieved.category.as_deref(), Some("employment"));
    assert_eq!(retrieved.source.as_deref(), Some("hr-system"));
    assert!((retrieved.confidence.unwrap() - 0.95_f32).abs() < 0.001);
    assert!(retrieved.invalid_at.is_none());
    assert_eq!(retrieved.access_count, 0);
}

// ---------------------------------------------------------------------------
// 2. list_facts_filters_by_scope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_facts_filters_by_scope() {
    let store = new_store().await;

    let f1 = Fact::new("fact for alice", Scope::user("acme", "alice"));
    let f2 = Fact::new("another for alice", Scope::user("acme", "alice"));
    let f3 = Fact::new("fact for bob", Scope::user("acme", "bob"));

    store.insert_fact(f1).await.unwrap();
    store.insert_fact(f2).await.unwrap();
    store.insert_fact(f3).await.unwrap();

    let filter = FactFilter::new().with_scope(Scope::user("acme", "alice"));
    let results = store.list_facts(&filter).await.expect("list");

    assert_eq!(results.len(), 2);
    for r in &results {
        assert_eq!(r.scope.user_id.as_deref(), Some("alice"));
    }
}

// ---------------------------------------------------------------------------
// 3. invalidate_excludes_from_valid_query
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalidate_excludes_from_valid_query() {
    let store = new_store().await;

    let scope = Scope::org("acme");
    let fact = Fact::new("temporary fact", scope.clone());
    let id = store.insert_fact(fact).await.unwrap();

    store.invalidate_fact(id).await.expect("invalidate");

    // valid_only=true (default) should exclude it
    let filter_valid = FactFilter::new().with_scope(scope.clone());
    let valid_results = store.list_facts(&filter_valid).await.unwrap();
    assert!(
        valid_results.iter().all(|f| f.id != id),
        "invalidated fact should not appear in valid_only query"
    );

    // include_invalid should see it
    let filter_all = FactFilter::new()
        .with_scope(scope.clone())
        .include_invalid();
    let all_results = store.list_facts(&filter_all).await.unwrap();
    assert!(
        all_results.iter().any(|f| f.id == id),
        "invalidated fact should appear when include_invalid"
    );
}

// ---------------------------------------------------------------------------
// 4. update_fact_patches_fields
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_fact_patches_fields() {
    let store = new_store().await;

    let fact = Fact::new("original text", Scope::org("acme"))
        .with_tier(MemoryTier::Conversation)
        .with_confidence(0.5);
    let id = store.insert_fact(fact).await.unwrap();

    let patch = FactPatch {
        confidence: Some(0.99),
        tier: Some(MemoryTier::Knowledge),
        ..Default::default()
    };
    let updated = store.update_fact(id, patch).await.expect("update");

    assert!((updated.confidence.unwrap() - 0.99_f32).abs() < 0.001);
    assert_eq!(updated.tier, MemoryTier::Knowledge);
    // unchanged fields remain
    assert_eq!(updated.text, "original text");
}

// ---------------------------------------------------------------------------
// 5. record_access_increments_count
// ---------------------------------------------------------------------------

#[tokio::test]
async fn record_access_increments_count() {
    let store = new_store().await;

    let fact = Fact::new("accessed fact", Scope::org("acme"));
    let id = store.insert_fact(fact).await.unwrap();

    store.record_access(id).await.expect("record_access 1");
    store.record_access(id).await.expect("record_access 2");

    let retrieved = store.get_fact(id).await.unwrap();
    assert_eq!(retrieved.access_count, 2);
    assert!(retrieved.last_accessed.is_some());
}

// ---------------------------------------------------------------------------
// 6. delete_scope_data_removes_all_facts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_scope_data_removes_all_facts() {
    let store = new_store().await;

    let scope_alice = Scope::user("acme", "alice");
    let scope_bob = Scope::user("acme", "bob");

    store
        .insert_fact(Fact::new("alice fact 1", scope_alice.clone()))
        .await
        .unwrap();
    store
        .insert_fact(Fact::new("alice fact 2", scope_alice.clone()))
        .await
        .unwrap();
    store
        .insert_fact(Fact::new("bob fact", scope_bob.clone()))
        .await
        .unwrap();

    let deleted = store.delete_scope_data(&scope_alice).await.expect("delete");
    assert_eq!(deleted, 2);

    // Bob's fact should still be there
    let filter = FactFilter::new().with_scope(scope_bob.clone());
    let remaining = store.list_facts(&filter).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].scope.user_id.as_deref(), Some("bob"));
}

// ---------------------------------------------------------------------------
// 7. stats_reports_counts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stats_reports_counts() {
    let store = new_store().await;

    let f1 = Fact::new("fact one", Scope::org("acme"));
    let f2 = Fact::new("fact two", Scope::org("acme"));
    let id1 = store.insert_fact(f1).await.unwrap();
    store.insert_fact(f2).await.unwrap();

    store.invalidate_fact(id1).await.unwrap();

    let stats = store.stats().await.expect("stats");
    assert_eq!(stats.total_facts, 2);
    assert_eq!(stats.valid_facts, 1);
    assert_eq!(stats.invalidated_facts, 1);
}

// ---------------------------------------------------------------------------
// 8. export_and_import_round_trips
// ---------------------------------------------------------------------------

#[tokio::test]
async fn export_and_import_round_trips() {
    let store_a = new_store().await;

    let scope = Scope::org("acme");
    store_a
        .insert_fact(Fact::new("exported fact 1", scope.clone()))
        .await
        .unwrap();
    store_a
        .insert_fact(Fact::new("exported fact 2", scope.clone()))
        .await
        .unwrap();

    let filter = FactFilter::new().with_scope(scope.clone());
    let exported = store_a.export(&filter).await.expect("export");
    assert_eq!(exported.len(), 2);

    let store_b = new_store().await;
    let count = store_b.import(exported).await.expect("import");
    assert_eq!(count, 2);

    let filter_b = FactFilter::new().with_scope(scope.clone());
    let imported = store_b.list_facts(&filter_b).await.unwrap();
    assert_eq!(imported.len(), 2);
}

// ---------------------------------------------------------------------------
// 9. list_facts_filters_by_tier
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_facts_filters_by_tier() {
    let store = new_store().await;

    let scope = Scope::org("acme");
    store
        .insert_fact(
            Fact::new("conversation fact", scope.clone()).with_tier(MemoryTier::Conversation),
        )
        .await
        .unwrap();
    store
        .insert_fact(Fact::new("knowledge fact", scope.clone()).with_tier(MemoryTier::Knowledge))
        .await
        .unwrap();

    let filter = FactFilter::new()
        .with_scope(scope.clone())
        .with_tier(MemoryTier::Knowledge);
    let results = store.list_facts(&filter).await.expect("list by tier");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].tier, MemoryTier::Knowledge);
    assert_eq!(results[0].text, "knowledge fact");
}
