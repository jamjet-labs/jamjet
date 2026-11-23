//! An engine killed at each kind of write, then started again, finishes the
//! run and leaves the store whole.
//!
//! Each case starts engine A with `JAMJET_TEST_CRASH_AT` naming one fault
//! point, drives one run to it, and sees A abort there: SIGABRT, with the
//! point on stderr. To the database that is a kill -9 inside the write.
//! The case checks what the store holds at that moment, starts A again
//! without the variable, finishes the run, and checks the invariants
//! ([`jamjet_testkit::invariants`]). A binary built without the fault points
//! cannot pass: A would never die.
//!
//! The kinds of write, and the point each case arms:
//!
//! | Case | SQLite | Postgres |
//! |---|---|---|
//! | `dispatch.before_commit` | `sqlite.commit_scheduler_pass#8` | `pg.commit_scheduler_pass#8` |
//! | `dispatch.after_commit` | `dispatch.after_commit#1` | the same |
//! | `reclaim.before_commit` | `sqlite.reclaim_expired_leases_recorded#7` | `pg.reclaim_expired_leases_recorded#6` |
//! | `commit_turn.before_commit` | `sqlite_scoped.commit_turn#11` | `pg.commit_turn#10` |
//! | `finish.before_commit` | `sqlite.commit_scheduler_pass#7` | `pg.commit_scheduler_pass#7` |
//! | `fail_fenced.before_commit` | `sqlite.fail_work_item_fenced_recorded#7` | `pg.fail_work_item_fenced_recorded#6` |
//! | `decision.after_append_before_seal` | `#1`, the REST route and the MCP tool | the same |
//! | `audit.seal.before_commit` | none (SQLite has no outbox) | `#1` |
//!
//! A store transaction's last point sits on the line before its commit, so
//! the `*.before_commit` cases arm it. The scheduler's pass reaches `#7`
//! only when it finishes a run, and `#8` on every pass that writes. A
//! worker's completion over HTTP runs on a tenant's view of the SQLite
//! store, which has its own `commit_turn` body.
//!
//! On SQLite a decision whose engine dies before its seal has no audit
//! entry, ever: SQLite has no outbox. Those cases assert that gap
//! (`..._known_gap`). On Postgres the outbox drain seals it after the
//! restart.
//!
//! Four cases run at a time, each on its own store. The Postgres cases skip
//! without `JAMJET_TEST_POSTGRES_URL`.
#![cfg(unix)]

#[path = "support/engine.rs"]
mod engine;

use engine::{
    approve, approve_then_work, bases, call, claim, client, complete, db_id, fail, register,
    start_run, wait_for_request, work_only, worker, Answers, Backend, Claimed, Engine, TestStore,
    CRASH_AT,
};
use jamjet_testkit::invariants::Snapshot;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The most cases that run at once.
const AT_ONCE: usize = 4;
/// One case, so a hang fails instead of eating the run. The slowest case
/// waits out a 30 s lease.
const CASE: Duration = Duration::from_secs(120);
/// How long an engine may take to reach its point.
const TO_DIE: Duration = Duration::from_secs(30);

/// A place among the [`AT_ONCE`] cases running, held for the whole case.
struct Slot;

static RUNNING: Mutex<usize> = Mutex::new(0);
static FREED: Condvar = Condvar::new();

impl Slot {
    fn take() -> Slot {
        let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        while *running >= AT_ONCE {
            running = FREED.wait(running).unwrap_or_else(|e| e.into_inner());
        }
        *running += 1;
        Slot
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        *RUNNING.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        FREED.notify_one();
    }
}

/// Run one case in a slot, on a runtime of its own, within [`CASE`].
fn case<F: std::future::Future<Output = ()>>(name: &str, body: F) {
    let _slot = Slot::take();
    let start = Instant::now();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(CASE, body)
                .await
                .unwrap_or_else(|_| panic!("{name} ran past {CASE:?}"))
        });
    eprintln!("{name}: {:.1} s", start.elapsed().as_secs_f64());
}

