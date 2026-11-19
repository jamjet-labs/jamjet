//! Scope ids, categories and search text are data, never SQL.
//!
//! The stores used to write these values into the SQL text and double the
//! single quotes. That is an escape on SQLite, and on Postgres only while
//! `standard_conforming_strings` is on. With it off, `\'` is an escaped quote,
//! and a scope id could close its own literal: `delete_scope_data` for one
//! organisation deleted every organisation's facts.
//!
//! The SQLite cases always run. The Postgres cases need a server: set
//! `ENGRAM_TEST_POSTGRES_URL`. With `ENGRAM_REQUIRE_POSTGRES_TESTS=1` (CI) a
//! missing URL fails the test instead of skipping it.

use std::str::FromStr;

use chrono::{Duration, Utc};
use engram::fact::{Entity, Fact, FactFilter, FactId, FactPatch, MemoryTier, Relationship};
use engram::graph::GraphStore;
use engram::graph_postgres::PostgresGraphStore;
use engram::graph_sqlite::SqliteGraphStore;
use engram::scope::Scope;
use engram::store::FactStore;
use engram::store_postgres::PostgresFactStore;
use engram::store_sqlite::SqliteFactStore;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

/// Values that mean something to an SQL parser.
const HOSTILE: &[&str] = &[
    "o'brien",
    "x' OR '1'='1",
    // Closes the literal where backslash escapes are on.
    "x\\' OR 1=1 --",
    "x'; DELETE FROM facts; --",
    "back\\slash",
    "100%_done",
];

/// A text search is a `LIKE`, which gives `%` and `_` a meaning of their own
/// and, on Postgres, treats `\` as its escape character. A needle with one
/// of those may miss its own row. It must still never return another row.
fn assert_search_result(found: &[uuid::Uuid], own: uuid::Uuid, value: &str) {
    if value.contains(['\\', '%', '_']) {
        assert!(
            found.iter().all(|id| *id == own),
            "search {value:?} returned other rows: {found:?}"
        );
    } else {
        assert_eq!(found, [own], "search {value:?}");
    }
}

fn ids(mut facts: Vec<Fact>) -> Vec<FactId> {
    let mut out: Vec<FactId> = facts.drain(..).map(|f| f.id).collect();
    out.sort();
    out
}

/// Every hostile value is used as an organisation, a user, a category and a
/// search text. Each must select its own fact and nothing else, and a delete
/// by a hostile organisation that owns nothing must delete nothing.
async fn facts_treat_values_as_data(store: &dyn FactStore) {
    let bystander = Fact::new("the bystander", Scope::user("bystander-org", "bob"));
    let bystander_id = store.insert_fact(bystander).await.expect("insert");

    let mut own: Vec<(Scope, FactId)> = Vec::new();
    for (i, value) in HOSTILE.iter().enumerate() {
        let scope = Scope::user(format!("org-{i}-{value}"), *value);
        let mut fact = Fact::new(format!("note {i}: {value} end"), scope.clone());
        fact.category = Some(value.to_string());
        let id = store.insert_fact(fact).await.expect("insert");
        own.push((scope, id));
    }

    for (i, value) in HOSTILE.iter().enumerate() {
        let (scope, id) = &own[i];

        let by_scope = store
            .list_facts(&FactFilter::new().with_scope(scope.clone()))
            .await
            .unwrap_or_else(|e| panic!("list by scope {value:?}: {e}"));
        assert_eq!(ids(by_scope), vec![*id], "scope {value:?}");

        let mut by_category = FactFilter::new();
        by_category.category = Some(value.to_string());
        let found = store
            .list_facts(&by_category)
            .await
            .unwrap_or_else(|e| panic!("list by category {value:?}: {e}"));
        assert_eq!(ids(found), vec![*id], "category {value:?}");

        // The text of every fact holds its own value between "note i: " and
        // " end", so the whole text is a search no other fact matches.
        let mut by_text = FactFilter::new();
        by_text.text_contains = Some(format!("note {i}: {value} end"));
        let found = store
            .list_facts(&by_text)
            .await
            .unwrap_or_else(|e| panic!("list by text {value:?}: {e}"));
        assert_search_result(&ids(found), *id, value);
    }

    // An organisation named like an attack owns nothing: nothing is deleted.
    for value in HOSTILE {
        let scope = Scope::org(*value);
        let deleted = store
            .delete_scope_data(&scope)
            .await
            .unwrap_or_else(|e| panic!("delete {value:?}: {e}"));
        assert_eq!(deleted, 0, "delete_scope_data({value:?}) deleted rows");
        let returned = store
            .delete_scope_returning(&scope)
            .await
            .unwrap_or_else(|e| panic!("delete returning {value:?}: {e}"));
        assert!(
            returned.is_empty(),
            "delete_scope_returning({value:?}): {returned:?}"
        );
    }
    store
        .get_fact(bystander_id)
        .await
        .expect("the bystander's fact is gone");
    for (scope, id) in &own {
        store
            .get_fact(*id)
            .await
            .unwrap_or_else(|e| panic!("fact of {:?} is gone: {e}", scope.org_id));
    }

    // A delete by a hostile scope takes exactly that scope's fact.
    for (scope, id) in &own {
        let deleted = store
            .delete_scope_data(scope)
            .await
            .expect("delete own scope");
        assert_eq!(deleted, 1, "scope {:?}", scope.org_id);
        assert!(
            store.get_fact(*id).await.is_err(),
            "fact of {:?} survived",
            scope.org_id
        );
    }
    store
        .get_fact(bystander_id)
        .await
        .expect("the bystander's fact is gone");
}

