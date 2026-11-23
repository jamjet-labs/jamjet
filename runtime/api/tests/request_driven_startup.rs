//! `JAMJET_BACKGROUND` at startup, on the real binary: with the loops off a
//! setting that needs them stops the server, nothing dispatches a run by
//! itself, and with the loops on a serverless platform gets one warning.
//!
//! The Postgres case of the dispatch test skips without
//! `JAMJET_TEST_POSTGRES_URL`.
#![cfg(unix)]

#[path = "support/engine.rs"]
mod engine;

use engine::{client, must, register, start_run, Backend, Engine, TestStore};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The line a server with the loops off logs once at start.
const LOOPS_OFF: &str = "background loops are off; call POST /ops/tick every minute";
/// Part of the warning a server with the loops on a platform logs.
const NEEDS_CPU: &str = "need CPU between requests";
/// Part of the line that says the embedded workers are not started.
const WORKERS_IGNORED: &str = "JAMJET_EMBED_WORKERS is ignored";

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jjrd-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    // An empty `.env` stops `dotenvy` reading a stray one in a parent.
    std::fs::write(dir.join(".env"), "").unwrap();
    dir
}

/// Run the server in `dir` with a cleared environment plus `env`, to its
/// end within 30 s: the exit code (`None` when it had to be killed) and
/// everything it printed.
fn run_to_end(dir: &Path, env: &[(&str, &str)]) -> (Option<i32>, String) {
    let mut cmd = Command::new(engine::BIN);
    cmd.current_dir(dir).env_clear();
    for key in ["PATH", "HOME", "TMPDIR"] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    cmd.env("JAMJET_BIND", "127.0.0.1")
        .env("JAMJET_PORT", "0")
        .env("RUST_LOG", "info");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let code = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut out = String::new();
    for pipe in [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read>),
    ]
    .into_iter()
    .flatten()
    {
        let mut pipe = pipe;
        let _ = pipe.read_to_string(&mut out);
    }
    (code, out)
}

