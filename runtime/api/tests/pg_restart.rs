//! The Postgres server restarts under the engines.
//!
//! These cases run `docker restart` on the container
//! `JAMJET_TEST_POSTGRES_CONTAINER` names, which must be the server
//! `JAMJET_TEST_POSTGRES_URL` points at: its published port is checked
//! first. A restart cuts every other test on that server, so CI runs this
//! binary alone, after the rest, with `--test-threads=1`. Without the
//! container variable the cases skip, unless `JAMJET_REQUIRE_RESTART_TESTS`
//! is set, as on that CI step: then a missing variable fails them.
//!
//! One case needs no server: [`an_unreachable_store_answers_503`] fails
//! the store's calls in process, so the 503 the restart case can only meet
//! by timing is met on every run.
#![cfg(unix)]

#[path = "support/engine.rs"]
mod engine;
mod support;

use chrono::Utc;
use engine::{
    approve, approve_then_work, bases, call, client, db_id, register, start_run, worker, Answers,
    Engine, TestStore,
};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{EventKind, PgBackend, PgSettings, StateBackend, StateBackendError, WorkItem};
use jamjet_testkit::pg::TestDatabase;
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Names the test server's Docker container.
const CONTAINER_ENV: &str = "JAMJET_TEST_POSTGRES_CONTAINER";
/// Set to `1` to make a missing [`CONTAINER_ENV`] a failure.
const REQUIRE_ENV: &str = "JAMJET_REQUIRE_RESTART_TESTS";

