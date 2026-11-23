//! Engines with their background loops off (`JAMJET_BACKGROUND=off`), as
//! processes, moved only by `POST /ops/tick` and the advance after each
//! write: two engines on one store, a stop signal during a tick and during
//! a write, an hour with no tick, and nobody ticking at all.
//!
//! The Postgres cases skip without `JAMJET_TEST_POSTGRES_URL`.
#![cfg(unix)]

#[path = "support/engine.rs"]
mod engine;

use engine::{
    approval_node, approve, approve_then_work, bases, claim, client, complete, condition, db_id,
    fail, graph, must, python, register, start_run, tick, tick_ran, wait_for_request, work_only,
    worker, Answers, Backend, Claimed, Engine, TestStore, HANG_AT, LOOPS_OFF,
};
use jamjet_testkit::invariants::{Snapshot, Violation};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the scheduler logs at debug level for each pass the store refused,
/// in a tick's walk and in an advance after a write alike.
const STALE: &str = "scheduler pass was stale";

/// The first HTTP client a test process builds sets up its TLS, which takes
/// many seconds on a loaded machine. Each case builds one before its clock
/// starts, so the cost falls outside every budget.
fn warm_up() {
    let _ = client();
}

/// Run `case` within twice its budget, so a hang fails instead of eating the
/// run, and say how long it took.
async fn within<F: std::future::Future<Output = ()>>(name: &str, budget: Duration, case: F) {
    let start = Instant::now();
    tokio::time::timeout(budget * 2, case)
        .await
        .unwrap_or_else(|_| panic!("{name} ran past twice its {budget:?} budget"));
    eprintln!(
        "{name}: {:.1} s (budget {budget:?})",
        start.elapsed().as_secs_f64()
    );
}