/// The fault point a case arms on each backend.
fn point(backend: Backend, sqlite: &'static str, postgres: &'static str) -> &'static str {
    match backend {
        Backend::Sqlite => sqlite,
        Backend::Postgres => postgres,
    }
}

/// One run on its own store, and engine A with the fault point armed.
struct Kill {
    store: TestStore,
    a: Engine,
    http: reqwest::Client,
    point: &'static str,
    /// The run, as the API names it.
    run: String,
}

impl Kill {
    /// A store on `backend` with engine A armed at `point`, embedded
    /// workers on or off, and one run of `ir` started. `None` when the
    /// backend is not configured.
    async fn begin(
        backend: Backend,
        name: &str,
        point: &'static str,
        ir: Value,
        embedded_workers: bool,
    ) -> Option<Kill> {
        let store = TestStore::on(backend, name).await?;
        let workers = if embedded_workers { "1" } else { "0" };
        let a = Engine::start(
            &store,
            &[(CRASH_AT, point), ("JAMJET_EMBED_WORKERS", workers)],
        )
        .await;
        let http = client();
        register(&http, &a.base(), &store.developer, &ir).await;
        let workflow = ir["workflow_id"].as_str().unwrap();
        let run = start_run(&http, &a.base(), &store.developer, workflow).await;
        Some(Kill {
            store,
            a,
            http,
            point,
            run,
        })
    }

    fn id(&self) -> String {
        db_id(&self.run)
    }

    async fn died(&mut self) {
        self.a.died_at(self.point, TO_DIE).await;
    }

    /// The run's event kinds, as the store holds them now.
    async fn kinds(&self) -> Vec<String> {
        let snapshot = self.store.snapshot().await;
        snapshot
            .events_of(&self.id())
            .map(|e| e.kind_type().to_string())
            .collect()
    }

    async fn request(&self) -> Value {
        wait_for_request(
            &self.http,
            &self.a.base(),
            &self.store.reviewer,
            &self.run,
            Duration::from_secs(15),
        )
        .await
    }

