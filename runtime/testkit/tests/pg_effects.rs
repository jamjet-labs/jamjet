//! Effect reservations on the Postgres store, where the suite's cases do not
//! reach: two engines racing for one key, a reservation's expiry on the
//! database's clock while this process's clock is an hour wrong, and a TTL
//! too large to add to a time.
//!
//! A reservation is one upsert that takes the key only while no live
//! reservation holds it, so there is no moment between "nobody holds it" and
//! "it is mine". The clock case runs again in a child of this test binary
//! whose process clock is an hour ahead (`JAMJET_TEST_CLOCK_SKEW_SECS`),
//! since the skew is read once per process.

use chrono::{DateTime, Utc};
use jamjet_core::workflow::ExecutionId;
use jamjet_state::backend::MAX_RESERVATION_TTL;
use jamjet_state::{
    PgBackend, PgSettings, ReserveOutcome, StateBackend, Tenant, TenantId, TenantStatus,
    DEFAULT_TENANT,
};
use jamjet_testkit::conformance::cases::support::race;
use jamjet_testkit::pg::{admin_url, TestDatabase, POOL_SIZE};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

/// The variable that moves this process's clock, under the `test-hooks`
/// feature, and how far the clock case moves it.
const SKEW_ENV: &str = "JAMJET_TEST_CLOCK_SKEW_SECS";
const SKEW_SECS: i64 = 3600;
/// What a child prints once it has run the clock case's body.
const CHILD_RAN: &str = "ran with the process clock an hour ahead";

fn settings() -> PgSettings {
    PgSettings {
        pool_size: POOL_SIZE,
        ..PgSettings::default()
    }
}

async fn store(db: &TestDatabase) -> PgBackend {
    PgBackend::connect(db.url(), settings())
        .await
        .unwrap_or_else(|e| panic!("connect to the test database: {e}"))
}

async fn make_tenant(b: &PgBackend, id: &str) -> PgBackend {
    let view = b.for_tenant(TenantId::from(id));
    let now = Utc::now();
    view.create_tenant(Tenant {
        id: TenantId::from(id),
        name: id.to_string(),
        status: TenantStatus::Active,
        policy: None,
        limits: None,
        created_at: now,
        updated_at: now,
    })
    .await
    .unwrap();
    view
}

/// When the reservation of `key` in `tenant` expires, and the database's
/// time now.
async fn expiry_of(pool: &PgPool, tenant: &str, key: &str) -> (DateTime<Utc>, DateTime<Utc>) {
    sqlx::query_as(
        "SELECT expires_at, statement_timestamp() FROM tool_reservations \
         WHERE tenant_id = $1 AND idempotency_key = $2",
    )
    .bind(tenant)
    .bind(key)
    .fetch_one(pool)
    .await
    .unwrap()
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

/// Two engines reserve one key at the same moment, 200 times on the
/// unscoped store and 200 times through a tenant's views: each time one
/// gets `Acquired`, the other `Held` with the winner as the owner. Some
/// rounds overlapped, so the two did meet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_reservers_one_key_one_winner() {
    const ROUNDS: usize = 200;
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("two_reservers_one_key_one_winner");
    };
    let (one, two) = (store(&db).await, store(&db).await);
    let acme = make_tenant(&one, "acme").await;
    let ttl = Duration::from_secs(300);
    let pairs: [(&str, PgBackend, PgBackend); 2] = [
        ("unscoped", one.clone(), two.clone()),
        ("acme", acme, two.for_tenant(TenantId::from("acme"))),
    ];
    for (whose, a, b) in pairs {
        let (a, b) = (Arc::new(a), Arc::new(b));
        let mut met = 0;
        for round in 0..ROUNDS {
            let key = format!("key-{round}");
            let run = ExecutionId::new();
            let (left, right, overlapped) = race(
                {
                    let (a, key, run) = (a.clone(), key.clone(), run.clone());
                    async move {
                        a.reserve_tool_effect(&key, &run, "n1", "worker-A", 1, ttl)
                            .await
                    }
                },
                {
                    let (b, key, run) = (b.clone(), key.clone(), run.clone());
                    async move {
                        b.reserve_tool_effect(&key, &run, "n1", "worker-B", 2, ttl)
                            .await
                    }
                },
            )
            .await;
            met += usize::from(overlapped);
            let outcomes = (left.unwrap(), right.unwrap());
            match &outcomes {
                (ReserveOutcome::Acquired, ReserveOutcome::Held { owner, .. })
                    if owner == "worker-A" => {}
                (ReserveOutcome::Held { owner, .. }, ReserveOutcome::Acquired)
                    if owner == "worker-B" => {}
                other => panic!("{whose}, round {round}: not one winner: {other:?}"),
            }
        }
        eprintln!("{whose}: {met} of {ROUNDS} rounds overlapped");
        assert!(met > 0, "{whose}: the reservers never ran at once");
    }
}

