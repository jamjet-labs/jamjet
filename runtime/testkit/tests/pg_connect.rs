//! Connecting to the Postgres store: the schema check, the session every
//! connection starts with, and the two calls the store has beside the
//! trait (the failover term and token revocation).

use jamjet_state::postgres::{migrate, SchemaState};
use jamjet_state::{PgBackend, PgSettings, TenantId};
use jamjet_testkit::pg::{TestDatabase, POOL_SIZE};
use sqlx::{AssertSqlSafe, Connection, PgConnection};
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

/// The seven settings the store sets on each connection, in this order:
/// application_name, TimeZone, standard_conforming_strings,
/// default_transaction_isolation, statement_timeout, lock_timeout and
/// idle_in_transaction_session_timeout.
async fn session_of(conn: &mut PgConnection) -> [String; 7] {
    let (a, b, c, d, e, f, g): (String, String, String, String, String, String, String) =
        sqlx::query_as(
            "SELECT current_setting('application_name'), current_setting('TimeZone'), \
                    current_setting('standard_conforming_strings'), \
                    current_setting('default_transaction_isolation'), \
                    current_setting('statement_timeout'), current_setting('lock_timeout'), \
                    current_setting('idle_in_transaction_session_timeout')",
        )
        .fetch_one(conn)
        .await
        .unwrap();
    [a, b, c, d, e, f, g]
}

#[tokio::test]
async fn every_connection_starts_with_the_session_settings() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("every_connection_starts_with_the_session_settings");
    };
    // A database whose own defaults differ from the store's in every setting,
    // as a managed database's may. Each statement names the test database,
    // which passed the kit's name check.
    let setup = db.pool().await;
    for setting in [
        "application_name = 'set_by_the_database'",
        "TimeZone = 'Asia/Kolkata'",
        "standard_conforming_strings = off",
        "default_transaction_isolation = 'repeatable read'",
        "statement_timeout = '41s'",
        "lock_timeout = '42s'",
        "idle_in_transaction_session_timeout = '43s'",
    ] {
        sqlx::raw_sql(AssertSqlSafe(format!(
            r#"ALTER DATABASE "{}" SET {setting}"#,
            db.name()
        )))
        .execute(&setup)
        .await
        .unwrap();
    }
    setup.close().await;

    // A connection without the store's session gets the database's values.
    // TimeZone is the exception: sqlx sends `TimeZone=UTC` in its startup
    // message, which outranks a database setting, so this test cannot tell
    // the store's own `TimeZone` setting from the driver's.
    let mut plain = PgConnection::connect_with(&db.connect_options())
        .await
        .unwrap();
    let mut theirs = session_of(&mut plain).await;
    plain.close().await.unwrap();
    theirs[1] = "(not compared)".to_string();
    assert_eq!(
        theirs,
        [
            "set_by_the_database",
            "(not compared)",
            "off",
            "repeatable read",
            "41s",
            "42s",
            "43s",
        ],
        "the database's own defaults took"
    );

    let settings = PgSettings {
        pool_size: 2,
        acquire_timeout: Duration::from_millis(1_500),
        statement_timeout: Duration::from_millis(1_234),
        lock_timeout: Duration::from_millis(567),
        idle_in_tx_timeout: Duration::from_millis(8_910),
    };
    let store = PgBackend::connect(db.url(), settings).await.unwrap();
    let pool = store.pool();
    // Both connections the pool may hold, checked out at once so they are
    // two sessions.
    let mut first = pool.acquire().await.unwrap();
    let mut second = pool.acquire().await.unwrap();
    let pid = "SELECT pg_backend_pid()";
    let first_pid: i32 = sqlx::query_scalar(pid)
        .fetch_one(&mut *first)
        .await
        .unwrap();
    let second_pid: i32 = sqlx::query_scalar(pid)
        .fetch_one(&mut *second)
        .await
        .unwrap();
    assert_ne!(first_pid, second_pid, "two connections");
    let ours = [
        "jamjet",
        "UTC",
        "on",
        "read committed",
        "1234ms",
        "567ms",
        "8910ms",
    ];
    assert_eq!(session_of(&mut first).await, ours, "the first connection");
    assert_eq!(session_of(&mut second).await, ours, "the second connection");
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
