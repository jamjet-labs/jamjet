//! Two engine processes on one store, each with its scheduler and embedded
//! workers, finish every run with every node completed once.
//!
//! A run is `approve` (a `human_approval` node the engines run) then `work`
//! (a `python_fn` node two test workers claim over HTTP), then the end. Runs
//! start on the two engines in turn, each run's approval goes to the other
//! engine, and the workers claim through both and complete through the
//! engine they did not claim from. At the end the invariants hold
//! ([`jamjet_testkit::invariants`]), and the two schedulers' logs show at
//! least one pass the store refused as stale, so the race between them was
//! run. Whether a round of runs opens that race is down to timing, so the
//! cases add rounds until it has (see `RACE_WINDOW`).
//!
//! The Postgres cases skip without `JAMJET_TEST_POSTGRES_URL`.
#![cfg(unix)]

#[path = "support/engine.rs"]
mod engine;

use engine::{
    approve, bases, claim, client, complete, db_id, must, register, start_run, work_only, worker,
    Answers, Backend, Engine, TestStore,
};
use jamjet_testkit::invariants::{Snapshot, Violation};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the scheduler logs at debug level for each pass the store refused.
const STALE: &str = "scheduler pass was stale";

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
            snapshot
                .events_of(open[0])
                .map(|e| e.kind_type())
                .collect::<Vec<_>>()
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

/// How long the two schedulers get to race. A pass is refused as stale when
/// the run's log moved between the scheduler's read of it and its commit,
/// under another scheduler's pass or a worker's or the approver's call. The
/// two schedulers tick every 500 ms, a few tens of milliseconds apart, so on
/// a quick machine the second can find each run already done, and a round
/// logs none, or three or four. A round with none is followed by another of
/// `runs` more runs, until one logs a stale pass or this has passed since
/// the engines were up.
const RACE_WINDOW: Duration = Duration::from_secs(60);

/// Every run of `ids` completed, with its approval decided once and each of
/// its nodes completed once.
fn each_run_once(snapshot: &Snapshot, ids: &[String]) {
    for id in ids {
        assert_eq!(snapshot.status_of(id), Some("completed"), "run {id}");
        let approvals = snapshot
            .events_of(id)
            .filter(|e| e.kind_type() == "approval_received")
            .count();
        assert_eq!(approvals, 1, "run {id} was approved {approvals} times");
        for node in ["approve", "work"] {
            let done = snapshot
                .events_of(id)
                .filter(|e| e.kind_type() == "node_completed" && e.node() == Some(node))
                .count();
            assert_eq!(done, 1, "run {id} completed {node} {done} times");
        }
    }
}

