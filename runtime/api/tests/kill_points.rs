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
//! With the background loops off (`JAMJET_BACKGROUND=off`) the engine moves
//! runs only inside a tick and right after a write, so those cases kill it
//! there, at seams between its parts, the same on both stores:
//!
//! | Case | Point |
//! |---|---|
//! | `tick.after_begin` | `#1`: the tick holds the store's lease, nothing else done |
//! | `tick.mid_walk` | `#1`: after the walk advanced its first run |
//! | `inline.after_write` | `#1`: a worker's completion committed, its advance not begun |
//! | `inline.mid_engine_step` | `#1`: the advance claimed an engine step's item, not run it |
//!
//! There A is the engine killed, and a second engine B on the same store,
//! nothing armed, sets the run up. A dead tick leaves the store's lease to
//! its holder, and a dead engine step its item's lease, until they run out
//! (a tick's lease outlasts its budget by two minutes): those cases end the
//! lease with SQL, as the time passing would, before the tick after the
//! restart.
//!
//! Four cases run at a time, each on its own store. The Postgres cases skip
//! without `JAMJET_TEST_POSTGRES_URL`.
#![cfg(unix)]

#[path = "support/engine.rs"]
mod engine;

use engine::{
    approval_node, approve, approve_then_work, bases, call, claim, client, complete, condition,
    db_id, fail, graph, python, register, start_run, tick, tick_ran, wait_for_request, work_only,
    worker, Answers, Backend, Claimed, Engine, TestStore, CRASH_AT, LOOPS_OFF,
};
use jamjet_testkit::invariants::{Snapshot, Violation};
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
    case_within(name, CASE, body);
}

