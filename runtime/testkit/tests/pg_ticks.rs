//! The tick lease on the Postgres store, where the suite's cases do not
//! reach: engines whose clocks are wrong still agree on who holds it,
//! because the lease is taken and checked on the database's clock.
//!
//! The case runs again in a child of this test binary whose process clock
//! is an hour ahead (`JAMJET_TEST_CLOCK_SKEW_SECS`), since the skew is read
//! once per process.

use chrono::Utc;
use jamjet_state::{PgBackend, PgSettings, StateBackend, TenantId, TickBegin};
use jamjet_testkit::pg::{admin_url, TestDatabase, POOL_SIZE};
use std::time::Duration;

/// The variable that moves this process's clock, under the `test-hooks`
/// feature, and how far the clock case moves it.
const SKEW_ENV: &str = "JAMJET_TEST_CLOCK_SKEW_SECS";
const SKEW_SECS: i64 = 3600;
/// What a child prints once it has run the clock case's body.
const CHILD_RAN: &str = "ran with the process clock an hour ahead";

async fn store(db: &TestDatabase) -> PgBackend {
    let settings = PgSettings {
        pool_size: POOL_SIZE,
        ..PgSettings::default()
    };
    PgBackend::connect(db.url(), settings)
        .await
        .unwrap_or_else(|e| panic!("connect to the test database: {e}"))
}

/// In the parent, run `test` again in a child of this binary whose process
/// clock is an hour ahead, require that it ran the case's body and passed,
/// and answer `false`. In the child, answer `true`: the caller runs the
/// body. Without a Postgres server the case skips here.
fn in_a_skewed_child(test: &str) -> bool {
    if std::env::var_os(SKEW_ENV).is_some() {
        return true;
    }
    if admin_url().is_none() {
        jamjet_testkit::skip(test);
        return false;
    }
    let out = std::process::Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", test, "--nocapture", "--test-threads", "1"])
        .env(SKEW_ENV, SKEW_SECS.to_string())
        .output()
        .expect("run the child");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        out.status.success(),
        "the child failed:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains(CHILD_RAN),
        "the child did not run the case's body:\n{stdout}\n{stderr}"
    );
    false
}

/// The process clock is an hour ahead. A 60 s tick lease ends 60 s from
/// now by the database's clock, and a second engine, unscoped or through a
/// tenant's view, does not take it while the database's clock says it is
/// live. Once the database's clock passes its end, the second engine takes
/// it over.
#[tokio::test]
async fn the_lease_follows_the_store_clock() {
    if !in_a_skewed_child("the_lease_follows_the_store_clock") {
        return;
    }
    let ahead = jamjet_state::clock::process_now() - Utc::now();
    assert!(
        (ahead.num_seconds() - SKEW_SECS).abs() <= 1,
        "the process clock is {ahead} ahead"
    );
    let db = TestDatabase::fresh()
        .await
        .expect("the parent saw a Postgres URL");
    let (first, second) = (store(&db).await, store(&db).await);
    let pool = db.pool().await;

    let began = first
        .begin_tick("engine-a:1", Duration::from_secs(60))
        .await
        .unwrap();
    assert!(matches!(began, TickBegin::Began { .. }), "{began:?}");
    let left: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM lease_until - statement_timestamp())::float8 \
         FROM engine_ticks WHERE name = 'tick'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        (58.0..=60.0).contains(&left),
        "the lease has {left} s left by the database's clock"
    );

    let scoped = second.for_tenant(TenantId::default());
    for (whose, other) in [
        ("second engine", &second as &dyn StateBackend),
        ("tenant view", &scoped as &dyn StateBackend),
    ] {
        let busy = other
            .begin_tick("engine-b:1", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(
            matches!(&busy, TickBegin::Busy { holder, .. } if holder == "engine-a:1"),
            "{whose} took a lease the database's clock says is live: {busy:?}"
        );
    }

    let done = sqlx::query(
        "UPDATE engine_ticks SET lease_until = statement_timestamp() - interval '1 second' \
         WHERE name = 'tick'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(done.rows_affected(), 1, "no tick row");
    let taken = second
        .begin_tick("engine-b:1", Duration::from_secs(60))
        .await
        .unwrap();
    assert!(
        matches!(taken, TickBegin::Began { .. }),
        "a lease lapsed by the database's clock was not taken over: {taken:?}"
    );
    println!("{CHILD_RAN}");
}
