//! The audit chain on Postgres, where several engines append to one log:
//! appends take turns on the head row, the schema refuses a second genesis
//! and a second successor, one event gets one entry, and entries are dated
//! by the database's clock.

use chrono::{DateTime, SubsecRound, Utc};
use jamjet_audit::backend::SealOntoHead;
use jamjet_audit::{
    verify_chain, ActorType, AuditBackend, AuditEnricher, AuditError, AuditLogEntry, AuditQuery,
    AuditRecord, AuditSigner, PgAuditBackend, SqliteAuditBackend,
};
use jamjet_core::workflow::ExecutionId;
use jamjet_state::event::{Event, EventKind};
use jamjet_state::{PgBackend, PgSettings};
use jamjet_testkit::pg::{admin_url, TestDatabase, POOL_SIZE};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use uuid::Uuid;

/// The records each of the two writers seals.
const PER_WRITER: usize = 500;

fn record(kind: &str) -> AuditRecord {
    AuditRecord {
        event_type: kind.into(),
        actor_id: "cli".into(),
        actor_type: ActorType::Human,
        tenant_id: "default".into(),
        detail: serde_json::json!({ "type": kind }),
    }
}

fn signer() -> AuditSigner {
    AuditSigner::new(b"a-test-key".to_vec())
}

/// A pool on the test database set up as the engine sets up its own.
async fn pool(db: &TestDatabase) -> sqlx::PgPool {
    let settings = PgSettings {
        pool_size: POOL_SIZE,
        ..PgSettings::default()
    };
    PgBackend::connect(db.url(), settings)
        .await
        .unwrap_or_else(|e| panic!("connect to the test database: {e}"))
        .pool()
}

/// Every entry, oldest first.
async fn chain(backend: &dyn AuditBackend) -> Vec<AuditLogEntry> {
    let mut all = backend
        .query(&AuditQuery {
            limit: 10_000,
            ..Default::default()
        })
        .await
        .unwrap();
    all.reverse();
    all
}

/// An audit backend that counts the linked appends refused because another
/// writer moved the head first.
struct Counting {
    inner: PgAuditBackend,
    refused: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl AuditBackend for Counting {
    async fn append(&self, entry: AuditLogEntry) -> Result<(), AuditError> {
        self.inner.append(entry).await
    }
    async fn append_linked(&self, entry: AuditLogEntry) -> Result<bool, AuditError> {
        let done = self.inner.append_linked(entry).await?;
        if !done {
            self.refused.fetch_add(1, Ordering::SeqCst);
        }
        Ok(done)
    }
    async fn append_sealed(
        &self,
        seal: &SealOntoHead<'_>,
    ) -> Result<Option<(Option<String>, DateTime<Utc>)>, AuditError> {
        self.inner.append_sealed(seal).await
    }
    async fn head_at(&self) -> Result<Option<(Option<String>, DateTime<Utc>)>, AuditError> {
        self.inner.head_at().await
    }
    async fn head(&self) -> Result<Option<String>, AuditError> {
        self.inner.head().await
    }
    async fn query(&self, q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError> {
        self.inner.query(q).await
    }
    async fn count(&self, q: &AuditQuery) -> Result<u64, AuditError> {
        self.inner.count(q).await
    }
    async fn now(&self) -> Result<DateTime<Utc>, AuditError> {
        self.inner.now().await
    }
}

/// Two engines, each with its own pool and its own idea of the head, seal
/// 500 records each at once. The log is one chain of 1,000 entries with one
/// genesis, and the writers did collide (a linked append was refused).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_writers_never_fork_the_chain() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("two_writers_never_fork_the_chain");
    };
    let refused = Arc::new(AtomicU64::new(0));
    let mut writers = Vec::new();
    for _ in 0..2 {
        let backend = Counting {
            inner: PgAuditBackend::new(pool(&db).await),
            refused: refused.clone(),
        };
        writers.push(Arc::new(AuditEnricher::with_signer(
            Arc::new(backend),
            signer(),
        )));
    }
    let tasks: Vec<_> = writers
        .iter()
        .enumerate()
        .map(|(w, writer)| {
            let writer = writer.clone();
            tokio::spawn(async move {
                for n in 0..PER_WRITER {
                    writer
                        .seal_record(record(&format!("w{w}-{n}")))
                        .await
                        .unwrap_or_else(|e| panic!("writer {w}, record {n}: {e}"));
                }
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }

    let reader = PgAuditBackend::new(pool(&db).await);
    let log = chain(&reader).await;
    assert_eq!(log.len(), 2 * PER_WRITER);
    verify_chain(&log, &signer()).expect("one chain, listed in order");
    let genesis = log.iter().filter(|e| e.prev_hash.is_none()).count();
    assert_eq!(genesis, 1, "entries with no predecessor");
    assert_eq!(reader.head().await.unwrap(), log.last().unwrap().entry_hash);
    let refused = refused.load(Ordering::SeqCst);
    assert!(refused > 0, "the writers never collided");
    println!("linked appends refused for a moved head: {refused}");
}

/// The sqlx error of a raw insert, which must be a unique violation of
/// `constraint`.
fn assert_unique_violation(
    result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>,
    constraint: &str,
) {
    let e = result.expect_err("the insert was refused");
    let db = e.as_database_error().expect("a database error");
    assert_eq!(db.code().as_deref(), Some("23505"), "{e}");
    assert_eq!(db.constraint(), Some(constraint), "{e}");
}

/// A raw insert of a row, skipping the backend: `prev_hash` as given.
async fn raw_insert(
    pool: &sqlx::PgPool,
    prev_hash: Option<&str>,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_log (id, event_id, execution_id, sequence, event_type, actor_id, \
         actor_type, created_at, raw_event, prev_hash, entry_hash) \
         VALUES ($1, $2, '', 0, 'forged', 'mallory', 'human', statement_timestamp(), '{}', $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(prev_hash)
    .bind(format!("forged-{}", Uuid::new_v4()))
    .execute(pool)
    .await
}

/// A row with no `prev_hash` beside the genesis is refused by the schema,
/// whoever writes it.
#[tokio::test]
async fn a_second_genesis_is_refused() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_second_genesis_is_refused");
    };
    let pool = pool(&db).await;
    let enricher =
        AuditEnricher::with_signer(Arc::new(PgAuditBackend::new(pool.clone())), signer());
    enricher.seal_record(record("genesis")).await.unwrap();
    assert_unique_violation(raw_insert(&pool, None).await, "audit_log_one_genesis");
}

/// A row linking to an entry that already has a successor is refused by
/// the schema, whoever writes it.
#[tokio::test]
async fn a_fork_by_raw_insert_is_refused() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_fork_by_raw_insert_is_refused");
    };
    let pool = pool(&db).await;
    let backend = PgAuditBackend::new(pool.clone());
    let enricher =
        AuditEnricher::with_signer(Arc::new(PgAuditBackend::new(pool.clone())), signer());
    for kind in ["one", "two", "three"] {
        enricher.seal_record(record(kind)).await.unwrap();
    }
    let log = chain(&backend).await;
    let linked = log[0].entry_hash.as_deref().expect("sealed");
    assert_eq!(log[1].prev_hash.as_deref(), Some(linked));
    assert_unique_violation(
        raw_insert(&pool, Some(linked)).await,
        "audit_log_one_successor",
    );
    assert_eq!(chain(&backend).await.len(), 3);
}

