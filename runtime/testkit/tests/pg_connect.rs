//! Connecting to the Postgres store: the schema check, the session every
//! connection starts with, and the two calls the store has beside the
//! trait (the failover term and token revocation).

use jamjet_state::postgres::{migrate, SchemaState};
use jamjet_state::{PgBackend, PgSettings, TenantId};
use jamjet_testkit::pg::{TestDatabase, POOL_SIZE};
use std::time::Duration;

fn small() -> PgSettings {
    PgSettings {
        pool_size: POOL_SIZE,
        ..PgSettings::default()
    }
}

#[tokio::test]
async fn connect_refuses_a_schema_that_is_not_current() {
    let Some(empty) = TestDatabase::empty().await else {
        return jamjet_testkit::skip("connect_refuses_a_schema_that_is_not_current");
    };
    let refused = PgBackend::connect(empty.url(), small())
        .await
        .err()
        .expect("an empty database is refused");
    assert_eq!(Some(refused.clone()), SchemaState::Empty.refusal());
    assert!(refused.contains("jamjet-server migrate"), "{refused}");
    // Unchecked, it connects, and migrating makes it acceptable.
    assert!(PgBackend::connect_unchecked(empty.url(), small())
        .await
        .is_ok());
    migrate(empty.url()).await.unwrap();
    assert!(PgBackend::connect(empty.url(), small()).await.is_ok());

    let ahead = TestDatabase::fresh().await.unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (9999, 'from a newer build', true, '\\x00', 0)",
    )
    .execute(&ahead.pool().await)
    .await
    .unwrap();
    let refused = PgBackend::connect(ahead.url(), small())
        .await
        .err()
        .expect("a newer schema is refused");
    assert!(refused.contains("do not roll back"), "{refused}");

    let dirty = TestDatabase::fresh().await.unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET success = false WHERE version = 1")
        .execute(&dirty.pool().await)
        .await
        .unwrap();
    let refused = PgBackend::connect(dirty.url(), small())
        .await
        .err()
        .expect("a half-applied schema is refused");
    assert!(refused.contains("migration 1"), "{refused}");
}

#[tokio::test]
async fn every_connection_starts_with_the_session_settings() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("every_connection_starts_with_the_session_settings");
    };
    let settings = PgSettings {
        pool_size: 2,
        acquire_timeout: Duration::from_millis(1_500),
        statement_timeout: Duration::from_millis(1_234),
        lock_timeout: Duration::from_millis(567),
        idle_in_tx_timeout: Duration::from_millis(8_910),
    };
    let store = PgBackend::connect(db.url(), settings).await.unwrap();
    let pool = store.pool();
    // Hold one connection so the second read is on a connection of its own.
    let held = pool.acquire().await.unwrap();
    for _ in 0..2 {
        let row: (String, String, String, String, String, String, String) = sqlx::query_as(
            "SELECT current_setting('application_name'), current_setting('TimeZone'), \
                    current_setting('standard_conforming_strings'), \
                    current_setting('default_transaction_isolation'), \
                    current_setting('statement_timeout'), current_setting('lock_timeout'), \
                    current_setting('idle_in_transaction_session_timeout')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row,
            (
                "jamjet".to_string(),
                "UTC".to_string(),
                "on".to_string(),
                "read committed".to_string(),
                "1234ms".to_string(),
                "567ms".to_string(),
                "8910ms".to_string(),
            )
        );
    }
    drop(held);
}

#[tokio::test]
async fn the_store_term_only_rises() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("the_store_term_only_rises");
    };
    let store = PgBackend::connect(db.url(), small()).await.unwrap();
    assert_eq!(store.set_store_term_at_least(0).await.unwrap(), 0);
    assert_eq!(store.set_store_term_at_least(7).await.unwrap(), 7);
    assert_eq!(store.set_store_term_at_least(3).await.unwrap(), 7);
    assert!(store.set_store_term_at_least(-1).await.is_err());
    let term: i64 = sqlx::query_scalar("SELECT term FROM store_identity WHERE id = 1")
        .fetch_one(&store.pool())
        .await
        .unwrap();
    assert_eq!(term, 7);
}

#[tokio::test]
async fn revoke_token_answers_what_it_revoked_once() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("revoke_token_answers_what_it_revoked_once");
    };
    let store = PgBackend::connect(db.url(), small()).await.unwrap();
    let pool = store.pool();
    sqlx::query("INSERT INTO tenants (id, name) VALUES ('acme', 'Acme')")
        .execute(&pool)
        .await
        .unwrap();
    for (id, hash, tenant) in [("tok-1", "h1", "default"), ("tok-2", "h2", "acme")] {
        sqlx::query(
            "INSERT INTO api_tokens (id, token_hash, name, role, tenant_id, created_at) \
             VALUES ($1, $2, $1, 'operator', $3, statement_timestamp())",
        )
        .bind(id)
        .bind(hash)
        .bind(tenant)
        .execute(&pool)
        .await
        .unwrap();
    }

    // A tenant's view cannot revoke another tenant's token.
    let acme = store.for_tenant(TenantId::from("acme"));
    assert_eq!(acme.revoke_token("tok-1").await.unwrap(), None);

    let revoked = store.revoke_token("tok-1").await.unwrap().expect("revoked");
    assert_eq!(
        (
            revoked.tenant_id.as_str(),
            revoked.role.as_str(),
            revoked.name.as_str()
        ),
        ("default", "operator", "tok-1")
    );
    assert_eq!(
        store.revoke_token("tok-1").await.unwrap(),
        None,
        "already revoked"
    );
    assert_eq!(store.revoke_token("no-such-token").await.unwrap(), None);
    assert!(acme.revoke_token("tok-2").await.unwrap().is_some());

    let still: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_tokens WHERE revoked_at IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still, 0);
}