/// `runs` runs on two engines, as the module docs say, and more in further
/// rounds of `runs` while no scheduler has logged a stale pass (see
/// [`RACE_WINDOW`]). Each engine also answers the other's approvals and
/// completions.
async fn every_node_once(backend: Backend, case: &str, runs: usize, budget: Duration) {
    let Some(store) = TestStore::on(backend, case).await else {
        return;
    };
    within(case, budget, async {
        // Two hundred runs at once keep every connection of a pool of the
        // harness's usual 5 busy, and a call that waits more than the acquire
        // timeout for one is answered 503. The server's own default is 10.
        let env: &[(&str, &str)] = match backend {
            Backend::Postgres => &[("JAMJET_PG_POOL_SIZE", "10")],
            Backend::Sqlite => &[],
        };
        let a = Engine::start(&store, env).await;
        let b = Engine::start(&store, env).await;
        let engines = [a.base(), b.base()];
        let http = client();
        let workflow = format!("two-engines-{}", uuid::Uuid::new_v4().simple());
        register(
            &http,
            &engines[0],
            &store.developer,
            &engine::approve_then_work(&workflow, None),
        )
        .await;

        let stop = Arc::new(AtomicBool::new(false));
        let answers = Answers::default();
        let shared = bases(&[&a, &b]);
        let workers = [
            worker(
                http.clone(),
                shared.clone(),
                store.developer.clone(),
                "worker-1",
                answers.clone(),
                stop.clone(),
            ),
            worker(
                http.clone(),
                shared.clone(),
                store.developer.clone(),
                "worker-2",
                answers.clone(),
                stop.clone(),
            ),
        ];

        // The engine each run started on, by the run's API id.
        let starter: Arc<Mutex<BTreeMap<String, usize>>> = Arc::default();

        // Approve each run through the engine that did not start it.
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
                                // Its start has not come back yet.
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

        // Round after round, start the runs on A and B in turn, twenty at a
        // time, and wait for them to finish, until a scheduler has logged a
        // stale pass or the window is over.
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
            if a.count(STALE) + b.count(STALE) >= 1 || Instant::now() >= race_over {
                break snapshot;
            }
            eprintln!(
                "{case}: no stale pass after round {rounds} ({} runs), another round",
                ids.len()
            );
        };
        stop.store(true, Ordering::SeqCst);
        approver.await.unwrap();
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
        let stale = a.count(STALE) + b.count(STALE);
        eprintln!(
            "{case}: {stale} stale passes ({} on A, {} on B), {} runs in {rounds} round(s)",
            a.count(STALE),
            b.count(STALE),
            ids.len()
        );
        assert!(
            stale >= 1,
            "neither scheduler logged a stale pass, so their race never opened"
        );
        a.assert_no_panics();
        b.assert_no_panics();
    })
    .await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_engines_on_postgres_complete_every_node_once() {
    every_node_once(
        Backend::Postgres,
        "two_engines_on_postgres_complete_every_node_once",
        200,
        Duration::from_secs(90),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_engines_on_one_sqlite_file_complete_every_node_once() {
    every_node_once(
        Backend::Sqlite,
        "two_engines_on_one_sqlite_file_complete_every_node_once",
        50,
        Duration::from_secs(60),
    )
    .await;
}

/// A cancel through A lands while a pass would finish the run: each run
/// ends once, cancelled or completed, with nothing after its end.
///
/// Thirty runs in two groups. Twenty complete their last step at once, so
/// one pass finishes them all, and their cancels go out 0 to 950 ms later:
/// some land while that pass works through the runs, after it listed them
/// as running. Ten complete 50 ms apart, so they meet the scheduler's
/// 500 ms tick at ten points, and each cancel goes out 20 ms after its
/// run's completion: whatever the tick's phase, most of those land while
/// the finish is due. The test asserts at least one cancel did, and at least
/// one of the ten staggered ones, so the bunched group alone cannot pass it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_engines_cancel_and_finish_race_once() {
    for backend in [Backend::Sqlite, Backend::Postgres] {
        let case = format!("two_engines_cancel_and_finish_race_once ({backend:?})");
        let Some(store) = TestStore::on(backend, &case).await else {
            continue;
        };
        within(&case, Duration::from_secs(10), async {
            // No embedded workers: nothing here is theirs to run.
            let no_workers = [("JAMJET_EMBED_WORKERS", "0")];
            let a = Engine::start(&store, &no_workers).await;
            let b = Engine::start(&store, &no_workers).await;
            let http = client();
            let workflow = format!("race-{}", uuid::Uuid::new_v4().simple());
            register(&http, &a.base(), &store.developer, &work_only(&workflow)).await;
            // (wait before the completion, wait from it to the cancel), in ms.
            let plan: Vec<(u64, u64)> = (0..20)
                .map(|j| (0, 50 * j))
                .chain((0..10).map(|k| (50 * k, 20)))
                .collect();
            let mut runs = Vec::new();
            for _ in 0..plan.len() {
                runs.push(start_run(&http, &a.base(), &store.developer, &workflow).await);
            }
            let mut held = Vec::new();
            while held.len() < runs.len() {
                if let (_, Some(item)) = claim(&http, &b.base(), &store.developer, "w", 30)
                    .await
                    .unwrap()
                {
                    held.push(item);
                }
            }
            let races = held.into_iter().zip(plan).map(|(item, (before, after))| {
                let http = http.clone();
                let (a, b) = (a.base(), b.base());
                let token = store.developer.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(before)).await;
                    let (st, body) = complete(&http, &b, &token, &item).await.unwrap();
                    assert_eq!(st, 200, "{body}");
                    tokio::time::sleep(Duration::from_millis(after)).await;
                    let (st, body) = must(
                        &http,
                        &a,
                        "POST",
                        &format!("/executions/{}/cancel", item.run),
                        &token,
                        Some(json!({})),
                    )
                    .await;
                    assert!(st == 200 || st == 400, "cancel answered {st}: {body}");
                    (db_id(&item.run), st)
                }
            });
            let outcomes = futures_join(races).await;
            let ids: Vec<String> = outcomes.iter().map(|(id, _)| id.clone()).collect();
            let snapshot = finished(&store, &ids, Duration::from_secs(10)).await;
            let mut cancelled_while_due = 0;
            let mut staggered_while_due = 0;
            for (n, (id, st)) in outcomes.iter().enumerate() {
                let kinds: Vec<&str> = snapshot.events_of(id).map(|e| e.kind_type()).collect();
                let ends: Vec<&&str> = kinds
                    .iter()
                    .filter(|k| k.starts_with("workflow_") && **k != "workflow_started")
                    .collect();
                assert_eq!(
                    ends.len(),
                    1,
                    "run {id} ended {} times: {kinds:?}",
                    ends.len()
                );
                let expected = if *st == 200 {
                    "workflow_cancelled"
                } else {
                    "workflow_completed"
                };
                assert_eq!(
                    *ends[0], expected,
                    "run {id}, cancel answered {st}: {kinds:?}"
                );
                if *st == 200 && kinds.contains(&"node_completed") {
                    cancelled_while_due += 1;
                    if n >= 20 {
                        staggered_while_due += 1;
                    }
                }
            }
            no_violations(&jamjet_testkit::invariants::check_snapshot(&snapshot));
            let lost = outcomes.iter().filter(|(_, st)| *st == 400).count();
            eprintln!(
                "{case}: {cancelled_while_due} cancels landed while the finish was due \
                 ({staggered_while_due} of the 10 staggered runs), {lost} came after it"
            );
            assert!(
                cancelled_while_due >= 1,
                "no cancel landed while a finish was due, so the race never opened"
            );
            assert!(
                staggered_while_due >= 1,
                "no staggered cancel landed while its finish was due, so the race \
                 opened only at one phase of the tick"
            );
            a.assert_no_panics();
            b.assert_no_panics();
        })
        .await;
    }
}

