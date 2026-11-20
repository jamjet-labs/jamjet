//! The Postgres test databases: each test gets one of its own, it is gone
//! when dropped, and a missing server is a skip only when nobody required
//! Postgres.

use jamjet_testkit::pg::{self, TestDatabase};
use jamjet_testkit::skip;
use sqlx::{Connection, PgConnection};

/// Set only in the child process `require_without_a_url_fails` starts.
const CHILD_ENV: &str = "JAMJET_TESTKIT_REQUIRE_CHILD";
const CHILD_TEST: &str = "fresh_database_in_a_child_with_require_and_no_url";

#[tokio::test]
async fn postgres_service_is_reachable_when_a_url_is_set() {
    let Some(admin) = pg::admin_url() else {
        return skip("postgres_service_is_reachable_when_a_url_is_set");
    };
    let mut conn = PgConnection::connect(&admin)
        .await
        .unwrap_or_else(|e| panic!("connect to the server in {}: {e}", pg::URL_ENV));
    let one: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(one, 1);
}

#[test]
fn require_without_a_url_fails() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            CHILD_TEST,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env_remove(pg::URL_ENV)
        .env(pg::REQUIRE_ENV, "1")
        .env(CHILD_ENV, "1")
        .output()
        .expect("run the child test");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "with {} set and no URL, the child passed:\n{text}",
        pg::REQUIRE_ENV
    );
    assert!(
        text.contains(&format!("{} is not set", pg::URL_ENV)),
        "the child failed without naming {}:\n{text}",
        pg::URL_ENV
    );
}

/// Run only by `require_without_a_url_fails`, in a child process with
/// `JAMJET_REQUIRE_POSTGRES_TESTS=1` and no URL.
#[tokio::test]
#[ignore = "run by require_without_a_url_fails in a child process"]
async fn fresh_database_in_a_child_with_require_and_no_url() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let db = TestDatabase::fresh().await;
    println!("fresh() returned without failing: {}", db.is_some());
}

#[tokio::test]
async fn two_fresh_databases_are_separate() {
    let Some(a) = TestDatabase::fresh().await else {
        return skip("two_fresh_databases_are_separate");
    };
    let b = TestDatabase::fresh().await.expect("a second database");
    assert_ne!(a.name(), b.name());
    let (pa, pb) = (a.pool().await, b.pool().await);
    for (db, pool) in [(&a, &pa), (&b, &pb)] {
        let current: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(current, db.name());
    }
    sqlx::query("CREATE TABLE only_in_a (id int)")
        .execute(&pa)
        .await
        .unwrap();
    let exists = "SELECT to_regclass('only_in_a') IS NOT NULL";
    let in_a: bool = sqlx::query_scalar(exists).fetch_one(&pa).await.unwrap();
    let in_b: bool = sqlx::query_scalar(exists).fetch_one(&pb).await.unwrap();
    assert!(in_a, "the table is missing where it was made");
    assert!(!in_b, "a table made in one test database shows in another");
}

#[tokio::test]
async fn a_dropped_fixture_drops_its_database() {
    let Some(db) = TestDatabase::fresh().await else {
        return skip("a_dropped_fixture_drops_its_database");
    };
    let name = db.name().to_string();
    // A live connection, which the drop must not wait for.
    let pool = db.pool().await;
    sqlx::query("SELECT 1").execute(&pool).await.unwrap();

    let mut admin = PgConnection::connect(&pg::admin_url().unwrap())
        .await
        .unwrap();
    let listed = "SELECT count(*) FROM pg_database WHERE datname = $1";
    let before: i64 = sqlx::query_scalar(listed)
        .bind(&name)
        .fetch_one(&mut admin)
        .await
        .unwrap();
    assert_eq!(before, 1);

    drop(db);
    let after: i64 = sqlx::query_scalar(listed)
        .bind(&name)
        .fetch_one(&mut admin)
        .await
        .unwrap();
    assert_eq!(after, 0, "{name} is still listed after its drop");
    drop(pool);
}
