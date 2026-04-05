//! Integration tests for `SqliteGraphStore`.
//!
//! All tests use an in-memory SQLite database so they are hermetic and fast.

use chrono::Utc;
use engram::fact::{Entity, Relationship};
use engram::graph::GraphStore;
use engram::graph_sqlite::SqliteGraphStore;
use engram::scope::Scope;

async fn new_store() -> SqliteGraphStore {
    let store = SqliteGraphStore::open("sqlite::memory:")
        .await
        .expect("open in-memory db");
    store.migrate().await.expect("migrate");
    store
}

fn org_scope() -> Scope {
    Scope::org("acme")
}

// ---------------------------------------------------------------------------
// 1. upsert_and_get_entity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upsert_and_get_entity() {
    let store = new_store().await;

    let entity = Entity::new("Alice", org_scope()).with_type("person");
    store.upsert_entity(&entity).await.expect("upsert");

    let retrieved = store
        .get_entity(entity.id)
        .await
        .expect("get")
        .expect("should exist");

    assert_eq!(retrieved.id, entity.id);
    assert_eq!(retrieved.name, "Alice");
    assert_eq!(retrieved.entity_type, "person");
    assert_eq!(retrieved.scope.org_id, "acme");
}

// ---------------------------------------------------------------------------
// 2. upsert_entity_updates_on_conflict
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upsert_entity_updates_on_conflict() {
    let store = new_store().await;

    let entity = Entity::new("Bob", org_scope()).with_type("person");
    store.upsert_entity(&entity).await.expect("upsert first");

    // Change entity_type and upsert again with the same id.
    let mut updated = entity.clone();
    updated.entity_type = "employee".to_string();
    updated.updated_at = Utc::now();
    store.upsert_entity(&updated).await.expect("upsert second");

    let retrieved = store
        .get_entity(entity.id)
        .await
        .expect("get")
        .expect("should exist");

    assert_eq!(retrieved.entity_type, "employee");
    assert_eq!(retrieved.name, "Bob");
}

// ---------------------------------------------------------------------------
// 3. upsert_and_query_relationship
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upsert_and_query_relationship() {
    let store = new_store().await;

    let src = Entity::new("Anthropic", org_scope()).with_type("organization");
    let tgt = Entity::new("San Francisco", org_scope()).with_type("city");
    store.upsert_entity(&src).await.unwrap();
    store.upsert_entity(&tgt).await.unwrap();

    let rel = Relationship::new(src.id, "located_in", tgt.id, org_scope());
    store.upsert_relationship(&rel).await.expect("upsert rel");

    let subgraph = store
        .neighbors(src.id, 1, None)
        .await
        .expect("neighbors");

    assert_eq!(subgraph.relationships.len(), 1);
    assert_eq!(subgraph.entities.len(), 1);
    assert_eq!(subgraph.relationships[0].relation, "located_in");
    assert_eq!(subgraph.entities[0].id, tgt.id);
}

// ---------------------------------------------------------------------------
// 4. invalidated_relationships_excluded_by_default
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalidated_relationships_excluded_by_default() {
    let store = new_store().await;

    let a = Entity::new("EntityA", org_scope());
    let b = Entity::new("EntityB", org_scope());
    store.upsert_entity(&a).await.unwrap();
    store.upsert_entity(&b).await.unwrap();

    let rel = Relationship::new(a.id, "knows", b.id, org_scope());
    store.upsert_relationship(&rel).await.unwrap();

    // Invalidate immediately.
    store
        .invalidate_relationship(rel.id, Utc::now())
        .await
        .expect("invalidate");

    let subgraph = store.neighbors(a.id, 1, None).await.expect("neighbors");

    assert!(
        subgraph.relationships.is_empty(),
        "invalidated relationship should not appear in default (None) neighbors query"
    );
    assert!(subgraph.entities.is_empty());
}

// ---------------------------------------------------------------------------
// 5. temporal_query_returns_valid_at_time
// ---------------------------------------------------------------------------