/// The next work item, as a worker claims it through `base` with a lease of
/// 30 s.
async fn claim_one(http: &reqwest::Client, base: &str, token: &str) -> engine::Claimed {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let (_, Some(item)) = claim(http, base, token, "holder", 30).await.unwrap() {
            return item;
        }
        assert!(Instant::now() < deadline, "nothing to claim");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// B's clock runs 120 s ahead. A worker holds a 30 s lease it took through
/// A: on B's clock it ran out 90 s ago, on the database's it has 30 s left.
/// For 10 s, while B's scheduler sweeps and its workers claim (each claim
/// reclaims expired leases first), the lease is not taken: no `NodeFailed`,
/// and the fenced completion still lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn skewed_engine_does_not_reclaim_a_live_lease() {
    let case = "skewed_engine_does_not_reclaim_a_live_lease";
    let Some(store) = TestStore::postgres(case).await else {
        return;
    };
    within(case, Duration::from_secs(15), async {
        let a = Engine::start(&store, &[("JAMJET_EMBED_WORKERS", "0")]).await;
        let b = Engine::start(&store, &[("JAMJET_TEST_CLOCK_SKEW_SECS", "120")]).await;
        let http = client();
        let workflow = format!("skew-lease-{}", uuid::Uuid::new_v4().simple());
        register(&http, &a.base(), &store.developer, &work_only(&workflow)).await;
        let run = start_run(&http, &a.base(), &store.developer, &workflow).await;
        let item = claim_one(&http, &a.base(), &store.developer).await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        let snapshot = store.snapshot().await;
        let id = db_id(&run);
        let kinds: Vec<&str> = snapshot.events_of(&id).map(|e| e.kind_type()).collect();
        assert!(
            !kinds.contains(&"node_failed") && !kinds.contains(&"retry_scheduled"),
            "the skewed engine took a live lease: {kinds:?}\n{}",
            b.log()
        );
        let (st, body) = complete(&http, &a.base(), &store.developer, &item)
            .await
            .unwrap();
        assert_eq!(st, 200, "the lease still holds: {body}");
        a.assert_no_panics();
        b.assert_no_panics();
    })
    .await;
}