/// With this process's clock an hour ahead, a reservation of 300 s expires
/// 300 s from now by the database's clock, and holds against another worker
/// until then: by this process's clock it would already have lapsed. Once
/// it has lapsed by the database's clock, the other worker takes it.
#[tokio::test]
async fn a_reservation_lapses_by_the_database_clock() {
    const TEST: &str = "a_reservation_lapses_by_the_database_clock";
    if !in_a_skewed_child(TEST) {
        return;
    }
    let ahead = jamjet_state::clock::process_now() - Utc::now();
    assert!(
        (ahead.num_seconds() - SKEW_SECS).abs() <= 1,
        "the process clock is {ahead} ahead"
    );
    let db = TestDatabase::fresh()
        .await
        .expect("the parent checked for a Postgres server");
    let b = store(&db).await;
    let pool = db.pool().await;
    let run = ExecutionId::new();
    let ttl = Duration::from_secs(300);

    let first = b
        .reserve_tool_effect("k", &run, "n1", "worker-A", 1, ttl)
        .await
        .unwrap();
    assert_eq!(first, ReserveOutcome::Acquired);
    let (expires, now) = expiry_of(&pool, DEFAULT_TENANT, "k").await;
    let left = (expires - now).num_seconds();
    assert!(
        (295..=300).contains(&left),
        "the reservation expires {left} s from now by the database's clock"
    );
    let second = b
        .reserve_tool_effect("k", &run, "n1", "worker-B", 2, ttl)
        .await
        .unwrap();
    assert!(
        matches!(&second, ReserveOutcome::Held { owner, .. } if owner == "worker-A"),
        "a live reservation was taken over: {second:?}"
    );

    let lapsed = sqlx::query(
        "UPDATE tool_reservations SET expires_at = statement_timestamp() - interval '1 second' \
         WHERE idempotency_key = 'k'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(lapsed.rows_affected(), 1);
    let third = b
        .reserve_tool_effect("k", &run, "n1", "worker-B", 2, ttl)
        .await
        .unwrap();
    assert_eq!(
        third,
        ReserveOutcome::Acquired,
        "a lapsed reservation is taken"
    );
    println!("{CHILD_RAN}");
}

/// A TTL larger than any time can hold is held to the longest reservation:
/// the key is reserved, and expires that long from now.
#[tokio::test]
async fn a_huge_ttl_is_held_to_the_longest_reservation() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_huge_ttl_is_held_to_the_longest_reservation");
    };
    let b = store(&db).await;
    let pool = db.pool().await;
    let outcome = b
        .reserve_tool_effect("k", &ExecutionId::new(), "n1", "worker-A", 1, Duration::MAX)
        .await
        .unwrap();
    assert_eq!(outcome, ReserveOutcome::Acquired);
    let (expires, now) = expiry_of(&pool, DEFAULT_TENANT, "k").await;
    let held = (expires - now).to_std().expect("the expiry is ahead");
    let longest = MAX_RESERVATION_TTL;
    assert!(
        held <= longest && longest - held < Duration::from_secs(60),
        "held {held:?}, the longest is {longest:?}"
    );
}