    async fn claim(&self, lease_secs: u64) -> Claimed {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let (st, item) = claim(
                &self.http,
                &self.a.base(),
                &self.store.developer,
                "worker-1",
                lease_secs,
            )
            .await
            .unwrap();
            if let Some(item) = item {
                return item;
            }
            assert_eq!(st, 200);
            assert!(Instant::now() < deadline, "nothing to claim");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Start A again without the fault point unless a case already did,
    /// finish the run with a worker over HTTP, and check the invariants.
    /// The store as the run finished.
    async fn finish(&mut self) -> Snapshot {
        if !self.a.is_running() {
            self.a.restart().await;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let w = worker(
            self.http.clone(),
            bases(&[&self.a]),
            self.store.developer.clone(),
            "worker-2",
            Answers::default(),
            stop.clone(),
        );
        let id = self.id();
        let deadline = Instant::now() + Duration::from_secs(60);
        let snapshot = loop {
            let snapshot = self.store.snapshot().await;
            if snapshot.status_of(&id) == Some("completed") {
                break snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "the run did not finish after the restart: {:?}\n{}",
                snapshot
                    .events_of(&id)
                    .map(|e| e.kind_type())
                    .collect::<Vec<_>>(),
                self.a.log()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        stop.store(true, Ordering::SeqCst);
        w.await.unwrap();
        let violations = jamjet_testkit::invariants::check_snapshot(&snapshot);
        assert!(
            violations.is_empty(),
            "invariants broken after a kill at {}:\n{}",
            self.point,
            violations
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        );
        snapshot
    }
}

/// How many times `kind` is in `kinds`.
fn count(kinds: &[String], kind: &str) -> usize {
    kinds.iter().filter(|k| *k == kind).count()
}

fn workflow_id(name: &str) -> String {
    format!("{name}-{}", uuid::Uuid::new_v4().simple())
}

// ── The scheduler's pass ─────────────────────────────────────────────────────

/// The pass that dispatches `approve` again after its decision dies before
/// its commit: none of it is written, and after the restart the pass runs
/// again.
async fn dispatch_before_commit(backend: Backend, name: &str) {
    let point = point(
        backend,
        "sqlite.commit_scheduler_pass#8",
        "pg.commit_scheduler_pass#8",
    );
    let ir = approve_then_work(&workflow_id("dispatch"), None);
    let Some(mut k) = Kill::begin(backend, name, point, ir, true).await else {
        return;
    };
    let row = k.request().await;
    let (st, body) = approve(&k.http, &k.a.base(), &k.store.reviewer, &row)
        .await
        .unwrap();
    assert_eq!(st, 200, "{body}");
    k.died().await;
    let kinds = k.kinds().await;
    assert_eq!(
        kinds.last().map(String::as_str),
        Some("approval_received"),
        "nothing of the pass was written: {kinds:?}"
    );
    assert_eq!(count(&kinds, "node_scheduled"), 1, "{kinds:?}");
    k.finish().await;
}

#[test]
fn dispatch_before_commit_on_sqlite() {
    case("dispatch_before_commit_on_sqlite", async {
        dispatch_before_commit(Backend::Sqlite, "dispatch_before_commit_on_sqlite").await
    });
}

#[test]
fn dispatch_before_commit_on_postgres() {
    case("dispatch_before_commit_on_postgres", async {
        dispatch_before_commit(Backend::Postgres, "dispatch_before_commit_on_postgres").await
    });
}

/// The same pass dies after its commit, before the scheduler updates what
/// it keeps for the run: the dispatch is written once, with one live item,
/// and the engine started again does not dispatch it a second time.
async fn dispatch_after_commit(backend: Backend, name: &str) {
    let ir = approve_then_work(&workflow_id("dispatched"), None);
    let Some(mut k) = Kill::begin(backend, name, "dispatch.after_commit#1", ir, true).await else {
        return;
    };
    let row = k.request().await;
    let (st, body) = approve(&k.http, &k.a.base(), &k.store.reviewer, &row)
        .await
        .unwrap();
    assert_eq!(st, 200, "{body}");
    k.died().await;
    let kinds = k.kinds().await;
    assert_eq!(
        count(&kinds, "node_scheduled"),
        2,
        "the pass was written: {kinds:?}"
    );
    let snapshot = k.store.snapshot().await;
    assert_eq!(
        snapshot
            .live_items
            .iter()
            .filter(|(run, node)| *run == k.id() && node == "approve")
            .count(),
        1,
        "one live item for the dispatched node"
    );
    let done = k.finish().await;
    let scheduled = done
        .events_of(&k.id())
        .filter(|e| e.kind_type() == "node_scheduled" && e.node() == Some("approve"))
        .count();
    assert_eq!(
        scheduled, 2,
        "approve was dispatched again only after its decision"
    );
}

#[test]
fn dispatch_after_commit_on_sqlite() {
    case("dispatch_after_commit_on_sqlite", async {
        dispatch_after_commit(Backend::Sqlite, "dispatch_after_commit_on_sqlite").await
    });
}

#[test]
fn dispatch_after_commit_on_postgres() {
    case("dispatch_after_commit_on_postgres", async {
        dispatch_after_commit(Backend::Postgres, "dispatch_after_commit_on_postgres").await
    });
}

/// The pass that finishes the run dies before its commit: the run is still
/// running with its last step completed, and the engine started again
/// finishes it once.
async fn finish_before_commit(backend: Backend, name: &str) {
    let point = point(
        backend,
        "sqlite.commit_scheduler_pass#7",
        "pg.commit_scheduler_pass#7",
    );
    let ir = work_only(&workflow_id("finish"));
    let Some(mut k) = Kill::begin(backend, name, point, ir, false).await else {
        return;
    };
    let item = k.claim(30).await;
    let (st, body) = complete(&k.http, &k.a.base(), &k.store.developer, &item)
        .await
        .unwrap();
    assert_eq!(st, 200, "{body}");
    k.died().await;
    let kinds = k.kinds().await;
    assert_eq!(
        kinds.last().map(String::as_str),
        Some("node_completed"),
        "{kinds:?}"
    );
    assert_eq!(k.store.snapshot().await.status_of(&k.id()), Some("running"));
    k.finish().await;
}

#[test]
fn finish_before_commit_on_sqlite() {
    case("finish_before_commit_on_sqlite", async {
        finish_before_commit(Backend::Sqlite, "finish_before_commit_on_sqlite").await
    });
}

#[test]
fn finish_before_commit_on_postgres() {
    case("finish_before_commit_on_postgres", async {
        finish_before_commit(Backend::Postgres, "finish_before_commit_on_postgres").await
    });
}

// ── Leases and a worker's reports ────────────────────────────────────────────

/// The scheduler's sweep of an expired lease dies before its commit: the
/// item is still claimed with no failure recorded, and after the restart
/// the sweep takes it once and the node runs again. No embedded workers, so
/// no claim reclaims the lease before the sweep does.
async fn reclaim_before_commit(backend: Backend, name: &str) {
    let point = point(
        backend,
        "sqlite.reclaim_expired_leases_recorded#7",
        "pg.reclaim_expired_leases_recorded#6",
    );
    let ir = work_only(&workflow_id("reclaim"));
    let Some(mut k) = Kill::begin(backend, name, point, ir, false).await else {
        return;
    };
    // The shortest lease the engine grants, left to run out.
    let _abandoned = k.claim(5).await;
    k.died().await;
    let kinds = k.kinds().await;
    assert_eq!(count(&kinds, "node_failed"), 0, "{kinds:?}");
    assert_eq!(count(&kinds, "retry_scheduled"), 0, "{kinds:?}");
    let done = k.finish().await;
    let id = k.id();
    let kinds: Vec<&str> = done.events_of(&id).map(|e| e.kind_type()).collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "retry_scheduled").count(),
        1,
        "reclaimed once: {kinds:?}"
    );
}

#[test]
fn reclaim_before_commit_on_sqlite() {
    case("reclaim_before_commit_on_sqlite", async {
        reclaim_before_commit(Backend::Sqlite, "reclaim_before_commit_on_sqlite").await
    });
}

#[test]
fn reclaim_before_commit_on_postgres() {
    case("reclaim_before_commit_on_postgres", async {
        reclaim_before_commit(Backend::Postgres, "reclaim_before_commit_on_postgres").await
    });
}

/// A worker's fenced completion dies before its commit: no `NodeCompleted`,
/// the lease still held, and the worker's retry after the restart lands
/// once.
async fn commit_turn_before_commit(backend: Backend, name: &str) {
    let point = point(backend, "sqlite_scoped.commit_turn#11", "pg.commit_turn#10");
    let ir = work_only(&workflow_id("turn"));
    let Some(mut k) = Kill::begin(backend, name, point, ir, false).await else {
        return;
    };
    let item = k.claim(30).await;
    let lost = complete(&k.http, &k.a.base(), &k.store.developer, &item).await;
    assert!(
        lost.is_err(),
        "the engine answered a completion it died in: {lost:?}"
    );
    k.died().await;
    let kinds = k.kinds().await;
    assert_eq!(count(&kinds, "node_completed"), 0, "{kinds:?}");
    k.a.restart().await;
    let (st, body) = complete(&k.http, &k.a.base(), &k.store.developer, &item)
        .await
        .unwrap();
    assert_eq!(st, 200, "the retried completion lands: {body}");
    let done = k.finish().await;
    let completed = done
        .events_of(&k.id())
        .filter(|e| e.kind_type() == "node_completed")
        .count();
    assert_eq!(completed, 1);
}

#[test]
fn commit_turn_before_commit_on_sqlite() {
    case("commit_turn_before_commit_on_sqlite", async {
        commit_turn_before_commit(Backend::Sqlite, "commit_turn_before_commit_on_sqlite").await
    });
}

#[test]
fn commit_turn_before_commit_on_postgres() {
    case("commit_turn_before_commit_on_postgres", async {
        commit_turn_before_commit(Backend::Postgres, "commit_turn_before_commit_on_postgres").await
    });
}

/// A worker's fenced failure report dies before its commit: nothing is
/// recorded and the lease still holds. The worker sends it again after the
/// restart; the node is retried once and completes.
async fn fail_fenced_before_commit(backend: Backend, name: &str) {
    let point = point(
        backend,
        "sqlite.fail_work_item_fenced_recorded#7",
        "pg.fail_work_item_fenced_recorded#6",
    );
    let ir = work_only(&workflow_id("fail"));
    let Some(mut k) = Kill::begin(backend, name, point, ir, false).await else {
        return;
    };
    let item = k.claim(30).await;
    let lost = fail(&k.http, &k.a.base(), &k.store.developer, &item).await;
    assert!(
        lost.is_err(),
        "the engine answered a report it died in: {lost:?}"
    );
    k.died().await;
    let kinds = k.kinds().await;
    assert_eq!(count(&kinds, "node_failed"), 0, "{kinds:?}");
    k.a.restart().await;
    let (st, body) = fail(&k.http, &k.a.base(), &k.store.developer, &item)
        .await
        .unwrap();
    assert_eq!(st, 200, "the report sent again lands: {body}");
    let done = k.finish().await;
    let id = k.id();
    let kinds: Vec<&str> = done.events_of(&id).map(|e| e.kind_type()).collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "node_failed").count(),
        1,
        "{kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|k| **k == "retry_scheduled").count(),
        1,
        "{kinds:?}"
    );
}