#[tokio::test]
async fn temporal_query_returns_valid_at_time() {
    use chrono::Duration;

    let store = new_store().await;

    let a = Entity::new("NodeA", org_scope());
    let b = Entity::new("NodeB", org_scope());
    let c = Entity::new("NodeC", org_scope());
    store.upsert_entity(&a).await.unwrap();
    store.upsert_entity(&b).await.unwrap();
    store.upsert_entity(&c).await.unwrap();

    // Use explicit, well-separated timestamps to avoid any clock precision issues.
    let t0 = Utc::now() - Duration::seconds(10);
    let t1 = Utc::now() - Duration::seconds(5); // query point
    let t2 = Utc::now() - Duration::seconds(2); // rel_ab invalidated at
    let t3 = Utc::now();                         // rel_ac starts at

    // Rel A→B: valid from t0, invalidated at t2.
    let mut rel_ab = Relationship::new(a.id, "rel_ab", b.id, org_scope());
    rel_ab.valid_from = t0;
    rel_ab.invalid_at = Some(t2);
    store.upsert_relationship(&rel_ab).await.unwrap();

    // Rel A→C: valid from t3 onwards.
    let mut rel_ac = Relationship::new(a.id, "rel_ac", c.id, org_scope());
    rel_ac.valid_from = t3;
    store.upsert_relationship(&rel_ac).await.unwrap();

    // Query at t1 — rel_ab is valid (t0 <= t1 < t2), rel_ac is not (t3 > t1).
    let subgraph = store
        .neighbors(a.id, 1, Some(t1))
        .await
        .expect("neighbors at query_time");

    let relations: Vec<&str> = subgraph
        .relationships
        .iter()
        .map(|r| r.relation.as_str())
        .collect();

    assert!(
        relations.contains(&"rel_ab"),
        "rel_ab should be visible at t1: {:?}",
        relations
    );
    assert!(
        !relations.contains(&"rel_ac"),
        "rel_ac should NOT be visible at t1 (starts at t3): {:?}",
        relations
    );
}

// ---------------------------------------------------------------------------
// 6. search_entities_by_name
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_entities_by_name() {
    let store = new_store().await;

    store
        .upsert_entity(&Entity::new("Austin", org_scope()))
        .await
        .unwrap();
    store
        .upsert_entity(&Entity::new("Denver", org_scope()))
        .await
        .unwrap();
    store
        .upsert_entity(&Entity::new("Austin Powers", org_scope()))
        .await
        .unwrap();

    let results = store
        .search_entities("Austin", 10)
        .await
        .expect("search");

    assert_eq!(results.len(), 2, "should find 'Austin' and 'Austin Powers'");
    let names: Vec<&str> = results.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"Austin"));
    assert!(names.contains(&"Austin Powers"));
    assert!(!names.contains(&"Denver"));
}

// ---------------------------------------------------------------------------
// 7. delete_by_scope_clears_entities_and_relationships
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_by_scope_clears_entities_and_relationships() {
    let store = new_store().await;

    let scope_u1 = Scope::user("acme", "u1");
    let scope_u2 = Scope::user("acme", "u2");

    let e1 = Entity::new("E1", scope_u1.clone());
    let e2 = Entity::new("E2", scope_u1.clone());
    let e3 = Entity::new("E3", scope_u2.clone());

    store.upsert_entity(&e1).await.unwrap();
    store.upsert_entity(&e2).await.unwrap();
    store.upsert_entity(&e3).await.unwrap();

    let rel = Relationship::new(e1.id, "linked_to", e2.id, scope_u1.clone());
    store.upsert_relationship(&rel).await.unwrap();

    // Delete u1 scope.
    let deleted = store
        .delete_by_scope(&scope_u1)
        .await
        .expect("delete_by_scope");

    // 2 entities + 1 relationship = 3 rows deleted.
    assert_eq!(deleted, 3, "should have deleted 2 entities + 1 relationship");

    // u2's entity should still exist.
    let e3_retrieved = store.get_entity(e3.id).await.expect("get e3").expect("e3 should exist");
    assert_eq!(e3_retrieved.name, "E3");

    // u1 entities should be gone.
    assert!(store.get_entity(e1.id).await.unwrap().is_none());
    assert!(store.get_entity(e2.id).await.unwrap().is_none());
}