fn decision(execution: &ExecutionId, sequence: i64) -> Event {
    Event::new(
        execution.clone(),
        sequence,
        EventKind::WorkflowCompleted {
            final_state: serde_json::json!({ "sequence": sequence }),
        },
    )
}

/// One event sealed twice (the inline seal and a retry) is one entry, and
/// the second seal is no error and no seal failure. The chain goes on from
/// the one entry.
#[tokio::test]
async fn the_same_event_sealed_twice_is_one_entry() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("the_same_event_sealed_twice_is_one_entry");
    };
    let backend = PgAuditBackend::new(pool(&db).await);
    let enricher =
        AuditEnricher::with_signer(Arc::new(PgAuditBackend::new(pool(&db).await)), signer());
    let execution = ExecutionId(Uuid::new_v4());
    let event = decision(&execution, 1);
    enricher.try_enrich_and_append(&event, None).await.unwrap();
    enricher
        .try_enrich_and_append(&event, None)
        .await
        .expect("a second seal of one event is no error");
    assert_eq!(enricher.seal_failures(), 0);
    let log = chain(&backend).await;
    let entries: Vec<_> = log.iter().filter(|e| e.event_id == event.id).collect();
    assert_eq!(entries.len(), 1, "entries for the event");

    enricher
        .try_enrich_and_append(&decision(&execution, 2), None)
        .await
        .unwrap();
    let log = chain(&backend).await;
    assert_eq!(log.len(), 2);
    verify_chain(&log, &signer()).expect("one chain");
}

/// Postgres keeps microseconds, and the entry's hash covers its times as
/// written: an entry dated to the nanosecond would read back as another
/// time and never verify, so it is refused and nothing is written.
#[tokio::test]
async fn a_time_finer_than_a_microsecond_is_refused() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_time_finer_than_a_microsecond_is_refused");
    };
    let backend = PgAuditBackend::new(pool(&db).await);
    let mut entry = AuditLogEntry::new(
        Uuid::new_v4(),
        String::new(),
        0,
        "fine".into(),
        "cli".into(),
        ActorType::Human,
        serde_json::json!({}),
    );
    entry.created_at = Utc::now().trunc_subsecs(6) + chrono::Duration::nanoseconds(1);
    entry.seal(None, &signer());
    let e = backend.append_linked(entry.clone()).await.unwrap_err();
    assert!(e.to_string().contains("finer than a microsecond"), "{e}");
    assert_eq!(backend.head_at().await.unwrap(), None);

    entry.created_at = entry.created_at.trunc_subsecs(6);
    entry.seal(None, &signer());
    assert!(backend.append_linked(entry).await.unwrap());
    verify_chain(&chain(&backend).await, &signer()).expect("reads back as sealed");
}