#[test]
fn fail_fenced_before_commit_on_sqlite() {
    case("fail_fenced_before_commit_on_sqlite", async {
        fail_fenced_before_commit(Backend::Sqlite, "fail_fenced_before_commit_on_sqlite").await
    });
}

#[test]
fn fail_fenced_before_commit_on_postgres() {
    case("fail_fenced_before_commit_on_postgres", async {
        fail_fenced_before_commit(Backend::Postgres, "fail_fenced_before_commit_on_postgres").await
    });
}

// ── Decisions and their seal ─────────────────────────────────────────────────

/// How a decision is sent.
#[derive(Clone, Copy)]
enum Via {
    Rest,
    Mcp,
}

/// Decide the listed request through A, by `via`.
async fn decide(k: &Kill, row: &Value, via: Via) -> Result<(u16, Value), reqwest::Error> {
    match via {
        Via::Rest => approve(&k.http, &k.a.base(), &k.store.reviewer, row).await,
        Via::Mcp => {
            let mut arguments = engine::approval(row);
            arguments["execution_id"] = row["execution_id"].clone();
            call(
                &k.http,
                &k.a.base(),
                "POST",
                "/mcp",
                &k.store.reviewer,
                Some(json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": { "name": "jamjet_approve", "arguments": arguments },
                })),
            )
            .await
        }
    }
}