async fn graph_treats_values_as_data(store: &dyn GraphStore) {
    let bystander = Entity::new("Bystander", Scope::org("bystander-org"));
    store.upsert_entity(&bystander).await.expect("upsert");
    // A relationship too, so the delete from `relationships` has a row to
    // take if a hostile scope ever widens it.
    let friend = Entity::new("Friend", Scope::org("bystander-org"));
    store.upsert_entity(&friend).await.expect("upsert");
    let knows = Relationship::new(
        bystander.id,
        "knows",
        friend.id,
        Scope::org("bystander-org"),
    );
    store.upsert_relationship(&knows).await.expect("upsert");

    let mut own: Vec<Entity> = Vec::new();
    for (i, value) in HOSTILE.iter().enumerate() {
        let entity = Entity::new(
            format!("name {i}: {value} end"),
            Scope::org(format!("org-{i}-{value}")),
        );
        store.upsert_entity(&entity).await.expect("upsert");
        own.push(entity);
    }

    for (i, value) in HOSTILE.iter().enumerate() {
        let found = store
            .search_entities(&format!("name {i}: {value} end"), 10)
            .await
            .unwrap_or_else(|e| panic!("search {value:?}: {e}"));
        let found: Vec<_> = found.into_iter().map(|e| e.id).collect();
        assert_search_result(&found, own[i].id, value);
    }

    for value in HOSTILE {
        let deleted = store
            .delete_by_scope(&Scope::org(*value))
            .await
            .unwrap_or_else(|e| panic!("delete {value:?}: {e}"));
        assert_eq!(deleted, 0, "delete_by_scope({value:?}) deleted rows");
    }
    assert!(store.get_entity(bystander.id).await.expect("get").is_some());
    let kept = store
        .neighbors(bystander.id, 1, None)
        .await
        .expect("neighbors");
    assert_eq!(
        kept.relationships.len(),
        1,
        "the bystander's relationship is gone"
    );
    for entity in &own {
        assert!(store.get_entity(entity.id).await.expect("get").is_some());
        let deleted = store
            .delete_by_scope(&entity.scope)
            .await
            .expect("delete own scope");
        assert_eq!(deleted, 1, "scope {:?}", entity.scope.org_id);
    }
    assert!(store.get_entity(bystander.id).await.expect("get").is_some());
}

#[tokio::test]
async fn sqlite_facts_treat_values_as_data() {
    let store = SqliteFactStore::open("sqlite::memory:")
        .await
        .expect("open");
    store.migrate().await.expect("migrate");
    facts_treat_values_as_data(&store).await;
}

#[tokio::test]
async fn sqlite_graph_treats_values_as_data() {
    let store = SqliteGraphStore::open("sqlite::memory:")
        .await
        .expect("open");
    store.migrate().await.expect("migrate");
    graph_treats_values_as_data(&store).await;
}

/// Doubling quotes is an escape on SQLite, so no value in `HOSTILE` tells a
/// bound parameter from one written into the text there. A NUL does: SQLite
/// stops reading SQL text at it, so written into the text it ends the
/// statement inside its own literal. As a bound value it is one more byte of
/// data. (Postgres text cannot hold a NUL, so this case is SQLite's alone.)
#[tokio::test]
async fn sqlite_binds_scope_ids_rather_than_writing_them() {
    let facts = SqliteFactStore::open("sqlite::memory:")
        .await
        .expect("open");
    facts.migrate().await.expect("migrate");
    let scope = Scope::full("a\0b", "agent\0x", "user\0y", "session\0z");
    let id = facts
        .insert_fact(Fact::new("held under ids with a NUL", scope.clone()))
        .await
        .expect("insert");
    let listed = facts
        .list_facts(&FactFilter::new().with_scope(scope.clone()))
        .await
        .expect("list by a scope with a NUL");
    assert_eq!(ids(listed), vec![id]);
    assert_eq!(
        facts.delete_scope_returning(&scope).await.expect("delete"),
        vec![id]
    );
    assert_eq!(
        facts.delete_scope_data(&scope).await.expect("delete again"),
        0
    );

    let graph = SqliteGraphStore::open("sqlite::memory:")
        .await
        .expect("open");
    graph.migrate().await.expect("migrate");
    let entity = Entity::new("Named", scope.clone());
    graph.upsert_entity(&entity).await.expect("upsert");
    assert_eq!(graph.delete_by_scope(&scope).await.expect("delete"), 1);
}