/// [`case`], within `limit`. The first HTTP client a test process builds
/// sets up its TLS, which takes many seconds on a loaded machine: one is
/// built before the case's clock starts.
fn case_within<F: std::future::Future<Output = ()>>(name: &str, limit: Duration, body: F) {
    let _ = client();
    let _slot = Slot::take();
    let start = Instant::now();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(limit, body)
                .await
                .unwrap_or_else(|_| panic!("{name} ran past {limit:?}"))
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

// ── Request-driven mode ──────────────────────────────────────────────────────

/// One request-driven case is given this long.
const DRIVEN_CASE: Duration = Duration::from_secs(60);

/// The tick's lease in the store, which a tick renews while it runs.
const TICK_LEASE: Duration = Duration::from_secs(30);

/// Engine A with the loops off and the fault point armed, and engine B with
/// the loops off and nothing armed, on one store of their own. B makes the
/// writes and ticks that bring a run to the point; then a call to A kills it.
struct Driven {
    store: TestStore,
    a: Engine,
    b: Engine,
    http: reqwest::Client,
    point: &'static str,
    workflow: String,
}

impl Driven {
    /// The store and both engines, with `ir` registered. `None` when the
    /// backend is not configured.
    async fn begin(backend: Backend, name: &str, point: &'static str, ir: Value) -> Option<Self> {
        let store = TestStore::on(backend, name).await?;
        let mut armed = LOOPS_OFF.to_vec();
        armed.push((CRASH_AT, point));
        let a = Engine::start(&store, &armed).await;
        let b = Engine::start(&store, &LOOPS_OFF).await;
        let http = client();
        register(&http, &b.base(), &store.developer, &ir).await;
        let workflow = ir["workflow_id"].as_str().unwrap().to_string();
        Some(Self {
            store,
            a,
            b,
            http,
            point,
            workflow,
        })
    }

    /// Start a run through B; its id as the store writes it.
    async fn start(&self) -> String {
        let run = start_run(
            &self.http,
            &self.b.base(),
            &self.store.developer,
            &self.workflow,
        )
        .await;
        db_id(&run)
    }

    /// Claim the next worker's item through B.
    async fn claim(&self) -> Claimed {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let (st, item) = claim(
                &self.http,
                &self.b.base(),
                &self.store.developer,
                "worker-1",
                30,
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

    async fn died(&mut self) {
        self.a.died_at(self.point, TO_DIE).await;
    }

    /// Tick through A until a tick runs: once the lease of the tick that
    /// died, which nothing renews, has run out. A tick renews its lease
    /// while it runs, so the lease is short, and this is within one lease of
    /// `sent`, the moment the call that killed it was sent.
    async fn tick_once_the_dead_lease_lapses(&self, sent: Instant) -> Value {
        let within = TICK_LEASE + Duration::from_secs(5);
        loop {
            let (st, body) = tick(&self.http, &self.a.base(), &self.store.operator)
                .await
                .unwrap();
            assert_eq!(st, 200, "{body}");
            if body["ran"] == true {
                let waited = sent.elapsed();
                eprintln!(
                    "{}: a tick ran {:.1} s after the call that killed the last one",
                    self.point,
                    waited.as_secs_f64()
                );
                assert!(
                    waited <= within,
                    "the dead tick's lease held for {waited:?}"
                );
                return body;
            }
            assert_eq!(body["reason"], "busy", "{body}");
            assert!(
                sent.elapsed() < within,
                "still busy {:?} after the kill: {body}",
                sent.elapsed()
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Drive `runs` to their end through A, which must be running again: a
    /// worker over HTTP for the workers' steps, and a tick every 200 ms for
    /// the engine's. The store as they all completed, with every rule held.
    async fn finish(&self, runs: &[String]) -> Snapshot {
        let stop = Arc::new(AtomicBool::new(false));
        let w = worker(
            self.http.clone(),
            bases(&[&self.a]),
            self.store.developer.clone(),
            "worker-2",
            Answers::default(),
            stop.clone(),
        );
        let ticker = {
            let (http, base) = (self.http.clone(), self.a.base());
            let (token, stop) = (self.store.operator.clone(), stop.clone());
            tokio::spawn(async move {
                while !stop.load(Ordering::SeqCst) {
                    let _ = tick(&http, &base, &token).await;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            })
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        let snapshot = loop {
            let snapshot = self.store.snapshot().await;
            let open: Vec<&String> = runs
                .iter()
                .filter(|r| snapshot.status_of(r) != Some("completed"))
                .collect();
            if open.is_empty() {
                break snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "run {} did not finish after the restart: {:?}\n{}",
                open[0],
                kinds_of(&snapshot, open[0]),
                self.a.log()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        stop.store(true, Ordering::SeqCst);
        w.await.unwrap();
        ticker.await.unwrap();
        holds(
            &jamjet_testkit::invariants::check_snapshot(&snapshot),
            self.point,
        );
        self.a.assert_no_panics();
        self.b.assert_no_panics();
        snapshot
    }
}

/// The event kinds of `run`, in order.
fn kinds_of(snapshot: &Snapshot, run: &str) -> Vec<String> {
    snapshot
        .events_of(run)
        .map(|e| e.kind_type().to_string())
        .collect()
}

/// How many events of `run` are of `kind`, for `node`.
fn of_node(snapshot: &Snapshot, run: &str, kind: &str, node: &str) -> usize {
    snapshot
        .events_of(run)
        .filter(|e| e.kind_type() == kind && e.node() == Some(node))
        .count()
}

/// The nodes `run` dispatched, in order.
fn scheduled(snapshot: &Snapshot, run: &str) -> Vec<String> {
    snapshot
        .events_of(run)
        .filter(|e| e.kind_type() == "node_scheduled")
        .filter_map(|e| e.node().map(str::to_string))
        .collect()
}

fn holds(violations: &[Violation], point: &str) {
    assert!(
        violations.is_empty(),
        "invariants broken after a kill at {point}:\n{}",
        violations
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// `s` (a `condition` the engine runs), then `work` (a worker's step).
fn condition_then_work() -> Value {
    graph(
        &workflow_id("rd-begin"),
        "s",
        vec![condition("s"), python("work")],
        &[("s", "work")],
    )
}

/// `s` (a worker's step), `c` (a `condition` the engine runs), then `p`
/// (a worker's step).
fn work_condition_work() -> Value {
    graph(
        &workflow_id("rd-inline"),
        "s",
        vec![python("s"), condition("c"), python("p")],
        &[("s", "c"), ("c", "p")],
    )
}

/// A tick dies right after it took the store's tick lease: nothing else of
/// it ran. The lease is still its holder's, so a tick after the restart is
/// told the store is busy, naming the dead tick. Nothing renews the lease,
/// so it runs out within one lease, 30 s, whatever the budget; then one
/// tick runs the step the dead one did not reach, and the run completes.
async fn tick_after_begin(backend: Backend, name: &str) {
    let point = "tick.after_begin#1";
    let Some(mut d) = Driven::begin(backend, name, point, condition_then_work()).await else {
        return;
    };
    // `s` starts the run, so its item waits on the queue for a tick.
    let run = d.start().await;
    let sent = Instant::now();
    let lost = tick(&d.http, &d.a.base(), &d.store.operator).await;
    assert!(
        lost.is_err(),
        "the engine answered a tick it died in: {lost:?}"
    );
    d.died().await;
    let row = d.store.tick_row().await.expect("the tick took the lease");
    let holder = row.holder.clone().expect("the lease is held");
    let left = row.lease_left.expect("the lease has an end");
    // One lease from the tick's start, a moment ago, and not the default
    // 45 s budget or more.
    assert!(
        left > chrono::Duration::zero() && left <= chrono::Duration::from_std(TICK_LEASE).unwrap(),
        "the lease is 30 s: {row:?}"
    );
    let before = d.store.snapshot().await;
    assert_eq!(
        of_node(&before, &run, "node_completed", "s"),
        0,
        "nothing of the tick ran: {:?}",
        kinds_of(&before, &run)
    );

    d.a.restart().await;
    let (st, busy) = tick(&d.http, &d.a.base(), &d.store.operator).await.unwrap();
    assert_eq!(st, 200, "{busy}");
    assert_eq!(busy["ran"], false, "the dead tick's lease is held: {busy}");
    assert_eq!(busy["held_by"], holder.as_str(), "{busy}");
    let report = d.tick_once_the_dead_lease_lapses(sent).await;
    assert_eq!(
        report["engine_steps"]["slow"], 1,
        "the tick ran `s`: {report}"
    );
    holds(&d.store.check().await, point);
    let done = d.finish(std::slice::from_ref(&run)).await;
    for node in ["s", "work"] {
        assert_eq!(of_node(&done, &run, "node_completed", node), 1, "{node}");
    }
}

#[test]
fn tick_after_begin_on_sqlite() {
    let name = "tick_after_begin_on_sqlite";
    case_within(name, DRIVEN_CASE, async {
        tick_after_begin(Backend::Sqlite, name).await
    });
}

#[test]
fn tick_after_begin_on_postgres() {
    let name = "tick_after_begin_on_postgres";
    case_within(name, DRIVEN_CASE, async {
        tick_after_begin(Backend::Postgres, name).await
    });
}

/// A tick dies after its walk advanced the first of two runs whose
/// approvals ran out undecided: that run's expiry, fallback and end are
/// written, and the other run was not reached. After the restart, once the
/// dead tick's lease has run out, within one lease, one tick expires the
/// other run's request and runs its fallback, and writes nothing again for
/// the first.
async fn tick_mid_walk(backend: Backend, name: &str) {
    let point = "tick.mid_walk#1";
    // `review` waits a second for a decision, then falls back to `hold`.
    let ir = graph(
        &workflow_id("rd-walk"),
        "review",
        vec![
            approval_node("review", Some(1), Some("hold")),
            python("publish"),
            condition("hold"),
        ],
        &[("review", "publish")],
    );
    let Some(mut d) = Driven::begin(backend, name, point, ir).await else {
        return;
    };
    let runs = [d.start().await, d.start().await];
    // B's tick raises both requests from the queue; nobody decides them.
    let raised = tick_ran(&d.http, &d.b.base(), &d.store.operator).await;
    assert_eq!(raised["engine_steps"]["slow"], 2, "{raised}");
    // Past both deadlines: each was a second from its request, which came
    // before the tick answered.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let sent = Instant::now();
    let lost = tick(&d.http, &d.a.base(), &d.store.operator).await;
    assert!(
        lost.is_err(),
        "the engine answered a tick it died in: {lost:?}"
    );
    d.died().await;
    let before = d.store.snapshot().await;
    let (reached, missed): (Vec<&String>, Vec<&String>) = runs
        .iter()
        .partition(|r| of_node(&before, r, "approval_expired", "review") == 1);
    assert_eq!(
        (reached.len(), missed.len()),
        (1, 1),
        "the walk advanced one run before it died: {:?} / {:?}",
        kinds_of(&before, &runs[0]),
        kinds_of(&before, &runs[1])
    );
    let (reached, missed) = (reached[0].clone(), missed[0].clone());
    assert_eq!(
        before.status_of(&reached),
        Some("completed"),
        "its fallback ran and it ended: {:?}",
        kinds_of(&before, &reached)
    );
    assert_eq!(
        of_node(&before, &missed, "approval_expired", "review"),
        0,
        "{:?}",
        kinds_of(&before, &missed)
    );
    assert!(d.store.tick_row().await.unwrap().holder.is_some());

    d.a.restart().await;
    let report = d.tick_once_the_dead_lease_lapses(sent).await;
    assert_eq!(report["deadlines"]["expired"], 1, "{report}");
    let after = d.store.snapshot().await;
    assert_eq!(
        kinds_of(&after, &reached),
        kinds_of(&before, &reached),
        "nothing more was written for the run the dead tick finished"
    );
    for run in &runs {
        assert_eq!(after.status_of(run), Some("completed"), "{run}");
        assert_eq!(of_node(&after, run, "approval_expired", "review"), 1);
        assert_eq!(of_node(&after, run, "node_completed", "hold"), 1);
        assert_eq!(of_node(&after, run, "node_scheduled", "publish"), 0);
    }
    holds(&jamjet_testkit::invariants::check_snapshot(&after), point);
    d.a.assert_no_panics();
    d.b.assert_no_panics();
}

#[test]
fn tick_mid_walk_on_sqlite() {
    let name = "tick_mid_walk_on_sqlite";
    case_within(name, DRIVEN_CASE, async {
        tick_mid_walk(Backend::Sqlite, name).await
    });
}

#[test]
fn tick_mid_walk_on_postgres() {
    let name = "tick_mid_walk_on_postgres";
    case_within(name, DRIVEN_CASE, async {
        tick_mid_walk(Backend::Postgres, name).await
    });
}

/// The engine dies after a worker's completion committed and before the
/// advance that follows it: the completion is on record, unanswered, and the
/// run did not move on. One tick after the restart runs the `condition`
/// that advance would have, and dispatches the next worker's step.
async fn inline_after_write(backend: Backend, name: &str) {
    let point = "inline.after_write#1";
    let Some(mut d) = Driven::begin(backend, name, point, work_condition_work()).await else {
        return;
    };
    let run = d.start().await;
    let item = d.claim().await;
    let lost = complete(&d.http, &d.a.base(), &d.store.developer, &item).await;
    assert!(
        lost.is_err(),
        "the engine answered a completion it died after: {lost:?}"
    );
    d.died().await;
    let before = d.store.snapshot().await;
    assert_eq!(of_node(&before, &run, "node_completed", "s"), 1);
    assert_eq!(
        scheduled(&before, &run),
        ["s"],
        "nothing moved on: {:?}",
        kinds_of(&before, &run)
    );

    d.a.restart().await;
    let report = tick_ran(&d.http, &d.a.base(), &d.store.operator).await;
    assert_eq!(
        report["engine_steps"]["quick"], 1,
        "the tick ran `c`: {report}"
    );
    let after = d.store.snapshot().await;
    assert_eq!(scheduled(&after, &run), ["s", "c", "p"]);
    assert_eq!(of_node(&after, &run, "node_completed", "c"), 1);
    holds(&jamjet_testkit::invariants::check_snapshot(&after), point);
    let done = d.finish(std::slice::from_ref(&run)).await;
    for node in ["s", "c", "p"] {
        assert_eq!(of_node(&done, &run, "node_completed", node), 1, "{node}");
    }
}

#[test]
fn inline_after_write_on_sqlite() {
    let name = "inline_after_write_on_sqlite";
    case_within(name, DRIVEN_CASE, async {
        inline_after_write(Backend::Sqlite, name).await
    });
}

#[test]
fn inline_after_write_on_postgres() {
    let name = "inline_after_write_on_postgres";
    case_within(name, DRIVEN_CASE, async {
        inline_after_write(Backend::Postgres, name).await
    });
}

/// The engine dies inside the engine step its advance runs after a worker's
/// completion: it claimed the `condition`'s item and had not run it. The item
/// is leased to the dead engine and nothing of the step is on record. Once
/// that lease has run out, one tick takes the item back for another attempt,
/// and the step runs once.
async fn inline_mid_engine_step(backend: Backend, name: &str) {
    let point = "inline.mid_engine_step#1";
    let Some(mut d) = Driven::begin(backend, name, point, work_condition_work()).await else {
        return;
    };
    let run = d.start().await;
    let item = d.claim().await;
    let lost = complete(&d.http, &d.a.base(), &d.store.developer, &item).await;
    assert!(
        lost.is_err(),
        "the engine answered a completion whose advance it died in: {lost:?}"
    );
    d.died().await;
    let before = d.store.snapshot().await;
    assert_eq!(scheduled(&before, &run), ["s", "c"]);
    assert_eq!(of_node(&before, &run, "node_completed", "c"), 0);
    assert!(
        before.live_items.contains(&(run.clone(), "c".to_string())),
        "the step's item is still leased"
    );

    d.a.restart().await;
    assert_eq!(
        d.store.push_back_leases(Duration::from_secs(3600)).await,
        1,
        "the dead step's lease"
    );
    let report = tick_ran(&d.http, &d.a.base(), &d.store.operator).await;
    assert_eq!(report["reclaimed"]["retried"], 1, "{report}");
    holds(&d.store.check().await, point);
    let done = d.finish(std::slice::from_ref(&run)).await;
    let kinds = kinds_of(&done, &run);
    assert_eq!(count(&kinds, "retry_scheduled"), 1, "{kinds:?}");
    for node in ["s", "c", "p"] {
        assert_eq!(of_node(&done, &run, "node_completed", node), 1, "{node}");
    }
}

#[test]
fn inline_mid_engine_step_on_sqlite() {
    let name = "inline_mid_engine_step_on_sqlite";
    case_within(name, DRIVEN_CASE, async {
        inline_mid_engine_step(Backend::Sqlite, name).await
    });
}

#[test]
fn inline_mid_engine_step_on_postgres() {
    let name = "inline_mid_engine_step_on_postgres";
    case_within(name, DRIVEN_CASE, async {
        inline_mid_engine_step(Backend::Postgres, name).await
    });
}