/// How many entries the SQLite audit log holds for `event_id`.
async fn sqlite_entries(k: &Kill, event_id: &str) -> i64 {
    use sqlx::Connection;
    let file = k.store.file().unwrap();
    let mut conn = sqlx::SqliteConnection::connect(&format!("sqlite://{}", file.display()))
        .await
        .unwrap();
    let n = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE event_id = ?")
        .bind(event_id)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    n
}

/// The id of the run's one event of `kind`.
fn event_of(snapshot: &Snapshot, run: &str, kind: &str) -> String {
    let ids: Vec<&str> = snapshot
        .events_of(run)
        .filter(|e| e.kind_type() == kind)
        .map(|e| e.id.as_str())
        .collect();
    assert_eq!(ids.len(), 1, "one {kind} in the run");
    ids[0].to_string()
}

/// Wait for the restarted engine's outbox drain to seal `event_id`. It
/// seals an event whose outbox row is 5 s old, every 2 s.
async fn sealed_by_the_drain(k: &Kill, event_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while k.store.snapshot().await.entries_for(event_id) == 0 {
        assert!(
            Instant::now() < deadline,
            "the drain never sealed {event_id}: {}",
            k.a.log()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The engine dies between a decision's append and its inline seal: the
/// decision is recorded and has no audit entry. On SQLite it never gets
/// one (the known gap); on Postgres the drain seals it once after the
/// restart.
async fn decision_after_append_before_seal(backend: Backend, name: &str, via: Via) {
    let ir = approve_then_work(&workflow_id("decision"), None);
    let point = "decision.after_append_before_seal#1";
    let Some(mut k) = Kill::begin(backend, name, point, ir, true).await else {
        return;
    };
    let row = k.request().await;
    let lost = decide(&k, &row, via).await;
    assert!(
        lost.is_err(),
        "the engine answered a decision it died in: {lost:?}"
    );
    k.died().await;
    let snapshot = k.store.snapshot().await;
    let request = event_of(&snapshot, &k.id(), "tool_approval_required");
    let decision = event_of(&snapshot, &k.id(), "approval_received");
    match backend {
        Backend::Sqlite => {
            assert_eq!(
                sqlite_entries(&k, &request).await,
                1,
                "the request was sealed"
            );
            assert_eq!(
                sqlite_entries(&k, &decision).await,
                0,
                "the decision was not"
            );
        }
        Backend::Postgres => {
            assert_eq!(
                snapshot.entries_for(&decision),
                0,
                "the decision was not sealed"
            );
            let outbox = &snapshot.audit.as_ref().unwrap().outbox;
            assert_eq!(outbox, &vec![decision.clone()], "the outbox holds it");
            k.a.restart().await;
            sealed_by_the_drain(&k, &decision).await;
        }
    }
    let done = k.finish().await;
    match backend {
        Backend::Sqlite => {
            assert_eq!(
                sqlite_entries(&k, &decision).await,
                0,
                "SQLite has no outbox: the decision stays unsealed"
            );
        }
        // `finish` checked rule 5: one entry for each sealed event, and
        // an empty outbox.
        Backend::Postgres => assert_eq!(done.entries_for(&decision), 1),
    }
}

#[test]
fn decision_after_append_before_seal_on_sqlite_known_gap() {
    let name = "decision_after_append_before_seal_on_sqlite_known_gap";
    case(name, async {
        decision_after_append_before_seal(Backend::Sqlite, name, Via::Rest).await
    });
}

#[test]
fn decision_after_append_before_seal_on_postgres() {
    let name = "decision_after_append_before_seal_on_postgres";
    case(name, async {
        decision_after_append_before_seal(Backend::Postgres, name, Via::Rest).await
    });
}

#[test]
fn mcp_decision_after_append_before_seal_on_sqlite_known_gap() {
    let name = "mcp_decision_after_append_before_seal_on_sqlite_known_gap";
    case(name, async {
        decision_after_append_before_seal(Backend::Sqlite, name, Via::Mcp).await
    });
}

#[test]
fn mcp_decision_after_append_before_seal_on_postgres() {
    let name = "mcp_decision_after_append_before_seal_on_postgres";
    case(name, async {
        decision_after_append_before_seal(Backend::Postgres, name, Via::Mcp).await
    });
}

/// The engine dies inside the audit log's commit while it seals the run's
/// approval request: the head locked, the entry written and the outbox row
/// deleted, none of it committed. After the restart the drain seals the
/// request once. The embedded worker died holding the approval node's item,
/// so the run goes on once that 30 s lease runs out.
#[test]
fn audit_seal_before_commit_on_postgres() {
    let name = "audit_seal_before_commit_on_postgres";
    case(name, async {
        let ir = approve_then_work(&workflow_id("seal"), None);
        let point = "audit.seal.before_commit#1";
        let Some(mut k) = Kill::begin(Backend::Postgres, name, point, ir, true).await else {
            return;
        };
        k.died().await;
        let snapshot = k.store.snapshot().await;
        let request = event_of(&snapshot, &k.id(), "tool_approval_required");
        assert_eq!(
            snapshot.entries_for(&request),
            0,
            "the seal was not committed"
        );
        assert_eq!(
            snapshot.audit.as_ref().unwrap().outbox,
            vec![request.clone()],
            "the outbox row is still there"
        );
        k.a.restart().await;
        sealed_by_the_drain(&k, &request).await;
        let row = k.request().await;
        let (st, body) = approve(&k.http, &k.a.base(), &k.store.reviewer, &row)
            .await
            .unwrap();
        assert_eq!(st, 200, "{body}");
        let done = k.finish().await;
        assert_eq!(done.entries_for(&request), 1);
    });
}
