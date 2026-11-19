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

use engram::fact::{Entity, Fact, FactFilter, FactId};
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
