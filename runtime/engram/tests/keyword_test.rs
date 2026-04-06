use engram::fact::Fact;
use engram::scope::Scope;
use engram::store::FactStore;
use engram::store_sqlite::SqliteFactStore;

async fn test_store() -> SqliteFactStore {
    let store = SqliteFactStore::open("sqlite::memory:").await.unwrap();
    store.migrate().await.unwrap();
    store
}

fn user_scope(uid: &str) -> Scope {
    Scope::user("default", uid)
}

#[tokio::test]
async fn keyword_search_finds_exact_terms() {
    let store = test_store().await;
    store
        .insert_fact(Fact::new("User is allergic to peanuts", user_scope("u1")))
        .await
        .unwrap();
    store
        .insert_fact(Fact::new("User lives in Austin Texas", user_scope("u1")))
        .await
        .unwrap();
    store
        .insert_fact(Fact::new("User prefers dark mode", user_scope("u1")))
        .await
        .unwrap();

    let results = store
        .keyword_search("peanuts", &user_scope("u1"), 10)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].text.contains("peanuts"));
}

#[tokio::test]
async fn keyword_search_finds_multiple_matches() {
    let store = test_store().await;
    store
        .insert_fact(Fact::new("User likes Austin restaurants", user_scope("u1")))
        .await
        .unwrap();
    store
        .insert_fact(Fact::new("User lives in Austin", user_scope("u1")))
        .await
        .unwrap();
    store
        .insert_fact(Fact::new("User prefers pizza", user_scope("u1")))
        .await
        .unwrap();

    let results = store
        .keyword_search("Austin", &user_scope("u1"), 10)
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
}

#[tokio::test]
async fn keyword_search_respects_scope() {
    let store = test_store().await;
    store
        .insert_fact(Fact::new("User likes pizza", user_scope("u1")))
        .await
        .unwrap();
    store
        .insert_fact(Fact::new("User likes pizza too", user_scope("u2")))
        .await
        .unwrap();

    let results = store
        .keyword_search("pizza", &user_scope("u1"), 10)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn keyword_search_excludes_invalid_facts() {
    let store = test_store().await;
    let fact = Fact::new("User allergic to peanuts", user_scope("u1"));
    let id = fact.id;
    store.insert_fact(fact).await.unwrap();
    store.invalidate_fact(id).await.unwrap();

    let results = store
        .keyword_search("peanuts", &user_scope("u1"), 10)
        .await
        .unwrap();
    assert_eq!(results.len(), 0);
}