/// B's clock runs 120 s ahead, and only B has workers, so B raises the
/// request. Its 60 s deadline is stamped on the database's clock, about
/// 60 s from now, and it is still pending after 5 s of B's sweeps, which
/// judge it on the same clock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn skewed_engine_does_not_expire_an_approval_early() {
    let case = "skewed_engine_does_not_expire_an_approval_early";
    let Some(store) = TestStore::postgres(case).await else {
        return;
    };
    within(case, Duration::from_secs(10), async {
        let skewed = [("JAMJET_TEST_CLOCK_SKEW_SECS", "120")];
        let a = Engine::start(&store, &[("JAMJET_EMBED_WORKERS", "0")]).await;
        let b = Engine::start(&store, &skewed).await;
        let http = client();
        let workflow = format!("skew-deadline-{}", uuid::Uuid::new_v4().simple());
        register(
            &http,
            &a.base(),
            &store.developer,
            &engine::approve_then_work(&workflow, Some(60)),
        )
        .await;
        let run = start_run(&http, &a.base(), &store.developer, &workflow).await;
        let row = engine::wait_for_request(
            &http,
            &b.base(),
            &store.reviewer,
            &run,
            Duration::from_secs(5),
        )
        .await;
        let pool = store.database().unwrap().pool().await;
        let db_now: chrono::DateTime<chrono::Utc> =
            sqlx::query_scalar("SELECT statement_timestamp()")
                .fetch_one(&pool)
                .await
                .unwrap();
        let expires: chrono::DateTime<chrono::Utc> =
            serde_json::from_value(row["expires_at"].clone()).expect("the request has a deadline");
        let left = (expires - db_now).num_seconds();
        assert!(
            (50..=60).contains(&left),
            "the deadline is {left} s away on the database's clock, not about 60: {row}"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        let snapshot = store.snapshot().await;
        let id = db_id(&run);
        let kinds: Vec<&str> = snapshot.events_of(&id).map(|e| e.kind_type()).collect();
        assert!(
            !kinds.contains(&"approval_expired"),
            "the skewed engine expired the request early: {kinds:?}"
        );
        let still: Vec<Value> = engine::pending(&http, &b.base(), &store.reviewer).await;
        assert!(
            still.iter().any(|r| r["execution_id"] == run.as_str()),
            "the request is no longer pending: {still:?}"
        );
        pool.close().await;
        a.assert_no_panics();
        b.assert_no_panics();
    })
    .await;
}