/// The runs of `ids` once every one is finished, read from the store.
async fn finished(store: &TestStore, ids: &[String], within: Duration) -> Snapshot {
    let deadline = Instant::now() + within;
    loop {
        let snapshot = store.snapshot().await;
        let open: Vec<&String> = ids
            .iter()
            .filter(|id| {
                !matches!(
                    snapshot.status_of(id),
                    Some("completed" | "failed" | "cancelled")
                )
            })
            .collect();
        if open.is_empty() {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "{} of {} runs still open after {within:?}, such as {}: {:?}",
            open.len(),
            ids.len(),
            open[0],
            kinds_of(&snapshot, open[0])
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn no_violations(violations: &[Violation]) {
    assert!(
        violations.is_empty(),
        "invariants broken:\n{}",
        violations
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The event kinds of `run`, in order.
fn kinds_of(snapshot: &Snapshot, run: &str) -> Vec<String> {
    snapshot
        .events_of(run)
        .map(|e| e.kind_type().to_string())
        .collect()
}

/// How many events of `run` are of `kind`, for `node` if given.
fn counted(snapshot: &Snapshot, run: &str, kind: &str, node: Option<&str>) -> usize {
    snapshot
        .events_of(run)
        .filter(|e| e.kind_type() == kind && (node.is_none() || e.node() == node))
        .count()
}

/// The fault point a case arms on each backend.
fn point(backend: Backend, sqlite: &'static str, postgres: &'static str) -> &'static str {
    match backend {
        Backend::Sqlite => sqlite,
        Backend::Postgres => postgres,
    }
}

/// Join every future in `futures`, in order.
async fn futures_join<T: Send + 'static>(
    futures: impl Iterator<Item = impl std::future::Future<Output = T> + Send + 'static>,
) -> Vec<T> {
    let handles: Vec<_> = futures.map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(h.await.unwrap());
    }
    out
}

/// A worker over HTTP and a tick every 200 ms, both through `engine`, until
/// the runs of `ids` are finished: the store then.
async fn drive_to_the_end(
    store: &TestStore,
    engine: &Engine,
    ids: &[String],
    within: Duration,
) -> Snapshot {
    let http = client();
    let stop = Arc::new(AtomicBool::new(false));
    let w = worker(
        http.clone(),
        bases(&[engine]),
        store.developer.clone(),
        "worker-end",
        Answers::default(),
        stop.clone(),
    );
    let ticker = {
        let (http, base) = (http.clone(), engine.base());
        let (token, stop) = (store.operator.clone(), stop.clone());
        tokio::spawn(async move {
            while !stop.load(Ordering::SeqCst) {
                let _ = tick(&http, &base, &token).await;
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
    };
    let snapshot = finished(store, ids, within).await;
    stop.store(true, Ordering::SeqCst);
    w.await.unwrap();
    ticker.await.unwrap();
    snapshot
}

// ── Two engines ──────────────────────────────────────────────────────────────

/// What the ticker's calls were answered.
#[derive(Debug, Default)]
struct Ticks {
    ran: usize,
    /// Refused: a tick of another engine held the store's lease.
    busy_elsewhere: usize,
    /// Refused: a tick of the same engine was running.
    busy_here: usize,
    /// Passes the store refused as stale, summed over the ticks' reports.
    stale_passes: u64,
    /// Calls that got no answer, or an answer other than a report or busy.
    failed: Vec<String>,
}

impl Ticks {
    fn busy(&self) -> usize {
        self.busy_elsewhere + self.busy_here
    }
}

/// A tick through `base`, its answer noted in `ticks`.
async fn one_tick(http: reqwest::Client, base: String, token: String, ticks: Arc<Mutex<Ticks>>) {
    let answer = tick(&http, &base, &token).await;
    let mut t = ticks.lock().unwrap();
    match answer {
        Ok((200, report)) if report["ran"] == true => {
            t.ran += 1;
            t.stale_passes += report["runs"]["stale_passes"].as_u64().unwrap_or(0);
        }
        Ok((200, busy)) if busy["reason"] == "busy" => {
            if busy["held_by"] == "this instance" {
                t.busy_here += 1;
            } else {
                t.busy_elsewhere += 1;
            }
        }
        Ok((st, body)) => t.failed.push(format!("{base}: {st} {body}")),
        Err(e) => t.failed.push(format!("{base}: {e}")),
    }
}

/// A scheduler's ticks, until `stop`: every 200 ms a tick through one engine,
/// the two in turn, except that one time in five it ticks both at once and
/// one time in ten it ticks the same engine twice at once. It does not wait
/// for an answer before the next, and waits for every one at the end.
fn ticker(
    engines: [String; 2],
    token: String,
    stop: Arc<AtomicBool>,
    ticks: Arc<Mutex<Ticks>>,
) -> tokio::task::JoinHandle<()> {
    // A tick that walks a few hundred runs can take longer than the
    // harness client's 20 s on a loaded machine.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()
        .unwrap();
    tokio::spawn(async move {
        let mut calls = Vec::new();
        let mut n: usize = 0;
        while !stop.load(Ordering::SeqCst) {
            n += 1;
            let targets = if n.is_multiple_of(5) {
                vec![0, 1]
            } else if n % 10 == 3 {
                vec![n % 2, n % 2]
            } else {
                vec![n % 2]
            };
            for at in targets {
                calls.push(tokio::spawn(one_tick(
                    http.clone(),
                    engines[at].clone(),
                    token.clone(),
                    ticks.clone(),
                )));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        for call in calls {
            call.await.unwrap();
        }
    })
}

/// How long the round of runs may be repeated to see the races open: a
/// refused tick and a stale pass. Whether one round opens them is down to
/// timing; a round that saw neither is followed by another, until both were
/// seen or this has passed since the engines were up.
const RACE_WINDOW: Duration = Duration::from_secs(60);

/// Every run of `ids` completed, with its approval decided once and each of
/// its nodes completed once.
fn each_run_once(snapshot: &Snapshot, ids: &[String]) {
    for id in ids {
        assert_eq!(snapshot.status_of(id), Some("completed"), "run {id}");
        let approvals = counted(snapshot, id, "approval_received", None);
        assert_eq!(approvals, 1, "run {id} was approved {approvals} times");
        for node in ["approve", "work"] {
            let done = counted(snapshot, id, "node_completed", Some(node));
            assert_eq!(done, 1, "run {id} completed {node} {done} times");
        }
    }
}

/// `runs` runs of `approve` then `work` on two engines with the loops off.
/// Runs start on the two in turn; the approval node waits on the queue for
/// a tick to raise its request, the approver decides through the engine
/// that did not start the run, and the workers claim through both engines
/// and complete through the one they did not claim from. The ticker ticks
/// both all along. More rounds of `runs` follow while no tick was refused or
/// no pass was refused as stale (see [`RACE_WINDOW`]).
async fn every_node_once(backend: Backend, case: &str, runs: usize, budget: Duration) {
    warm_up();
    let Some(store) = TestStore::on(backend, case).await else {
        return;
    };
    within(case, budget, async {
        let mut env = LOOPS_OFF.to_vec();
        // Two hundred runs at once keep every connection of a pool of the
        // harness's usual 5 busy, and a call that waits more than the acquire
        // timeout for one is answered 503. The server's own default is 10.
        if backend == Backend::Postgres {
            env.push(("JAMJET_PG_POOL_SIZE", "10"));
        }
        let a = Engine::start(&store, &env).await;
        let b = Engine::start(&store, &env).await;
        let engines = [a.base(), b.base()];
        let http = client();
        let workflow = format!("rd-engines-{}", uuid::Uuid::new_v4().simple());
        register(
            &http,
            &engines[0],
            &store.developer,
            &approve_then_work(&workflow, None),
        )
        .await;

        let stop = Arc::new(AtomicBool::new(false));
        let ticks = Arc::new(Mutex::new(Ticks::default()));
        let ticking = ticker(
            engines.clone(),
            store.operator.clone(),
            stop.clone(),
            ticks.clone(),
        );
        let answers = Answers::default();
        let shared = bases(&[&a, &b]);
        let workers = ["worker-1", "worker-2"].map(|name| {
            worker(
                http.clone(),
                shared.clone(),
                store.developer.clone(),
                name,
                answers.clone(),
                stop.clone(),
            )
        });

        // The engine each run started on, by the run's API id.
        let starter: Arc<Mutex<BTreeMap<String, usize>>> = Arc::default();
        let approver = {
            let http = http.clone();
            let engines = engines.clone();
            let token = store.reviewer.clone();
            let stop = stop.clone();
            let starter = starter.clone();
            tokio::spawn(async move {
                let mut decided: BTreeSet<String> = BTreeSet::new();
                while !stop.load(Ordering::SeqCst) {
                    for base in &engines {
                        for row in engine::pending(&http, base, &token).await {
                            let run = row["execution_id"].as_str().unwrap().to_string();
                            let started_on = starter.lock().unwrap().get(&run).copied();
                            let Some(started_on) = started_on else {
                                continue;
                            };
                            if decided.contains(&run) {
                                continue;
                            }
                            let other = &engines[1 - started_on];
                            match approve(&http, other, &token, &row).await {
                                Ok((200, _)) => {
                                    decided.insert(run);
                                }
                                Ok((st, body)) => panic!("approval of {run} refused: {st} {body}"),
                                Err(e) => panic!("approval of {run}: {e:?}"),
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
        };

        let race_over = Instant::now() + RACE_WINDOW;
        let mut ids: Vec<String> = Vec::new();
        let mut rounds = 0;
        let snapshot = loop {
            rounds += 1;
            for chunk in (0..runs).collect::<Vec<_>>().chunks(20) {
                let starts = chunk.iter().map(|i| {
                    let http = http.clone();
                    let base = engines[i % 2].clone();
                    let token = store.developer.clone();
                    let workflow = workflow.clone();
                    let on = i % 2;
                    async move { (start_run(&http, &base, &token, &workflow).await, on) }
                });
                for (id, on) in futures_join(starts).await {
                    ids.push(db_id(&id));
                    starter.lock().unwrap().insert(id, on);
                }
            }
            let snapshot = finished(&store, &ids, budget).await;
            each_run_once(&snapshot, &ids);
            let stale = a.count(STALE) + b.count(STALE);
            let busy = ticks.lock().unwrap().busy();
            if (stale >= 1 && busy >= 1) || Instant::now() >= race_over {
                break snapshot;
            }
            eprintln!(
                "{case}: after round {rounds} ({} runs) {stale} stale passes, {busy} busy ticks; \
                 another round",
                ids.len()
            );
        };
        stop.store(true, Ordering::SeqCst);
        approver.await.unwrap();
        ticking.await.unwrap();
        let mut completed = 0;
        for w in workers {
            completed += w.await.unwrap();
        }
        assert_eq!(
            completed,
            ids.len(),
            "one completion per run: {}",
            answers.summary()
        );

        no_violations(&jamjet_testkit::invariants::check_snapshot(&snapshot));
        if backend == Backend::Postgres {
            let audit = snapshot.audit.as_ref().unwrap();
            assert!(
                audit.entries.len() >= 2 * ids.len(),
                "each run's request and decision are in the chain: {} entries",
                audit.entries.len()
            );
        }
        let ticks = ticks.lock().unwrap();
        let stale = a.count(STALE) + b.count(STALE);
        eprintln!(
            "{case}: {} runs in {rounds} round(s); {} ticks ran, {} refused while another \
             engine's ran, {} while the same engine's ran; {stale} stale passes ({} on A, {} on B; \
             {} in the ticks' walks)",
            ids.len(),
            ticks.ran,
            ticks.busy_elsewhere,
            ticks.busy_here,
            a.count(STALE),
            b.count(STALE),
            ticks.stale_passes
        );
        assert!(
            ticks.failed.is_empty(),
            "ticks that failed: {:?}",
            ticks.failed
        );
        assert!(ticks.ran >= 1, "no tick ran");
        assert!(
            ticks.busy() >= 1,
            "no tick was refused, so two ticks never met"
        );
        assert!(
            stale >= 1,
            "no pass was refused as stale, so the ticks and the advances never raced"
        );
        a.assert_no_panics();
        b.assert_no_panics();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_request_driven_engines_on_postgres_complete_every_node_once() {
    every_node_once(
        Backend::Postgres,
        "two_request_driven_engines_on_postgres_complete_every_node_once",
        200,
        Duration::from_secs(120),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_request_driven_engines_on_one_sqlite_file() {
    every_node_once(
        Backend::Sqlite,
        "two_request_driven_engines_on_one_sqlite_file",
        50,
        Duration::from_secs(60),
    )
    .await;
}

// ── A stop signal ────────────────────────────────────────────────────────────

/// What the server logs when SIGTERM arrives.
const SIGTERM_RECEIVED: &str = "SIGTERM received";
/// What it logs when the grace ran out with work still in hand.
const GRACE_OVER: &str = "Grace period over with workers still running";

/// An engine with its loops off on a serverless platform (`K_SERVICE`), so
/// its shutdown grace is the platform's 4 s, with `extra` on top.
fn on_a_platform<'a>(backend: Backend, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut env = LOOPS_OFF.to_vec();
    env.push(("K_SERVICE", "svc"));
    if backend == Backend::Sqlite {
        // A SQLite file on a platform is refused unless it is meant.
        env.push(("JAMJET_ALLOW_EPHEMERAL_STORE", "1"));
    }
    env.extend_from_slice(extra);
    env
}

/// SIGTERM to `engine`, and how long it took to exit, within 10 s: the
/// platform's time between SIGTERM and SIGKILL.
async fn stop_within_ten_seconds(engine: &mut Engine) -> Duration {
    let signalled = Instant::now();
    engine.terminate();
    let status = engine
        .exited(Duration::from_secs(10))
        .await
        .unwrap_or_else(|| panic!("still running 10 s after SIGTERM: {}", engine.log()));
    let took = signalled.elapsed();
    assert!(
        status.success(),
        "SIGTERM must end in a clean exit: {status}\n{}",
        engine.log()
    );
    assert!(engine.log().contains(SIGTERM_RECEIVED), "{}", engine.log());
    took
}

/// SIGTERM while a tick's pass is in a store transaction, on a serverless
/// platform. The tick in hand finishes the pass and gives the lease back,
/// within the 4 s grace, and the process exits inside 10 s. After a restart
/// the next tick finishes the run the stopped tick did not reach.
///
/// Two runs wait for the pass that ends them. Engine A's tick hangs 2.5 s
/// inside the first one's pass, and its caller gives up at 0.5 s, as a
/// scheduler's attempt deadline would: from then on only the engine's own
/// count of work in flight keeps the drain waiting for the tick, not a
/// request the HTTP server is still serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigterm_mid_tick_ends_within_ten_seconds() {
    warm_up();
    for backend in [Backend::Sqlite, Backend::Postgres] {
        let case = format!("sigterm_mid_tick_ends_within_ten_seconds ({backend:?})");
        let Some(store) = TestStore::on(backend, &case).await else {
            continue;
        };
        within(&case, Duration::from_secs(30), async {
            let pass = point(
                backend,
                "sqlite.commit_scheduler_pass#8",
                "pg.commit_scheduler_pass#8",
            );
            let hang = format!("{pass}=2500");
            let b = Engine::start(&store, &LOOPS_OFF).await;
            let mut a = Engine::start(&store, &on_a_platform(backend, &[(HANG_AT, &hang)])).await;
            let http = client();
            let workflow = format!("rd-stop-tick-{}", uuid::Uuid::new_v4().simple());
            register(
                &http,
                &b.base(),
                &store.developer,
                &graph(&workflow, "s", vec![condition("s")], &[]),
            )
            .await;
            let runs = [
                db_id(&start_run(&http, &b.base(), &store.developer, &workflow).await),
                db_id(&start_run(&http, &b.base(), &store.developer, &workflow).await),
            ];
            // B's tick runs both `s` steps from the queue; each run then
            // waits for the pass that ends it.
            let report = tick_ran(&http, &b.base(), &store.operator).await;
            assert_eq!(report["engine_steps"]["slow"], 2, "{report}");

            let impatient = reqwest::Client::builder()
                .timeout(Duration::from_millis(500))
                .build()
                .unwrap();
            let gave_up = tick(&impatient, &a.base(), &store.operator).await;
            assert!(
                gave_up.is_err(),
                "the tick answered before its caller gave up: {gave_up:?}"
            );
            let hanging = format!("fault point {pass}: hang");
            assert!(
                a.printed(&hanging, Duration::from_secs(10)).await,
                "the tick's pass never reached the hang: {}",
                a.log()
            );
            let took = stop_within_ten_seconds(&mut a).await;
            assert!(
                !a.log().contains(GRACE_OVER),
                "the tick finished within the grace: {}",
                a.log()
            );

            // The pass in hand was committed: one run ended. The walk stopped
            // there, so the other was not reached.
            let stopped = store.snapshot().await;
            let (ended, waiting): (Vec<&String>, Vec<&String>) = runs
                .iter()
                .partition(|r| stopped.status_of(r) == Some("completed"));
            assert_eq!(
                (ended.len(), waiting.len()),
                (1, 1),
                "{:?} / {:?}",
                kinds_of(&stopped, &runs[0]),
                kinds_of(&stopped, &runs[1])
            );
            // The tick gave its lease back and kept where it stopped.
            let row = store.tick_row().await.expect("the tick took the lease");
            assert_eq!(row.holder, None, "the lease was given back: {row:?}");
            assert_eq!(row.last_complete, Some(false), "{row:?}");
            assert_eq!(row.cursor.as_deref(), Some(ended[0].as_str()), "{row:?}");

            a.restart().await;
            let report = tick_ran(&http, &a.base(), &store.operator).await;
            assert_eq!(report["runs"]["advanced"], 1, "{report}");
            let after = store.snapshot().await;
            assert_eq!(
                kinds_of(&after, ended[0]),
                kinds_of(&stopped, ended[0]),
                "nothing more for the run the stopped tick ended"
            );
            for run in &runs {
                assert_eq!(after.status_of(run), Some("completed"), "{run}");
                assert_eq!(counted(&after, run, "workflow_completed", None), 1);
            }
            no_violations(&jamjet_testkit::invariants::check_snapshot(&after));
            eprintln!("{case}: exited {:.1} s after SIGTERM", took.as_secs_f64());
            a.assert_no_panics();
            b.assert_no_panics();
        })
        .await;
    }
}

/// SIGTERM while the engine step an inline advance runs is inside its store
/// transaction, and stays there: the drain waits out the 4 s grace, the
/// process exits inside 10 s with the transaction open, and the store rolls
/// it back. Nothing of the step is written, the decision before it stands,
/// and after a restart the store holds every rule.
///
/// The dead step leaves its item leased and its effect's idempotency key
/// reserved, as any step that died mid-run would, so that no retry runs it
/// again while it might still be running. Once both have run
/// out (moved back with SQL here), the run finishes with the step run once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigterm_mid_transaction_leaves_no_partial_write() {
    warm_up();
    for backend in [Backend::Sqlite, Backend::Postgres] {
        let case = format!("sigterm_mid_transaction_leaves_no_partial_write ({backend:?})");
        let Some(store) = TestStore::on(backend, &case).await else {
            continue;
        };
        within(&case, Duration::from_secs(30), async {
            // The engine's own workers write through the unscoped store; on
            // SQLite a worker's completion over HTTP has a body of its own.
            let turn = point(backend, "sqlite.commit_turn#11", "pg.commit_turn#10");
            let hang = format!("{turn}=60000");
            let b = Engine::start(&store, &LOOPS_OFF).await;
            let mut a = Engine::start(&store, &on_a_platform(backend, &[(HANG_AT, &hang)])).await;
            let http = client();
            let workflow = format!("rd-stop-write-{}", uuid::Uuid::new_v4().simple());
            register(
                &http,
                &b.base(),
                &store.developer,
                &approve_then_work(&workflow, None),
            )
            .await;
            let api_run = start_run(&http, &b.base(), &store.developer, &workflow).await;
            let run = db_id(&api_run);
            // B's tick raises the request from the queue.
            let report = tick_ran(&http, &b.base(), &store.operator).await;
            assert_eq!(report["engine_steps"]["slow"], 1, "{report}");
            let row = wait_for_request(
                &http,
                &b.base(),
                &store.reviewer,
                &api_run,
                Duration::from_secs(10),
            )
            .await;

            // The decision through A: its advance runs `approve` again, as
            // the engine, and hangs in the write that completes it.
            let deciding = {
                let (http, base, token) = (http.clone(), a.base(), store.reviewer.clone());
                tokio::spawn(async move { approve(&http, &base, &token, &row).await })
            };
            let hanging = format!("fault point {turn}: hang");
            assert!(
                a.printed(&hanging, Duration::from_secs(10)).await,
                "the engine step never reached the hang: {}",
                a.log()
            );
            let took = stop_within_ten_seconds(&mut a).await;
            assert!(
                a.log().contains(GRACE_OVER),
                "the drain gave up on the step when the grace ran out: {}",
                a.log()
            );
            let decided = deciding.await.unwrap();
            assert!(
                decided.is_err(),
                "the decision whose advance was cut off got no answer: {decided:?}"
            );

            let stopped = store.snapshot().await;
            assert_eq!(counted(&stopped, &run, "approval_received", None), 1);
            assert_eq!(
                counted(&stopped, &run, "node_completed", Some("approve")),
                0,
                "nothing of the step was written: {:?}",
                kinds_of(&stopped, &run)
            );
            assert!(
                stopped
                    .live_items
                    .contains(&(run.clone(), "approve".to_string())),
                "the step's item is still leased"
            );

            a.restart().await;
            no_violations(&store.check().await);
            assert_eq!(
                store.push_back_leases(Duration::from_secs(3600)).await,
                1,
                "the dead step's lease"
            );
            assert_eq!(
                store
                    .push_back_reservations(Duration::from_secs(3600))
                    .await,
                1,
                "the dead step's reservation of its idempotency key"
            );
            let done = drive_to_the_end(
                &store,
                &a,
                std::slice::from_ref(&run),
                Duration::from_secs(20),
            )
            .await;
            assert_eq!(done.status_of(&run), Some("completed"));
            assert_eq!(counted(&done, &run, "approval_received", None), 1);
            for node in ["approve", "work"] {
                assert_eq!(
                    counted(&done, &run, "node_completed", Some(node)),
                    1,
                    "{node}"
                );
            }
            no_violations(&jamjet_testkit::invariants::check_snapshot(&done));
            eprintln!("{case}: exited {:.1} s after SIGTERM", took.as_secs_f64());
            a.assert_no_panics();
            b.assert_no_panics();
        })
        .await;
    }
}

// ── An hour without a tick ───────────────────────────────────────────────────

/// Claim the next worker's item through `base`, if one is claimable now.
async fn claim_now(http: &reqwest::Client, base: &str, token: &str, lease: u64) -> Option<Claimed> {
    let (st, item) = claim(http, base, token, "worker-hour", lease)
        .await
        .unwrap();
    assert_eq!(st, 200);
    item
}

/// Nobody ticks for an hour, then one tick catches up, once. Three runs
/// wait on an approval that expires in 30 minutes, three on a worker's
/// lease, and three on a retry after a failure. SQLite: the engine is
/// restarted with its clock an hour ahead, which moves what the store reads
/// on that clock (the approvals' deadlines and the tick lease); a lease and a
/// retry are read on the machine's clock there, so those are moved back an
/// hour with SQL. Postgres decides all of them on the database's clock: the
/// deadlines, leases and retries are moved back an hour with SQL.
///
/// One tick then expires each request once and runs its fallback once, and
/// takes back each lapsed lease once; the parked retries are claimable; and
/// a second tick finds nothing to do.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_hour_without_a_tick_catches_up_once() {
    warm_up();
    const HOUR: Duration = Duration::from_secs(3600);
    for backend in [Backend::Sqlite, Backend::Postgres] {
        let case = format!("an_hour_without_a_tick_catches_up_once ({backend:?})");
        let Some(store) = TestStore::on(backend, &case).await else {
            continue;
        };
        within(&case, Duration::from_secs(30), async {
            let mut a = Engine::start(&store, &LOOPS_OFF).await;
            let http = client();
            let suffix = uuid::Uuid::new_v4().simple().to_string();
            // `s`, then `review`, which falls back to `hold` after 30 minutes.
            let deadline_wf = format!("rd-hour-deadline-{suffix}");
            register(
                &http,
                &a.base(),
                &store.developer,
                &graph(
                    &deadline_wf,
                    "s",
                    vec![
                        python("s"),
                        approval_node("review", Some(1800), Some("hold")),
                        python("publish"),
                        condition("hold"),
                    ],
                    &[("s", "review"), ("review", "publish")],
                ),
            )
            .await;
            let lease_wf = format!("rd-hour-lease-{suffix}");
            let retry_wf = format!("rd-hour-retry-{suffix}");
            for wf in [&lease_wf, &retry_wf] {
                register(&http, &a.base(), &store.developer, &work_only(wf)).await;
            }
            let start = |wf: String| {
                let (http, base, token) = (http.clone(), a.base(), store.developer.clone());
                async move { db_id(&start_run(&http, &base, &token, &wf).await) }
            };
            let (dev, base) = (store.developer.clone(), a.base());

            // Each group's runs start before any of their items is claimed,
            // so a claim only ever finds fresh items: no retry is due yet.
            let mut waiting = Vec::new();
            for _ in 0..3 {
                waiting.push(start(deadline_wf.clone()).await);
            }
            // Three approvals: `s` completes, and the advance after it raises
            // `review`'s request.
            for _ in 0..3 {
                let item = claim_now(&http, &base, &dev, 30).await.expect("`s`");
                let (st, body) = complete(&http, &base, &dev, &item).await.unwrap();
                assert_eq!(st, 200, "{body}");
            }
            // Three leases held by a worker that never comes back.
            let mut leased = Vec::new();
            for _ in 0..3 {
                leased.push(start(lease_wf.clone()).await);
            }
            for _ in 0..3 {
                claim_now(&http, &base, &dev, 60).await.expect("a lease");
            }
            // Three failures, each with a retry parked behind its backoff.
            let mut retried = Vec::new();
            for _ in 0..3 {
                retried.push(start(retry_wf.clone()).await);
            }
            for _ in 0..3 {
                let item = claim_now(&http, &base, &dev, 60).await.expect("an attempt");
                let (st, body) = fail(&http, &base, &dev, &item).await.unwrap();
                assert_eq!(st, 200, "{body}");
            }
            let setup = store.snapshot().await;
            for run in &waiting {
                assert_eq!(
                    counted(&setup, run, "tool_approval_required", None),
                    1,
                    "{:?}",
                    kinds_of(&setup, run)
                );
            }
            for run in &retried {
                assert_eq!(
                    counted(&setup, run, "retry_scheduled", None),
                    1,
                    "{:?}",
                    kinds_of(&setup, run)
                );
            }
            // A tick now finds every deadline, lease and retry still ahead.
            let now = tick_ran(&http, &base, &store.operator).await;
            assert_eq!(now["deadlines"]["expired"], 0, "{now}");
            assert_eq!(now["reclaimed"]["retried"], 0, "{now}");

            // The hour.
            assert_eq!(store.push_back_leases(HOUR).await, 3, "the leases");
            assert_eq!(store.push_back_retries(HOUR).await, 3, "the parked retries");
            match backend {
                Backend::Sqlite => {
                    a.restart_with(&[("JAMJET_TEST_CLOCK_SKEW_SECS", "3600")])
                        .await;
                }
                Backend::Postgres => {
                    assert_eq!(store.push_back_deadlines(HOUR).await, 3, "the requests");
                }
            }
            let base = a.base();
            let before = store.snapshot().await;

            let report = tick_ran(&http, &base, &store.operator).await;
            assert_eq!(report["deadlines"]["expired"], 3, "{report}");
            assert_eq!(report["reclaimed"]["retried"], 3, "{report}");
            assert_eq!(report["complete"], true, "{report}");
            let after = store.snapshot().await;
            for run in &waiting {
                assert_eq!(after.status_of(run), Some("completed"), "{run}");
                assert_eq!(counted(&after, run, "approval_expired", Some("review")), 1);
                assert_eq!(counted(&after, run, "node_completed", Some("hold")), 1);
                assert_eq!(counted(&after, run, "node_scheduled", Some("publish")), 0);
            }
            for run in &leased {
                assert_eq!(
                    counted(&after, run, "retry_scheduled", None),
                    1,
                    "taken back once: {:?}",
                    kinds_of(&after, run)
                );
            }
            for run in &retried {
                assert_eq!(
                    kinds_of(&after, run),
                    kinds_of(&before, run),
                    "a parked retry is not the tick's to touch"
                );
            }

            // The parked retries are claimable now. The leases taken back
            // wait for their own backoff from the tick, so a claim may get
            // one of them too once it has passed.
            let mut claimed: Vec<Claimed> = Vec::new();
            while let Some(item) = claim_now(&http, &base, &dev, 30).await {
                claimed.push(item);
            }
            let claimed_runs: Vec<String> = claimed.iter().map(|c| db_id(&c.run)).collect();
            for run in &retried {
                assert!(
                    claimed_runs.contains(run),
                    "the parked retry of {run} is claimable: {claimed_runs:?}"
                );
            }
            let unique: BTreeSet<&String> = claimed_runs.iter().collect();
            assert_eq!(unique.len(), claimed_runs.len(), "{claimed_runs:?}");

            let again = tick_ran(&http, &base, &store.operator).await;
            assert_eq!(again["deadlines"]["expired"], 0, "{again}");
            assert_eq!(again["reclaimed"]["retried"], 0, "{again}");
            let still = store.snapshot().await;
            for run in &waiting {
                assert_eq!(kinds_of(&still, run), kinds_of(&after, run), "{run}");
            }
            for run in &leased {
                assert_eq!(counted(&still, run, "retry_scheduled", None), 1, "{run}");
            }

            for item in &claimed {
                let (st, body) = complete(&http, &base, &dev, item).await.unwrap();
                assert_eq!(st, 200, "{body}");
            }
            let all: Vec<String> = waiting
                .iter()
                .chain(&leased)
                .chain(&retried)
                .cloned()
                .collect();
            let done = drive_to_the_end(&store, &a, &all, Duration::from_secs(20)).await;
            for run in &all {
                assert_eq!(done.status_of(run), Some("completed"), "{run}");
            }
            no_violations(&jamjet_testkit::invariants::check_snapshot(&done));
            a.assert_no_panics();
        })
        .await;
    }
}

// ── Nobody ticking ───────────────────────────────────────────────────────────

/// The line an engine with its loops off logs while nobody ticks it.
const NO_TICK: &str = "no tick for ";

/// `/health`'s answer.
async fn health(http: &reqwest::Client, base: &str) -> Value {
    let (st, body) = must(http, base, "GET", "/health", "", None).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["status"], "ok", "{body}");
    body
}

/// An engine with its loops off and a tick expected every second says so
/// when nobody ticks it: after three intervals `/health` reports the tick
/// overdue, and requests write one error line about it, not one each. A tick
/// clears it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loops_off_and_nobody_ticking_is_visible() {
    warm_up();
    for backend in [Backend::Sqlite, Backend::Postgres] {
        let case = format!("loops_off_and_nobody_ticking_is_visible ({backend:?})");
        let Some(store) = TestStore::on(backend, &case).await else {
            continue;
        };
        within(&case, Duration::from_secs(10), async {
            let mut env = LOOPS_OFF.to_vec();
            env.push(("JAMJET_TICK_INTERVAL_SECS", "1"));
            let http = client();
            let a = Engine::start(&store, &env).await;
            let base = a.base();
            let fresh = health(&http, &base).await;
            assert_eq!(fresh["background"], "off", "{fresh}");

            tokio::time::sleep(Duration::from_millis(3500)).await;
            for _ in 0..5 {
                let late = health(&http, &base).await;
                assert_eq!(late["tick_overdue"], true, "{late}");
            }
            let (st, _) = must(&http, &base, "GET", "/executions", &store.developer, None).await;
            assert_eq!(st, 200);
            let (st, _) = must(&http, &base, "GET", "/executions", "made-up", None).await;
            assert_eq!(st, 401);
            assert!(
                a.printed(NO_TICK, Duration::from_secs(2)).await,
                "no line: {}",
                a.log()
            );
            assert_eq!(
                a.count(NO_TICK),
                1,
                "one line, not one per request: {}",
                a.log()
            );

            tick_ran(&http, &base, &store.operator).await;
            let ticked = health(&http, &base).await;
            assert_eq!(ticked["tick_overdue"], false, "{ticked}");
            assert_eq!(a.count(NO_TICK), 1, "{}", a.log());
            a.assert_no_panics();
        })
        .await;
    }
}