#[test]
fn loops_off_with_embedded_cron_refuses() {
    let dir = temp_dir();
    let (code, out) = run_to_end(
        &dir,
        &[("JAMJET_BACKGROUND", "off"), ("JAMJET_EMBED_CRON", "1")],
    );
    let code = code.unwrap_or_else(|| panic!("the server must refuse and exit, not serve: {out}"));
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("JAMJET_BACKGROUND"), "{out}");
    assert!(out.contains("JAMJET_EMBED_CRON"), "{out}");
    // Refused before any store was opened.
    assert!(
        !dir.join(".jamjet").exists(),
        "a .jamjet directory was made"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A tick that could take every connection the pool has would leave none
/// for the requests: refused before the database is reached, so the URL
/// needs no server behind it.
#[test]
fn loops_off_on_postgres_refuses_a_tick_that_can_take_the_pool() {
    let dir = temp_dir();
    let (code, out) = run_to_end(
        &dir,
        &[
            ("JAMJET_BACKGROUND", "off"),
            ("DATABASE_URL", "postgres://jj@127.0.0.1:1/none"),
            ("JAMJET_PG_POOL_SIZE", "5"),
            ("JAMJET_TICK_ENGINE_CONCURRENCY", "4"),
        ],
    );
    let code = code.unwrap_or_else(|| panic!("the server must refuse and exit, not serve: {out}"));
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("JAMJET_TICK_ENGINE_CONCURRENCY"), "{out}");
    assert!(out.contains("JAMJET_PG_POOL_SIZE"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn loops_off_refuses_a_driver_setting_that_does_not_parse() {
    let dir = temp_dir();
    let (code, out) = run_to_end(
        &dir,
        &[
            ("JAMJET_BACKGROUND", "off"),
            ("STORAGE_BACKEND", "memory"),
            ("JAMJET_TICK_BUDGET_MS", "45s"),
        ],
    );
    let code = code.unwrap_or_else(|| panic!("the server must refuse and exit, not serve: {out}"));
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("JAMJET_TICK_BUDGET_MS"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_typo_in_the_switch_refuses() {
    let dir = temp_dir();
    let (code, out) = run_to_end(
        &dir,
        &[("JAMJET_BACKGROUND", "of"), ("STORAGE_BACKEND", "memory")],
    );
    let code = code.unwrap_or_else(|| panic!("the server must refuse and exit, not serve: {out}"));
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("JAMJET_BACKGROUND"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The engine still runs model steps with the loops off (inside requests),
/// so a bad model setup still stops startup.
#[tokio::test]
async fn loops_off_still_checks_the_model_setup() {
    let dir = temp_dir();
    let (code, out) = run_to_end(
        &dir,
        &[
            ("JAMJET_BACKGROUND", "off"),
            ("STORAGE_BACKEND", "memory"),
            ("JAMJET_MODEL_TIMEOUT_SECS", "soon"),
        ],
    );
    let code = code.unwrap_or_else(|| panic!("the server must refuse and exit, not serve: {out}"));
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("JAMJET_MODEL_TIMEOUT_SECS"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
    // With the loops on and no embedded workers the setting is not read, so
    // the refusal above is the loops-off rule.
    let store = TestStore::sqlite_file().await;
    let on = Engine::start(
        &store,
        &[
            ("JAMJET_EMBED_WORKERS", "0"),
            ("JAMJET_MODEL_TIMEOUT_SECS", "soon"),
        ],
    )
    .await;
    assert!(on.is_running());
}

/// Dev mode starts the embedded workers and cron by itself; with the loops
/// off it starts neither, nor any other loop.
#[tokio::test]
async fn loops_off_in_dev_mode_starts_no_loop() {
    let store = TestStore::sqlite_file().await;
    let engine = Engine::start(
        &store,
        &[
            ("JAMJET_DEV_MODE", "1"),
            ("JAMJET_EMBED_WORKERS", "0"),
            ("JAMJET_BACKGROUND", "off"),
        ],
    )
    .await;
    assert!(printed(&engine, LOOPS_OFF).await, "{}", engine.log());
    // Give a loop that did start the time to say so.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let log = engine.log();
    for started in STARTED {
        assert!(!log.contains(started), "{started}: {log}");
    }
    drop(engine);
    // Dev mode with the loops on logs every one of them but the outbox
    // drain (Postgres only), so the case above can fail.
    let engine = Engine::start(
        &store,
        &[("JAMJET_DEV_MODE", "1"), ("JAMJET_EMBED_WORKERS", "0")],
    )
    .await;
    for started in &STARTED[..4] {
        assert!(
            printed(&engine, started).await,
            "{started}: {}",
            engine.log()
        );
    }
}

/// What the server logs as it starts each loop.
const STARTED: [&str; 5] = [
    "Scheduler started",
    "Projector started",
    "Embedded worker pool started",
    "Cron scheduler started",
    "Audit outbox drain started",
];

/// Wait until `engine` has printed `needle`, for up to 10 s.
async fn printed(engine: &Engine, needle: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if engine.log().contains(needle) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn loops_on_on_a_platform_warns_once() {
    let store = TestStore::sqlite_file().await;
    let platform = [("K_SERVICE", "svc"), ("JAMJET_ALLOW_EPHEMERAL_STORE", "1")];
    let on = Engine::start(&store, &platform).await;
    assert!(printed(&on, NEEDS_CPU).await, "{}", on.log());
    // Let the loops run a while: the warning is not repeated.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(on.count(NEEDS_CPU), 1, "{}", on.log());
    assert!(on.log().contains("JAMJET_BACKGROUND=off"), "{}", on.log());
    assert_eq!(on.count(LOOPS_OFF), 0, "{}", on.log());
    drop(on);

    // Off a platform, and with the loops off on one, no warning.
    let off_platform = Engine::start(&store, &[]).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(off_platform.count(NEEDS_CPU), 0, "{}", off_platform.log());
    drop(off_platform);
    let mut off: Vec<(&str, &str)> = platform.to_vec();
    off.push(("JAMJET_BACKGROUND", "off"));
    let off = Engine::start(&store, &off).await;
    assert!(printed(&off, LOOPS_OFF).await, "{}", off.log());
    assert_eq!(off.count(NEEDS_CPU), 0, "{}", off.log());
}

/// `route` (a `condition` node the engine runs), then `work` (a `python_fn`
/// node a worker claims), then the end.
fn condition_then_work(id: &str) -> Value {
    let node = |id: &str, kind: Value| {
        json!({
            "id": id, "kind": kind,
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
        })
    };
    json!({
        "workflow_id": id, "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "route",
        "nodes": {
            "route": node("route", json!({ "type": "condition", "branches": [] })),
            "work": node("work", json!({
                "type": "python_fn", "module": "tests.steps", "function": "work", "output_schema": ""
            })),
        },
        "edges": [
            { "from": "route", "to": "work", "condition": null },
            { "from": "work", "to": "end", "condition": null }
        ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

/// `(kind, node)` of every event of `run`, in order.
async fn events(engine: &Engine, token: &str, run: &str) -> Vec<(String, String)> {
    let (st, body) = must(
        &client(),
        &engine.base(),
        "GET",
        &format!("/executions/{run}/events"),
        token,
        None,
    )
    .await;
    assert_eq!(st, 200, "{body}");
    body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["kind"]["type"].as_str().unwrap_or("").to_string(),
                e["kind"]["node_id"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

fn scheduled(events: &[(String, String)]) -> Vec<&str> {
    events
        .iter()
        .filter(|(kind, _)| kind == "node_scheduled")
        .map(|(_, node)| node.as_str())
        .collect()
}

/// Start a run of [`condition_then_work`] on an engine with `env`; its
/// events after 3 s, the engine, and the run.
async fn run_for_three_seconds(
    store: &TestStore,
    name: &str,
    env: &[(&str, &str)],
) -> (Vec<(String, String)>, Engine, String) {
    let engine = Engine::start(store, env).await;
    let http = client();
    register(
        &http,
        &engine.base(),
        &store.developer,
        &condition_then_work(name),
    )
    .await;
    let run = start_run(&http, &engine.base(), &store.developer, name).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    (events(&engine, &store.developer, &run).await, engine, run)
}

async fn loops_off_dispatches_nothing(backend: Backend) {
    let Some(store) = TestStore::on(backend, "loops_off_dispatches_nothing_by_itself").await else {
        return;
    };
    // The harness's pool holds 5 connections: a tick may use 3 at most.
    let off = [
        ("JAMJET_BACKGROUND", "off"),
        ("JAMJET_TICK_ENGINE_CONCURRENCY", "3"),
    ];
    let (seen, engine, run) = run_for_three_seconds(&store, "rd-off", &off).await;
    assert_eq!(scheduled(&seen), vec!["route"], "{seen:?}");
    assert!(
        !seen.iter().any(|(kind, _)| kind == "node_completed"),
        "{seen:?}"
    );
    let log = engine.log();
    assert_eq!(engine.count(LOOPS_OFF), 1, "{log}");
    // The harness sets JAMJET_EMBED_WORKERS=1: ignored, said once.
    assert_eq!(engine.count(WORKERS_IGNORED), 1, "{log}");
    for started in STARTED {
        assert!(!log.contains(started), "{started}: {log}");
    }

    // A developer's token cannot make it act. A tick with the operator's does
    // what the loops would have: the first runs `route`, which sat on the
    // engine's queue, and the next sees it done and dispatches `work`.
    let http = client();
    let tick = |token: String| {
        let (http, base) = (http.clone(), engine.base());
        async move { must(&http, &base, "POST", "/ops/tick", &token, Some(json!({}))).await }
    };
    let (st, refused) = tick(store.developer.clone()).await;
    assert_eq!(st, 403, "{refused}");
    let seen = events(&engine, &store.developer, &run).await;
    assert_eq!(scheduled(&seen), vec!["route"], "{seen:?}");

    let (st, report) = tick(store.operator.clone()).await;
    assert_eq!(st, 200, "{report}");
    assert_eq!(report["ran"], true, "{report}");
    assert_eq!(report["background"], "off", "{report}");
    assert_eq!(report["engine_steps"]["slow"], 1, "{report}");
    let seen = events(&engine, &store.developer, &run).await;
    assert!(
        seen.contains(&("node_completed".to_string(), "route".to_string())),
        "{seen:?}"
    );

    let (st, report) = tick(store.operator.clone()).await;
    assert_eq!(st, 200, "{report}");
    assert_eq!(report["runs"]["advanced"], 1, "{report}");
    let seen = events(&engine, &store.developer, &run).await;
    assert_eq!(scheduled(&seen), vec!["route", "work"], "{seen:?}");
    drop(engine);

    // The same workflow with the loops on runs `route` and dispatches
    // `work` in that time, so the case above can fail.
    let (seen, _engine, _run) = run_for_three_seconds(&store, "rd-on", &[]).await;
    assert_eq!(scheduled(&seen), vec!["route", "work"], "{seen:?}");
}

#[tokio::test]
async fn loops_off_dispatches_nothing_by_itself() {
    loops_off_dispatches_nothing(Backend::Sqlite).await;
}

#[tokio::test]
async fn loops_off_dispatches_nothing_by_itself_on_postgres() {
    loops_off_dispatches_nothing(Backend::Postgres).await;
}