/// The container to restart, `None` (skipped) when none is named. Panics
/// when one is required and none is named, and when the container does not
/// publish the port of the server the tests connect to.
fn container(case: &str) -> Option<String> {
    let named = std::env::var(CONTAINER_ENV).ok().filter(|c| !c.is_empty());
    let Some(name) = named else {
        assert!(
            !std::env::var(REQUIRE_ENV).is_ok_and(|v| !v.is_empty() && v != "0"),
            "{REQUIRE_ENV} is set but {CONTAINER_ENV} is not"
        );
        eprintln!("skipped {case}: {CONTAINER_ENV} not set");
        return None;
    };
    let url = jamjet_testkit::pg::admin_url()?;
    let port = url
        .split_once("://")
        .and_then(|(_, rest)| rest.rsplit_once('@').map_or(Some(rest), |(_, h)| Some(h)))
        .and_then(|host| host.split(['/', '?']).next())
        .and_then(|host| host.rsplit_once(':'))
        .map_or("5432".to_string(), |(_, port)| port.to_string());
    let out = std::process::Command::new("docker")
        .args(["port", &name, "5432/tcp"])
        .output()
        .expect("run docker port");
    let published = String::from_utf8_lossy(&out.stdout);
    assert!(
        published
            .lines()
            .any(|line| line.rsplit(':').next() == Some(port.as_str())),
        "container {name} does not publish port {port}, the one the test server URL names: \
         {published}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(name)
}

/// `docker restart` on another thread; join it for the outcome.
fn restart(container: String) -> std::thread::JoinHandle<std::process::Output> {
    std::thread::spawn(move || {
        std::process::Command::new("docker")
            .args(["restart", "-t", "1", &container])
            .output()
            .expect("run docker restart")
    })
}

fn restarted(out: std::process::Output) {
    assert!(
        out.status.success(),
        "docker restart failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Wait until the test server takes connections. `docker restart` returns
/// once the container runs, which can be before Postgres inside is ready,
/// and a case can start right after another case's restart.
async fn server_ready() {
    use sqlx::Connection;
    let url = jamjet_testkit::pg::admin_url().expect("the test server URL");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match sqlx::PgConnection::connect(&url).await {
            Ok(conn) => {
                let _ = conn.close().await;
                return;
            }
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "the test server took no connection for 60 s: {e}"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// The server restarts under a pool of the store: while it is down, calls
/// answer `Unavailable` and nothing else, and once it is back they succeed
/// on the same store, with no reconnect by the caller.
#[tokio::test]
async fn the_pool_reconnects_after_a_restart() {
    let Some(container) = container("the_pool_reconnects_after_a_restart") else {
        return;
    };
    server_ready().await;
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("the_pool_reconnects_after_a_restart");
    };
    let settings = PgSettings {
        pool_size: 3,
        acquire_timeout: Duration::from_secs(1),
        ..PgSettings::default()
    };
    let b = PgBackend::connect(db.url(), settings).await.unwrap();
    let run = ExecutionId::new();
    let now = Utc::now();
    let item = WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: run.clone(),
        node_id: "begin".into(),
        queue_type: "work".into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: now,
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: jamjet_state::DEFAULT_TENANT.into(),
    };
    b.start_execution_atomic(
        WorkflowExecution {
            execution_id: run.clone(),
            workflow_id: "wf".into(),
            workflow_version: "1.0.0".into(),
            status: WorkflowStatus::Running,
            initial_input: json!({}),
            current_state: json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
            parent_execution_id: None,
            segment_number: 0,
        },
        EventKind::WorkflowStarted {
            workflow_id: "wf".into(),
            workflow_version: "1.0.0".into(),
            initial_input: json!({}),
            initiated_by: None,
            run_key: None,
        },
        EventKind::NodeScheduled {
            node_id: "begin".into(),
            queue_type: "work".into(),
        },
        item,
    )
    .await
    .unwrap();

    let restart = restart(container);
    let mut unavailable = 0;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut back = false;
    while Instant::now() < deadline {
        match b.get_execution(&run).await {
            Ok(found) => {
                assert!(found.is_some(), "the run is gone after the restart");
                if restart.is_finished() && unavailable > 0 {
                    back = true;
                    break;
                }
            }
            Err(StateBackendError::Unavailable(_)) => unavailable += 1,
            Err(other) => panic!("a call during the restart answered {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    restarted(restart.join().unwrap());
    server_ready().await;
    assert!(unavailable > 0, "no call saw the server down");
    assert!(
        back,
        "the store did not work again within 60 s of the restart"
    );
    let item = b.claim_work_item("w1", &["work"]).await.unwrap();
    assert!(item.is_some(), "a claim after the restart");
}

/// Twenty runs in flight on two engines when the server restarts. While
/// it is down the engines answer 503 and never 500; afterwards every run
/// finishes and the invariants hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_restart_mid_run_keeps_invariants() {
    let case = "postgres_restart_mid_run_keeps_invariants";
    let Some(container) = container(case) else {
        return;
    };
    server_ready().await;
    let Some(store) = TestStore::postgres(case).await else {
        return;
    };
    let budget = Duration::from_secs(60);
    let start = Instant::now();
    tokio::time::timeout(budget * 2, async {
        // A call that finds no connection gives up after 500 ms, so the
        // calls made while the server is down answer before it is back.
        let quick = [("JAMJET_PG_ACQUIRE_TIMEOUT_MS", "500")];
        let a = Engine::start(&store, &quick).await;
        let b = Engine::start(&store, &quick).await;
        let engines = [a.base(), b.base()];
        let http = client();
        let workflow = format!("restart-{}", uuid::Uuid::new_v4().simple());
        register(
            &http,
            &engines[0],
            &store.developer,
            &approve_then_work(&workflow, None),
        )
        .await;
        let mut runs = Vec::new();
        for i in 0..20 {
            runs.push(start_run(&http, &engines[i % 2], &store.developer, &workflow).await);
        }
        let ids: Vec<String> = runs.iter().map(|r| db_id(r)).collect();

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
        // The approver decides through both engines, slowly enough that
        // decisions are still going out while the server restarts.
        let approver = {
            let (http, engines, token) = (http.clone(), engines.clone(), store.reviewer.clone());
            let (stop, answers) = (stop.clone(), answers.clone());
            tokio::spawn(async move {
                let mut decided: BTreeSet<String> = BTreeSet::new();
                let mut turn = 0;
                while !stop.load(Ordering::SeqCst) {
                    turn += 1;
                    let base = &engines[turn % 2];
                    match call(&http, base, "GET", "/approvals?limit=200", &token, None).await {
                        Ok((200, body)) => {
                            let rows = body["approvals"].as_array().cloned().unwrap_or_default();
                            if let Some(row) = rows
                                .iter()
                                .find(|r| !decided.contains(r["execution_id"].as_str().unwrap()))
                            {
                                let done = approve(&http, base, &token, row).await;
                                answers.note_answer("approve", &done);
                                if let Ok((200, _)) = done {
                                    decided.insert(row["execution_id"].as_str().unwrap().into());
                                }
                            }
                        }
                        other => answers.note_answer("list approvals", &other),
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            })
        };
        // A reader on each engine through the restart.
        let reader = {
            let (http, engines, token) = (http.clone(), engines.clone(), store.developer.clone());
            let (stop, answers, run) = (stop.clone(), answers.clone(), runs[0].clone());
            tokio::spawn(async move {
                while !stop.load(Ordering::SeqCst) {
                    for base in &engines {
                        let read = call(
                            &http,
                            base,
                            "GET",
                            &format!("/executions/{run}"),
                            &token,
                            None,
                        )
                        .await;
                        answers.note_answer("read a run", &read);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
        };

        // Restart once some runs are past their approval and some are not.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let restart = restart(container);
        while !restart.is_finished() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        restarted(restart.join().unwrap());
        server_ready().await;

        let deadline = Instant::now() + budget;
        loop {
            let snapshot = store.snapshot().await;
            let open: Vec<&String> = ids
                .iter()
                .filter(|id| snapshot.status_of(id) != Some("completed"))
                .collect();
            if open.is_empty() {
                let violations = jamjet_testkit::invariants::check_snapshot(&snapshot);
                assert!(
                    violations.is_empty(),
                    "invariants broken after the restart:\n{}",
                    violations
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{} runs still open, such as {}: {:?}\nA: {}\nB: {}",
                open.len(),
                open[0],
                snapshot
                    .events_of(open[0])
                    .map(|e| e.kind_type())
                    .collect::<Vec<_>>(),
                a.log(),
                b.log()
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        stop.store(true, Ordering::SeqCst);
        approver.await.unwrap();
        reader.await.unwrap();
        for w in workers {
            w.await.unwrap();
        }
        eprintln!("{case}: answers by status {}", answers.summary());
        assert_eq!(
            answers.count(500),
            0,
            "an engine answered 500: {}",
            answers.summary()
        );
        assert!(
            answers.count(503) > 0,
            "no call saw the database down: {}",
            answers.summary()
        );
    })
    .await
    .unwrap_or_else(|_| panic!("{case} ran past twice its {budget:?} budget"));
    eprintln!("{case}: {:.1} s", start.elapsed().as_secs_f64());
}

/// A store that cannot be reached, as while its database restarts: the
/// token check fails, then (with tokens readable again) a decision's first
/// read of the log fails. Both answer 503 with `Retry-After`, not 500.
#[tokio::test]
async fn an_unreachable_store_answers_503() {
    use axum::body::Body;
    use axum::http::Request;
    use jamjet_testkit::{Hooks, Intercept, Op};
    use std::sync::atomic::AtomicU8;
    use tower::ServiceExt;

    /// 0: the store answers; 1: the token check fails; 2: reading a log fails.
    struct Down(AtomicU8);

    #[async_trait::async_trait]
    impl Hooks for Down {
        async fn before(&self, op: &Op<'_>) -> Option<StateBackendError> {
            let down = match self.0.load(Ordering::SeqCst) {
                1 => matches!(op, Op::ValidateToken),
                2 => matches!(op, Op::GetEvents(_)),
                _ => false,
            };
            down.then(|| StateBackendError::Unavailable("the database is restarting".into()))
        }
    }

    let down = Arc::new(Down(AtomicU8::new(0)));
    let inner = Arc::new(jamjet_state::InMemoryBackend::new());
    let (token, _) = inner.create_token("reviewer", "reviewer").await.unwrap();
    let backend: Arc<dyn StateBackend> = Arc::new(Intercept::new(inner, down.clone()));
    let router = jamjet_api::routes::build_router_with_opts(support::make_state(backend), false);
    let send = |method: &str, path: &str, body: serde_json::Value| {
        Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let run = ExecutionId::new();

    down.0.store(1, Ordering::SeqCst);
    let resp = router
        .clone()
        .oneshot(send("GET", "/executions", serde_json::Value::Null))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503, "the token check");
    assert_eq!(resp.headers()["retry-after"], "1");

    down.0.store(2, Ordering::SeqCst);
    let resp = router
        .oneshot(send(
            "POST",
            &format!("/executions/{run}/approve"),
            json!({ "decision": "approved", "node_id": "approve" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503, "a decision");
    assert_eq!(resp.headers()["retry-after"], "1");
}