/// The invariant check names each part of each rule a store breaks: an
/// engine finishes twelve runs cleanly, the check finds nothing, then each
/// run gets one break (one gets an allowed `AuditSealFailed` after its end)
/// and the check must name exactly those, by rule, part and run.
async fn names_each_broken_rule(backend: Backend, case: &str) {
    let Some(store) = TestStore::on(backend, case).await else {
        return;
    };
    within(case, Duration::from_secs(30), async {
        let ids = finish_runs(&store, 12).await;
        assert!(store.check().await.is_empty(), "{:?}", store.check().await);
        let mut expected = BTreeSet::new();
        let mut expect = |rule: u8, clause: &'static str, run: Option<&String>| {
            expected.insert((rule, clause, run.cloned()));
        };
        breaks::delete_the_first_event(&store, &ids[0]).await;
        expect(1, "dense", Some(&ids[0]));
        breaks::revive_two_items(&store, &ids[1]).await;
        expect(2, "live_items", Some(&ids[1]));
        breaks::complete_twice(&store, &ids[2]).await;
        expect(2, "completed_twice", Some(&ids[2]));
        breaks::reopen_with_a_node_scheduled(&store, &ids[3]).await;
        expect(3, "stranded", Some(&ids[3]));
        breaks::end_twice(&store, &ids[4]).await;
        expect(4, "ends", Some(&ids[4]));
        breaks::append_after_the_end(&store, &ids[5]).await;
        expect(4, "after_end", Some(&ids[5]));
        // Allowed: a seal gap recorded after the run ended.
        breaks::seal_gap_after_the_end(&store, &ids[6]).await;
        breaks::set_status(&store, &ids[7], "failed").await;
        expect(4, "end_kind", Some(&ids[7]));
        breaks::set_status(&store, &ids[8], "running").await;
        expect(4, "unfinished_end", Some(&ids[8]));
        if backend == Backend::Postgres {
            breaks::drop_an_audit_entry(&store, &ids[9]).await;
            expect(5, "entries", Some(&ids[9]));
            // The entry after it no longer links.
            expect(5, "chain", None);
            breaks::refill_the_outbox(&store, &ids[10]).await;
            expect(5, "outbox", None);
            breaks::drop_the_genesis(&store).await;
            expect(5, "genesis", None);
        }
        let found: BTreeSet<(u8, &'static str, Option<String>)> = store
            .check()
            .await
            .into_iter()
            .map(|v| (v.rule, v.clause, v.run))
            .collect();
        assert_eq!(found, expected, "{:#?}", store.check().await);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_invariants_name_each_broken_rule_on_sqlite() {
    names_each_broken_rule(
        Backend::Sqlite,
        "the_invariants_name_each_broken_rule_on_sqlite",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_invariants_name_each_broken_rule_on_postgres() {
    names_each_broken_rule(
        Backend::Postgres,
        "the_invariants_name_each_broken_rule_on_postgres",
    )
    .await;
}

/// `n` runs of `approve` then `work`, finished by one engine; their ids as
/// the store writes them. The engine is gone when this returns.
async fn finish_runs(store: &TestStore, n: usize) -> Vec<String> {
    let a = Engine::start(store, &[]).await;
    let http = client();
    let workflow = format!("finished-{}", uuid::Uuid::new_v4().simple());
    register(
        &http,
        &a.base(),
        &store.developer,
        &engine::approve_then_work(&workflow, None),
    )
    .await;
    let stop = Arc::new(AtomicBool::new(false));
    let w = worker(
        http.clone(),
        bases(&[&a]),
        store.developer.clone(),
        "worker",
        Answers::default(),
        stop.clone(),
    );
    let mut runs = Vec::new();
    for _ in 0..n {
        runs.push(start_run(&http, &a.base(), &store.developer, &workflow).await);
    }
    for run in &runs {
        let row = engine::wait_for_request(
            &http,
            &a.base(),
            &store.reviewer,
            run,
            Duration::from_secs(10),
        )
        .await;
        let (st, body) = approve(&http, &a.base(), &store.reviewer, &row)
            .await
            .unwrap();
        assert_eq!(st, 200, "{body}");
    }
    let ids: Vec<String> = runs.iter().map(|r| db_id(r)).collect();
    finished(store, &ids, Duration::from_secs(20)).await;
    stop.store(true, Ordering::SeqCst);
    w.await.unwrap();
    drop(a);
    ids
}

/// One broken rule each, written into a finished store as a bug would. The
/// statements name runs by the ids the store gave them.
mod breaks {
    use super::TestStore;
    use sqlx::{Connection, Executor};

    async fn sql(store: &TestStore, statement: String) {
        match store.file() {
            Some(file) => {
                let mut conn =
                    sqlx::SqliteConnection::connect(&format!("sqlite://{}", file.display()))
                        .await
                        .unwrap();
                conn.execute(sqlx::AssertSqlSafe(statement)).await.unwrap();
                conn.close().await.unwrap();
            }
            None => {
                let pool = store.database().unwrap().pool().await;
                pool.execute(sqlx::AssertSqlSafe(statement)).await.unwrap();
                pool.close().await;
            }
        }
    }

    /// Rule 1: the run's log starts at 2.
    pub async fn delete_the_first_event(store: &TestStore, run: &str) {
        sql(
            store,
            format!("DELETE FROM events WHERE execution_id = '{run}' AND sequence = 1"),
        )
        .await;
    }

    /// Rule 2: both of `approve`'s items (the first dispatch and the one
    /// after the decision) are live again.
    pub async fn revive_two_items(store: &TestStore, run: &str) {
        sql(
            store,
            format!(
                "UPDATE work_items SET status = 'pending' \
                 WHERE execution_id = '{run}' AND node_id = 'approve'"
            ),
        )
        .await;
    }

    /// Rule 2: `approve` completes twice for its second dispatch, its
    /// `NodeStarted` there rewritten as a copy of its `NodeCompleted`.
    pub async fn complete_twice(store: &TestStore, run: &str) {
        let statement = if store.file().is_some() {
            format!(
                "UPDATE events SET kind_json = (SELECT kind_json FROM events \
                 WHERE execution_id = '{run}' AND kind_json LIKE '%\"type\":\"node_completed\"%' \
                 AND kind_json LIKE '%\"node_id\":\"approve\"%') \
                 WHERE execution_id = '{run}' AND sequence = (SELECT MAX(sequence) FROM events \
                 WHERE execution_id = '{run}' AND kind_json LIKE '%\"type\":\"node_started\"%')"
            )
        } else {
            format!(
                "UPDATE events SET kind_json = c.kind_json, kind_type = c.kind_type \
                 FROM (SELECT kind_json, kind_type FROM events WHERE execution_id = '{run}' \
                 AND kind_type = 'node_completed' AND node_id = 'approve') c \
                 WHERE execution_id = '{run}' AND sequence = (SELECT MAX(sequence) FROM events \
                 WHERE execution_id = '{run}' AND kind_type = 'node_started')"
            )
        };
        sql(store, statement).await;
    }

    /// Rule 3: the run is running again with `work` dispatched and its item
    /// settled: the log loses its last two events (work's completion and
    /// the run's end), which leaves it dense.
    pub async fn reopen_with_a_node_scheduled(store: &TestStore, run: &str) {
        sql(
            store,
            format!(
                "DELETE FROM events WHERE execution_id = '{run}' AND sequence >= \
                 (SELECT MAX(sequence) - 1 FROM events WHERE execution_id = '{run}')"
            ),
        )
        .await;
        sql(
            store,
            format!(
                "UPDATE workflow_executions SET status = 'running' WHERE execution_id = '{run}'"
            ),
        )
        .await;
    }

    /// Rule 4: the run's end is in its log twice, the second at the next
    /// sequence.
    pub async fn end_twice(store: &TestStore, run: &str) {
        let (id, kind_type) = if store.file().is_some() {
            (format!("'{}'", uuid::Uuid::new_v4()), "")
        } else {
            ("gen_random_uuid()".to_string(), "kind_type, ")
        };
        sql(
            store,
            format!(
                "INSERT INTO events (id, execution_id, sequence, {kind_type}kind_json, created_at) \
                 SELECT {id}, execution_id, sequence + 1, {kind_type}kind_json, created_at \
                 FROM events WHERE execution_id = '{run}' AND sequence = \
                 (SELECT MAX(sequence) FROM events WHERE execution_id = '{run}')"
            ),
        )
        .await;
    }

    /// Rule 4: an event after the run's end, a copy of its last
    /// `NodeScheduled` at the next sequence.
    pub async fn append_after_the_end(store: &TestStore, run: &str) {
        copy_to_the_end(store, run, "node_scheduled").await;
    }

    /// Allowed by rule 4: an `AuditSealFailed` after the run's end, made
    /// from a copy of its last event with its kind rewritten.
    pub async fn seal_gap_after_the_end(store: &TestStore, run: &str) {
        let gap = serde_json::json!({
            "type": "audit_seal_failed",
            "event_id": uuid::Uuid::new_v4(),
            "sequence": 3,
            "event_type": "tool_approval_required",
            "error": "the audit log was unreachable",
        })
        .to_string();
        let (id, kind_type, kind_value) = if store.file().is_some() {
            (format!("'{}'", uuid::Uuid::new_v4()), "", "")
        } else {
            (
                "gen_random_uuid()".to_string(),
                "kind_type, node_id, ",
                "'audit_seal_failed', NULL, ",
            )
        };
        sql(
            store,
            format!(
                "INSERT INTO events (id, execution_id, sequence, {kind_type}kind_json, created_at) \
                 SELECT {id}, execution_id, sequence + 1, {kind_value}'{gap}', created_at \
                 FROM events WHERE execution_id = '{run}' AND sequence = \
                 (SELECT MAX(sequence) FROM events WHERE execution_id = '{run}')"
            ),
        )
        .await;
    }

    /// Rule 4: the run's status, set without its events.
    pub async fn set_status(store: &TestStore, run: &str, status: &str) {
        sql(
            store,
            format!(
                "UPDATE workflow_executions SET status = '{status}' WHERE execution_id = '{run}'"
            ),
        )
        .await;
    }

    /// Rule 5: an outbox row again for the run's sealed decision.
    pub async fn refill_the_outbox(store: &TestStore, run: &str) {
        sql(
            store,
            format!(
                "INSERT INTO audit_outbox (event_id, execution_id, sequence, tenant_id, kind_type) \
                 SELECT id, execution_id, sequence, tenant_id, kind_type FROM events \
                 WHERE execution_id = '{run}' AND kind_type = 'approval_received'"
            ),
        )
        .await;
    }

    /// Rule 5: the chain's first entry gone, so no entry is its genesis.
    pub async fn drop_the_genesis(store: &TestStore) {
        sql(
            store,
            "DELETE FROM audit_log WHERE prev_hash IS NULL".to_string(),
        )
        .await;
    }

    /// A copy of the run's last event of `kind`, at the run's next
    /// sequence.
    async fn copy_to_the_end(store: &TestStore, run: &str, kind: &str) {
        let (id, kind_type, of_kind) = if store.file().is_some() {
            (
                format!("'{}'", uuid::Uuid::new_v4()),
                "",
                format!("kind_json LIKE '%\"type\":\"{kind}\"%'"),
            )
        } else {
            (
                "gen_random_uuid()".to_string(),
                "kind_type, node_id, ",
                format!("kind_type = '{kind}'"),
            )
        };
        sql(
            store,
            format!(
                "INSERT INTO events (id, execution_id, sequence, {kind_type}kind_json, created_at) \
                 SELECT {id}, execution_id, \
                   (SELECT MAX(sequence) + 1 FROM events WHERE execution_id = '{run}'), \
                   {kind_type}kind_json, created_at \
                 FROM events WHERE execution_id = '{run}' AND sequence = \
                 (SELECT MAX(sequence) FROM events WHERE execution_id = '{run}' AND {of_kind})"
            ),
        )
        .await;
    }

    /// Rule 5: the run's approval request has no audit entry.
    pub async fn drop_an_audit_entry(store: &TestStore, run: &str) {
        sql(
            store,
            format!(
                "DELETE FROM audit_log WHERE event_id IN (SELECT id FROM events \
                 WHERE execution_id = '{run}' AND kind_type = 'tool_approval_required')"
            ),
        )
        .await;
    }
}