// ── Every rewritten query runs ───────────────────────────────────────────────

/// The queries the stores build at run time, each run with every kind of
/// value it binds: all four scope ids, a tier, a point in time, a page, a
/// patch, and a delete that returns rows. On Postgres these are the first
/// tests that send the typed values to a server.
async fn fact_queries_run(store: &dyn FactStore) {
    let scope = Scope::full("acme", "agent-1", "alice", "session-1");
    let now = Utc::now();

    let mut current = Fact::new("current knowledge", scope.clone());
    current.tier = MemoryTier::Knowledge;
    current.valid_from = now - Duration::hours(2);
    let current_id = store.insert_fact(current).await.expect("insert");

    let mut retired = Fact::new("retired knowledge", scope.clone());
    retired.tier = MemoryTier::Knowledge;
    retired.valid_from = now - Duration::hours(3);
    retired.invalid_at = Some(now - Duration::hours(1));
    let retired_id = store.insert_fact(retired).await.expect("insert");

    let chat = Fact::new("small talk", scope.clone());
    let chat_id = store.insert_fact(chat).await.expect("insert");

    // Another session of the same user is outside the full scope.
    let other = Fact::new(
        "another session",
        Scope::full("acme", "agent-1", "alice", "session-2"),
    );
    let other_id = store.insert_fact(other).await.expect("insert");

    let in_scope = FactFilter::new().with_scope(scope.clone());

    let listed = store.list_facts(&in_scope).await.expect("list");
    let mut expected = vec![current_id, chat_id];
    expected.sort();
    assert_eq!(ids(listed), expected, "valid facts in the full scope");

    let knowledge = store
        .list_facts(&in_scope.clone().with_tier(MemoryTier::Knowledge))
        .await
        .expect("list by tier");
    assert_eq!(ids(knowledge), vec![current_id], "tier");

    // Ninety minutes ago the retired fact was still valid and the chat fact
    // did not exist yet.
    let mut then = in_scope.clone();
    then.as_of = Some(now - Duration::minutes(90));
    let mut expected = vec![current_id, retired_id];
    expected.sort();
    assert_eq!(
        ids(store.list_facts(&then).await.expect("list as of")),
        expected,
        "as_of"
    );

    let mut page = in_scope.clone().include_invalid();
    page.limit = 1;
    let first = store.list_facts(&page).await.expect("first page");
    page.offset = 1;
    let second = store.list_facts(&page).await.expect("second page");
    page.offset = 3;
    let past_the_end = store.list_facts(&page).await.expect("past the end");
    assert_eq!(
        (first.len(), second.len(), past_the_end.len()),
        (1, 1, 0),
        "limit and offset"
    );
    assert_ne!(
        first[0].id, second[0].id,
        "the second page repeated the first"
    );

    let patch = FactPatch {
        text: Some("current knowledge, revised".to_string()),
        tier: Some(MemoryTier::Working),
        category: Some("revised".to_string()),
        source: Some("test".to_string()),
        confidence: Some(0.5),
        invalid_at: Some(now),
        superseded_by: Some(chat_id),
        metadata: [("reviewed".to_string(), serde_json::json!(true))]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let patched = store
        .update_fact(current_id, patch)
        .await
        .expect("update_fact");
    assert_eq!(patched.text, "current knowledge, revised");
    assert_eq!(patched.tier, MemoryTier::Working);
    assert_eq!(patched.category.as_deref(), Some("revised"));
    assert_eq!(patched.source.as_deref(), Some("test"));
    assert_eq!(patched.confidence, Some(0.5));
    assert_eq!(patched.superseded_by, Some(chat_id));
    assert!(patched.invalid_at.is_some());
    assert_eq!(
        patched.metadata.get("reviewed"),
        Some(&serde_json::json!(true))
    );

    let mut deleted = store
        .delete_scope_returning(&scope)
        .await
        .expect("delete returning");
    deleted.sort();
    let mut expected = vec![current_id, retired_id, chat_id];
    expected.sort();
    assert_eq!(deleted, expected, "delete_scope_returning");
    store
        .get_fact(other_id)
        .await
        .expect("the other session's fact is gone");
    assert_eq!(
        store
            .delete_scope_data(&Scope::org("acme"))
            .await
            .expect("delete"),
        1
    );
}

async fn graph_queries_run(store: &dyn GraphStore) {
    let scope = Scope::full("acme", "agent-1", "alice", "session-1");
    let now = Utc::now();
    let a = Entity::new("A", scope.clone());
    let b = Entity::new("B", scope.clone());
    let c = Entity::new("C", scope.clone());
    for entity in [&a, &b, &c] {
        store.upsert_entity(entity).await.expect("upsert");
    }
    let mut current = Relationship::new(a.id, "knows", b.id, scope.clone());
    current.valid_from = now - Duration::hours(2);
    store.upsert_relationship(&current).await.expect("upsert");
    let mut retired = Relationship::new(a.id, "knew", c.id, scope.clone());
    retired.valid_from = now - Duration::hours(3);
    retired.invalid_at = Some(now - Duration::hours(1));
    store.upsert_relationship(&retired).await.expect("upsert");

    let names = |entities: Vec<Entity>| {
        let mut names: Vec<String> = entities.into_iter().map(|e| e.name).collect();
        names.sort();
        names
    };
    let today = store.neighbors(a.id, 1, None).await.expect("neighbors");
    assert_eq!(names(today.entities), ["B"], "neighbors now");
    let then = store
        .neighbors(a.id, 1, Some(now - Duration::minutes(90)))
        .await
        .expect("neighbors as of");
    assert_eq!(names(then.entities), ["B", "C"], "neighbors as of");
    assert_eq!(then.relationships.len(), 2);

    // Two relationships and three entities.
    assert_eq!(store.delete_by_scope(&scope).await.expect("delete"), 5);
    assert!(store.get_entity(a.id).await.expect("get").is_none());
}

#[tokio::test]
async fn sqlite_fact_queries_run() {
    let store = SqliteFactStore::open("sqlite::memory:")
        .await
        .expect("open");
    store.migrate().await.expect("migrate");
    fact_queries_run(&store).await;
}

#[tokio::test]
async fn sqlite_graph_queries_run() {
    let store = SqliteGraphStore::open("sqlite::memory:")
        .await
        .expect("open");
    store.migrate().await.expect("migrate");
    graph_queries_run(&store).await;
}

#[tokio::test]
async fn postgres_fact_queries_run() {
    let Some(pool) = postgres_pool(true).await else {
        return;
    };
    let store = PostgresFactStore::new(pool);
    store.migrate().await.expect("migrate");
    fact_queries_run(&store).await;
}

#[tokio::test]
async fn postgres_graph_queries_run() {
    let Some(pool) = postgres_pool(true).await else {
        return;
    };
    let store = PostgresGraphStore::new(pool);
    store.migrate().await.expect("migrate");
    graph_queries_run(&store).await;
}

// ── Postgres ─────────────────────────────────────────────────────────────────

/// A pool on a schema of its own, with `standard_conforming_strings` as given.
/// `None` when no server is configured and none is required.
async fn postgres_pool(conforming_strings: bool) -> Option<sqlx::PgPool> {
    let url = match std::env::var("ENGRAM_TEST_POSTGRES_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            assert!(
                std::env::var("ENGRAM_REQUIRE_POSTGRES_TESTS").is_err(),
                "ENGRAM_REQUIRE_POSTGRES_TESTS is set but ENGRAM_TEST_POSTGRES_URL is not"
            );
            eprintln!("skipped: ENGRAM_TEST_POSTGRES_URL is not set");
            return None;
        }
    };
    let base = PgConnectOptions::from_str(&url).expect("ENGRAM_TEST_POSTGRES_URL");
    let schema = format!("t_{}", uuid::Uuid::new_v4().simple());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(base.clone())
        .await
        .expect("connect to ENGRAM_TEST_POSTGRES_URL");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .expect("create schema");
    let options = base.options([
        ("search_path", schema.as_str()),
        (
            "standard_conforming_strings",
            if conforming_strings { "on" } else { "off" },
        ),
    ]);
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .expect("connect on the test schema");
    let setting: String = sqlx::query_scalar("SHOW standard_conforming_strings")
        .fetch_one(&pool)
        .await
        .expect("show");
    assert_eq!(setting, if conforming_strings { "on" } else { "off" });
    Some(pool)
}

#[tokio::test]
async fn postgres_facts_treat_values_as_data() {
    for conforming_strings in [true, false] {
        let Some(pool) = postgres_pool(conforming_strings).await else {
            return;
        };
        let store = PostgresFactStore::new(pool);
        store.migrate().await.expect("migrate");
        facts_treat_values_as_data(&store).await;
    }
}

#[tokio::test]
async fn postgres_graph_treats_values_as_data() {
    for conforming_strings in [true, false] {
        let Some(pool) = postgres_pool(conforming_strings).await else {
            return;
        };
        let store = PostgresGraphStore::new(pool);
        store.migrate().await.expect("migrate");
        graph_treats_values_as_data(&store).await;
    }
}