/// On a log with entries, a plain append must continue the chain too: one
/// linked to an entry that is not the head is an error and writes nothing.
#[tokio::test]
async fn an_unlinked_append_must_continue_the_chain() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("an_unlinked_append_must_continue_the_chain");
    };
    let backend = PgAuditBackend::new(pool(&db).await);
    let t0 = Utc::now().trunc_subsecs(6);
    let entry = |kind: &str, prev: Option<String>, at: DateTime<Utc>| {
        let mut e = AuditLogEntry::new(
            Uuid::new_v4(),
            String::new(),
            0,
            kind.into(),
            "cli".into(),
            ActorType::Human,
            serde_json::json!({}),
        );
        e.created_at = at;
        e.seal(prev, &signer());
        e
    };
    let one = entry("one", None, t0);
    backend.append(one.clone()).await.unwrap();
    let dangling = entry(
        "dangling",
        Some("no-such-entry".into()),
        t0 + chrono::Duration::seconds(1),
    );
    let e = backend.append(dangling).await.unwrap_err();
    assert!(e.to_string().contains("does not continue the chain"), "{e}");
    assert_eq!(backend.head().await.unwrap(), one.entry_hash);

    let two = entry(
        "two",
        one.entry_hash.clone(),
        t0 + chrono::Duration::seconds(1),
    );
    backend.append(two.clone()).await.unwrap();
    assert_eq!(backend.head().await.unwrap(), two.entry_hash);
    let log = chain(&backend).await;
    assert_eq!(log.len(), 2);
    verify_chain(&log, &signer()).expect("one chain");
}

/// An audit backend over SQLite whose clock starts two days ahead and
/// moves a day each time it is read.
struct AheadClock {
    inner: SqliteAuditBackend,
    at: DateTime<Utc>,
    reads: AtomicU64,
}

#[async_trait::async_trait]
impl AuditBackend for AheadClock {
    async fn append(&self, entry: AuditLogEntry) -> Result<(), AuditError> {
        self.inner.append(entry).await
    }
    async fn append_linked(&self, entry: AuditLogEntry) -> Result<bool, AuditError> {
        self.inner.append_linked(entry).await
    }
    async fn head_at(&self) -> Result<Option<(Option<String>, DateTime<Utc>)>, AuditError> {
        self.inner.head_at().await
    }
    async fn head(&self) -> Result<Option<String>, AuditError> {
        self.inner.head().await
    }
    async fn query(&self, q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError> {
        self.inner.query(q).await
    }
    async fn count(&self, q: &AuditQuery) -> Result<u64, AuditError> {
        self.inner.count(q).await
    }
    async fn now(&self) -> Result<DateTime<Utc>, AuditError> {
        let n = self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.at + chrono::Duration::days(n as i64))
    }
}

/// The enricher dates each entry by its backend's clock, not its own: the
/// first, sealed onto a head it reads, and the next, sealed onto the head
/// it keeps.
#[tokio::test]
async fn the_enricher_dates_entries_by_its_backends_clock() {
    let inner = SqliteAuditBackend::open("sqlite::memory:").await.unwrap();
    inner.migrate().await.unwrap();
    let at = Utc::now().trunc_subsecs(6) + chrono::Duration::days(2);
    let backend = Arc::new(AheadClock {
        inner,
        at,
        reads: AtomicU64::new(0),
    });
    let enricher = AuditEnricher::with_signer(backend.clone(), signer());
    enricher.seal_record(record("first")).await.unwrap();
    enricher.seal_record(record("next")).await.unwrap();
    let log = chain(&*backend).await;
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].created_at, at);
    assert!(
        log[1].created_at >= at + chrono::Duration::days(1),
        "the next entry is dated {}, not by the backend's clock",
        log[1].created_at
    );
    verify_chain(&log, &signer()).expect("one chain");
}

const SKEW_ENV: &str = "JAMJET_TEST_CLOCK_SKEW_SECS";
const SKEW_SECS: i64 = 3600;
const CHILD_RAN: &str = "the skewed child ran its case";

/// Whether to run the case's body here: in the child the parent started
/// with the process clock an hour ahead. In the parent, start that child,
/// require it to pass, and answer false.
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

/// With the process clock an hour ahead, entries are dated by the
/// database's clock: the first, sealed onto a head it reads, and the next,
/// sealed onto the head the writer keeps.
#[tokio::test]
async fn entries_are_dated_by_the_database_clock() {
    if !in_a_skewed_child("entries_are_dated_by_the_database_clock") {
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
    let pool = pool(&db).await;
    let enricher =
        AuditEnricher::with_signer(Arc::new(PgAuditBackend::new(pool.clone())), signer());
    enricher.seal_record(record("first")).await.unwrap();
    enricher.seal_record(record("next")).await.unwrap();
    let db_now: DateTime<Utc> = sqlx::query_scalar("SELECT statement_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let log = chain(&PgAuditBackend::new(pool)).await;
    assert_eq!(log.len(), 2);
    for e in &log {
        let off = e.created_at - db_now;
        assert!(
            off.num_seconds().abs() <= 5,
            "{} is dated {off} off the database's clock",
            e.event_type
        );
    }
    println!("{CHILD_RAN}");
}
