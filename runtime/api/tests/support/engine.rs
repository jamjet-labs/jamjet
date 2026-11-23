//! Engines as processes, for the tests that run several on one store, kill
//! one at a fault point, or restart the database under them.
//!
//! Each engine is the real `jamjet-server` binary, started in its store's
//! temp directory with a cleared environment on `JAMJET_PORT=0`. The harness
//! waits for `listening on` and reads the address from it, and keeps what
//! the process prints, so a test can count log lines or find the message a
//! fault point prints when it aborts the process. A store is migrated by
//! `jamjet-server migrate`, and its tokens come from `jamjet-server token
//! create`, as an operator would make them.
//!
//! Every wait here is async, so a case's own timeout can fire during any
//! of them.
#![allow(dead_code)]

use jamjet_testkit::invariants::{self, Snapshot, Store, Violation, AUDIT_KEY};
use jamjet_testkit::pg::TestDatabase;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_jamjet-server");
/// How long a process may take to listen, or a command to run.
pub const WAIT: Duration = Duration::from_secs(60);
/// The signal `std::process::abort` raises, as an exit status reports it.
pub const SIGABRT: i32 = 6;
/// The queue the scheduler puts a `python_fn` node's item on.
pub const PYTHON_QUEUE: &str = "python_tool";

// ── Stores ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
    Postgres,
}

/// A store for engines to share: a SQLite file or a Postgres database of its
/// own, migrated, with a developer and a reviewer token. Its directory is
/// where every engine and command on it runs.
pub struct TestStore {
    pub backend: Backend,
    dir: PathBuf,
    url: String,
    file: Option<PathBuf>,
    db: Option<TestDatabase>,
    /// Starts runs, and works items as a worker does.
    pub developer: String,
    /// Decides approvals.
    pub reviewer: String,
}

impl Drop for TestStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl TestStore {
    /// A SQLite file in a temp directory of its own.
    pub async fn sqlite_file() -> TestStore {
        let dir = temp_dir();
        let file = dir.join("runtime.db");
        let url = format!("sqlite://{}", file.display());
        Self::made(Backend::Sqlite, dir, url, Some(file), None).await
    }

    /// A Postgres database of its own, `None` (skipped) without a Postgres
    /// URL.
    pub async fn postgres(case: &str) -> Option<TestStore> {
        let Some(db) = TestDatabase::empty().await else {
            jamjet_testkit::skip(case);
            return None;
        };
        let url = db.url().to_string();
        Some(Self::made(Backend::Postgres, temp_dir(), url, None, Some(db)).await)
    }

    /// A store on `backend`, `None` for Postgres when it is not configured.
    pub async fn on(backend: Backend, case: &str) -> Option<TestStore> {
        match backend {
            Backend::Sqlite => Some(Self::sqlite_file().await),
            Backend::Postgres => Self::postgres(case).await,
        }
    }

    async fn made(
        backend: Backend,
        dir: PathBuf,
        url: String,
        file: Option<PathBuf>,
        db: Option<TestDatabase>,
    ) -> TestStore {
        let migrated = run(command(&dir, &url, &["migrate"], &[])).await;
        assert_eq!(migrated.code, Some(0), "migrate: {}", migrated.all());
        let token = |role: &'static str| {
            let dir = dir.clone();
            let url = url.clone();
            async move {
                let made = run(command(
                    &dir,
                    &url,
                    &["token", "create", "--role", role, "--name", role],
                    &[],
                ))
                .await;
                assert_eq!(made.code, Some(0), "token create: {}", made.all());
                made.stdout.trim().to_string()
            }
        };
        let developer = token("developer").await;
        let reviewer = token("reviewer").await;
        TestStore {
            backend,
            dir,
            url,
            file,
            db,
            developer,
            reviewer,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The store as the invariant check reads it.
    pub fn store(&self) -> Store {
        match &self.file {
            Some(file) => Store::SqliteFile(file.clone()),
            None => Store::Postgres(self.url.clone()),
        }
    }

    /// Every invariant the store breaks.
    pub async fn check(&self) -> Vec<Violation> {
        invariants::check(&self.store()).await
    }

    /// What the invariant check reads, for a test's own assertions.
    pub async fn snapshot(&self) -> Snapshot {
        invariants::read(&self.store()).await
    }

    /// The Postgres database, `None` on SQLite.
    pub fn database(&self) -> Option<&TestDatabase> {
        self.db.as_ref()
    }

    /// The SQLite file, `None` on Postgres.
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jjeng-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    // `dotenvy` walks up to the first `.env`; an empty one stops it here.
    std::fs::write(dir.join(".env"), "").unwrap();
    dir
}

/// The binary with only `PATH`, `HOME` and `TMPDIR` inherited, run in `dir`
/// on `url`, signing the audit log with the key the invariant check
/// verifies.
fn command(dir: &Path, url: &str, args: &[&str], env: &[(String, String)]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.args(args).current_dir(dir).env_clear();
    for key in ["PATH", "HOME", "TMPDIR"] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    cmd.env("RUST_LOG", "info")
        .env("DATABASE_URL", url)
        .env("JAMJET_AUDIT_SIGNING_KEY", AUDIT_KEY)
        .env("JAMJET_PG_POOL_SIZE", "5");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// How a command that ran to the end ended.
pub struct Exit {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Exit {
    pub fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Run `cmd` to the end, off the async threads.
async fn run(mut cmd: Command) -> Exit {
    let out = tokio::task::spawn_blocking(move || cmd.output())
        .await
        .unwrap()
        .expect("run jamjet-server");
    Exit {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

// ── Engines ──────────────────────────────────────────────────────────────────

/// A running `jamjet-server`, killed when dropped.
pub struct Engine {
    child: Option<Child>,
    addr: String,
    /// Everything the process printed, over every start.
    log: Arc<Mutex<String>>,
    dir: PathBuf,
    url: String,
    env: Vec<(String, String)>,
    /// The threads that copy the process's stdout and stderr into `log`.
    readers: Vec<std::thread::JoinHandle<()>>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        // A case that is failing says what the engine's own threads panicked
        // on, if they did: a call that got no answer may be one of them.
        if std::thread::panicking() {
            for panic in self.panics() {
                eprintln!("the engine on {} panicked: {panic}", self.addr);
            }
            // And the end of what it said, for a case that failed on time.
            let log = self.log();
            let lines: Vec<&str> = log.lines().collect();
            for line in &lines[lines.len().saturating_sub(12)..] {
                let said: String = line.chars().take(300).collect();
                eprintln!("the engine on {}: {said}", self.addr);
            }
        }
        self.kill();
    }
}

/// The variable that arms a fault point in an engine.
pub const CRASH_AT: &str = "JAMJET_TEST_CRASH_AT";

impl Engine {
    /// Start an engine on `store` with its scheduler, embedded workers and
    /// the scheduler's debug lines (a stale pass is one), plus `env`, and
    /// wait until it listens.
    pub async fn start(store: &TestStore, env: &[(&str, &str)]) -> Engine {
        let mut all: Vec<(String, String)> = [
            ("JAMJET_BIND", "127.0.0.1"),
            ("JAMJET_PORT", "0"),
            ("JAMJET_EMBED_WORKERS", "1"),
            ("RUST_LOG", "info,jamjet_scheduler=debug"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        all.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let mut engine = Engine {
            child: None,
            addr: String::new(),
            log: Arc::new(Mutex::new(String::new())),
            dir: store.dir.clone(),
            url: store.url.clone(),
            env: all,
            readers: Vec::new(),
        };
        engine.spawn().await;
        engine
    }

    async fn spawn(&mut self) {
        let from = self.log.lock().unwrap().len();
        let mut child = command(&self.dir, &self.url, &[], &self.env)
            .spawn()
            .expect("start jamjet-server");
        for pipe in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let log = self.log.clone();
            self.readers.push(std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(pipe).lines().map_while(Result::ok) {
                    let mut log = log.lock().unwrap();
                    log.push_str(&line);
                    log.push('\n');
                }
            }));
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let said = self.log.lock().unwrap()[from..].to_string();
            if let Some(rest) = said.split("listening on ").nth(1) {
                self.addr = rest
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ':')
                    .collect();
                self.child = Some(child);
                return;
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!("the engine exited before it listened ({status}): {said}");
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the engine never listened: {said}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Whether the process is running, as far as the harness knows: it was
    /// started and was not seen to exit or killed since.
    pub fn is_running(&self) -> bool {
        self.child.is_some()
    }

    /// `http://<address>` of the running engine. It changes on a restart.
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn log(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    /// How many times `needle` appears in what the engine printed.
    pub fn count(&self, needle: &str) -> usize {
        self.log.lock().unwrap().matches(needle).count()
    }

    /// What the process printed for each panic of one of its threads: the
    /// line that says where, and the message under it. Its stdout and stderr
    /// are read apart, so the log lines in between (JSON) are skipped.
    pub fn panics(&self) -> Vec<String> {
        let log = self.log.lock().unwrap();
        let lines: Vec<&str> = log.lines().collect();
        let mut panics = Vec::new();
        for (at, line) in lines.iter().enumerate() {
            if !line.contains("panicked at") {
                continue;
            }
            let mut said = Vec::new();
            for next in lines[at..].iter().filter(|l| !l.starts_with('{')).take(6) {
                said.push(*next);
                if next.starts_with("note:") {
                    break;
                }
            }
            panics.push(said.join("\n"));
        }
        panics
    }

    /// Fails the case, with the text of each panic, if a thread of the engine
    /// has panicked. A thread that panics while it serves a request drops the
    /// request, and the caller sees only that no answer came.
    pub fn assert_no_panics(&self) {
        let panics = self.panics();
        assert!(
            panics.is_empty(),
            "the engine on {} panicked on {} thread(s):\n{}",
            self.addr,
            panics.len(),
            panics.join("\n")
        );
    }

    /// SIGKILL, and wait for the process to go. Then, unless the case is
    /// already failing, fail it if a thread of the engine had panicked: the
    /// last lines are read first, so a panic just before the kill is counted.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // The readers end when the process's pipes close. Wait for them, but
        // not for a pipe that something the process started holds open.
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.readers.iter().any(|r| !r.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.readers.retain(|r| !r.is_finished());
        if !std::thread::panicking() {
            self.assert_no_panics();
        }
    }

    /// Start the engine again on the same store with the same settings,
    /// without a fault point armed.
    pub async fn restart(&mut self) {
        self.kill();
        self.env.retain(|(k, _)| k != CRASH_AT);
        self.spawn().await;
    }

    /// How the process ended, if it ends within `within`.
    pub async fn exited(&mut self, within: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + within;
        loop {
            let child = self.child.as_mut().expect("the engine was started");
            if let Some(status) = child.try_wait().unwrap() {
                self.child = None;
                // Let the readers take the last lines.
                tokio::time::sleep(Duration::from_millis(200)).await;
                return Some(status);
            }
            if Instant::now() > deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The engine aborted at fault point `point` (`label#step`) within
    /// `within`: SIGABRT, with the point's message on stderr.
    pub async fn died_at(&mut self, point: &str, within: Duration) {
        let status = self.exited(within).await.unwrap_or_else(|| {
            panic!(
                "the engine did not die at {point} within {within:?}: {}",
                self.log()
            )
        });
        assert_eq!(
            status.signal(),
            Some(SIGABRT),
            "the engine ended by {status}, not by an abort at {point}: {}",
            self.log()
        );
        let said = format!("fault point {point}: abort");
        assert!(
            self.log().contains(&said),
            "no {said:?} on stderr: {}",
            self.log()
        );
    }
}

// ── HTTP ─────────────────────────────────────────────────────────────────────

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}

/// `method base+path` with `token` and a JSON `body`: the status and the
/// JSON answer (`Null` for one that is not JSON). An error when no answer
/// came, as from an engine that died while it handled the call.
pub async fn call(
    client: &reqwest::Client,
    base: &str,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> Result<(u16, Value), reqwest::Error> {
    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
    let mut req = client
        .request(method, format!("{base}{path}"))
        .bearer_auth(token);
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    let text = resp.text().await?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

/// As [`call`], for a call that must be answered.
pub async fn must(
    client: &reqwest::Client,
    base: &str,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    call(client, base, method, path, token, body)
        .await
        .unwrap_or_else(|e| panic!("{method} {path} on {base}: {e}"))
}

// ── Workflows ────────────────────────────────────────────────────────────────

fn python_step(id: &str) -> Value {
    json!({
        "id": id,
        "kind": { "type": "python_fn", "module": "tests.steps", "function": id, "output_schema": "" },
        "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
    })
}

fn workflow(id: &str, start: &str, nodes: Value, edges: Vec<Value>) -> Value {
    json!({
        "workflow_id": id, "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": start,
        "nodes": nodes, "edges": edges,
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

/// `approve` (a `human_approval` node the engine runs, with a deadline of
/// `timeout_secs` if given), then `work` (a `python_fn` node a worker claims
/// over HTTP), then the end.
pub fn approve_then_work(id: &str, timeout_secs: Option<u64>) -> Value {
    workflow(
        id,
        "approve",
        json!({
            "approve": {
                "id": "approve",
                "kind": {
                    "type": "human_approval", "description": "Go ahead?",
                    "timeout_secs": timeout_secs, "fallback_node": null
                },
                "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
            },
            "work": python_step("work"),
        }),
        vec![
            json!({ "from": "approve", "to": "work", "condition": null }),
            json!({ "from": "work", "to": "end", "condition": null }),
        ],
    )
}

/// `work` (a `python_fn` node a worker claims over HTTP), then the end.
pub fn work_only(id: &str) -> Value {
    workflow(
        id,
        "work",
        json!({ "work": python_step("work") }),
        vec![json!({ "from": "work", "to": "end", "condition": null })],
    )
}

pub async fn register(client: &reqwest::Client, base: &str, token: &str, ir: &Value) {
    let (st, body) = must(
        client,
        base,
        "POST",
        "/workflows",
        token,
        Some(json!({ "ir": ir })),
    )
    .await;
    assert!((200..300).contains(&st), "register: {st} {body}");
}

/// Start a run of `workflow_id`; its id as the API gives it (`exec_...`).
pub async fn start_run(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    workflow_id: &str,
) -> String {
    let (st, body) = must(
        client,
        base,
        "POST",
        "/executions",
        token,
        Some(json!({ "workflow_id": workflow_id, "workflow_version": "0.1.0", "input": {} })),
    )
    .await;
    assert!((200..300).contains(&st), "start: {st} {body}");
    body["execution_id"].as_str().unwrap().to_string()
}

/// A run's id as the store writes it, from the API's `exec_<hex>`.
pub fn db_id(api_id: &str) -> String {
    let hex = api_id.strip_prefix("exec_").unwrap_or(api_id);
    uuid::Uuid::parse_str(hex).unwrap().to_string()
}

/// The pending approvals the engine lists, oldest first.
pub async fn pending(client: &reqwest::Client, base: &str, token: &str) -> Vec<Value> {
    match call(client, base, "GET", "/approvals?limit=200", token, None).await {
        Ok((200, body)) => body["approvals"].as_array().cloned().unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The pending approval of `run` (an API id), once the engine lists it.
pub async fn wait_for_request(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    run: &str,
    within: Duration,
) -> Value {
    let deadline = Instant::now() + within;
    loop {
        if let Some(row) = pending(client, base, token)
            .await
            .into_iter()
            .find(|row| row["execution_id"] == run)
        {
            return row;
        }
        assert!(
            Instant::now() < deadline,
            "run {run} raised no approval request within {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The body that approves a listed request.
pub fn approval(row: &Value) -> Value {
    json!({
        "decision": "approved",
        "node_id": row["node_id"],
        "action_digest": row["action_digest"],
        "request_sequence": row["request_sequence"],
    })
}

/// Approve a listed request through `base`.
pub async fn approve(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    row: &Value,
) -> Result<(u16, Value), reqwest::Error> {
    let run = row["execution_id"].as_str().unwrap();
    call(
        client,
        base,
        "POST",
        &format!("/executions/{run}/approve"),
        token,
        Some(approval(row)),
    )
    .await
}

/// A claimed item, as a worker holds it.
#[derive(Debug, Clone)]
pub struct Claimed {
    pub id: String,
    pub run: String,
    pub fence: i64,
    pub key: Value,
}

/// Claim the next `python_fn` item through `base` with a lease of
/// `lease_secs`.
pub async fn claim(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    worker: &str,
    lease_secs: u64,
) -> Result<(u16, Option<Claimed>), reqwest::Error> {
    let (st, body) = call(
        client,
        base,
        "POST",
        "/work-items/claim",
        token,
        Some(
            json!({ "worker_id": worker, "queue_types": [PYTHON_QUEUE], "lease_secs": lease_secs }),
        ),
    )
    .await?;
    if st != 200 || body["claimed"] != true {
        return Ok((st, None));
    }
    let item = &body["work_item"];
    Ok((
        st,
        Some(Claimed {
            id: item["id"].as_str().unwrap().to_string(),
            run: item["execution_id"].as_str().unwrap().to_string(),
            fence: item["lease_fence"].as_i64().unwrap(),
            key: item["idempotency_key"].clone(),
        }),
    ))
}

pub async fn heartbeat(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    worker: &str,
    item: &Claimed,
) -> Result<(u16, Value), reqwest::Error> {
    call(
        client,
        base,
        "POST",
        &format!("/work-items/{}/heartbeat", item.id),
        token,
        Some(json!({ "worker_id": worker, "lease_fence": item.fence })),
    )
    .await
}

/// Complete `item` with its fence.
pub async fn complete(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    item: &Claimed,
) -> Result<(u16, Value), reqwest::Error> {
    call(
        client,
        base,
        "POST",
        &format!("/work-items/{}/complete", item.id),
        token,
        Some(json!({
            "output": { "done": true },
            "state_patch": {},
            "duration_ms": 1,
            "lease_fence": item.fence,
            "idempotency_key": item.key,
        })),
    )
    .await
}

/// Report `item` failed, with its fence.
pub async fn fail(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    item: &Claimed,
) -> Result<(u16, Value), reqwest::Error> {
    call(
        client,
        base,
        "POST",
        &format!("/work-items/{}/fail", item.id),
        token,
        Some(json!({ "error": "the step failed", "lease_fence": item.fence })),
    )
    .await
}

// ── Workers ──────────────────────────────────────────────────────────────────

/// The status codes the harness's own calls got, by code, each with the
/// first answer seen with that code.
#[derive(Clone, Default)]
pub struct Answers(Arc<Mutex<BTreeMap<u16, (usize, String)>>>);

impl Answers {
    pub fn note(&self, what: &str, code: u16, body: &Value) {
        let mut seen = self.0.lock().unwrap();
        let entry = seen
            .entry(code)
            .or_insert_with(|| (0, format!("{what}: {body}")));
        entry.0 += 1;
    }

    /// Note an answer, if one came.
    pub fn note_answer(&self, what: &str, result: &Result<(u16, Value), reqwest::Error>) {
        if let Ok((code, body)) = result {
            self.note(what, *code, body);
        }
    }

    pub fn count(&self, code: u16) -> usize {
        self.0.lock().unwrap().get(&code).map_or(0, |(n, _)| *n)
    }

    pub fn summary(&self) -> String {
        format!("{:?}", self.0.lock().unwrap())
    }
}

/// The addresses a worker spreads its calls over, updated when an engine
/// restarts.
pub type Bases = Arc<Mutex<Vec<String>>>;

pub fn bases(engines: &[&Engine]) -> Bases {
    Arc::new(Mutex::new(engines.iter().map(|e| e.base()).collect()))
}

/// A worker over HTTP for the `python_fn` queue, until `stop`: it claims
/// through each engine in turn with a 30 s lease, heartbeats through the
/// engine it claimed from, and completes through the next one, with the
/// fence. Answers to its claims, heartbeats and completions go to
/// `answers`; a completion refused with anything but 200 fails the test.
pub fn worker(
    client: reqwest::Client,
    bases: Bases,
    token: String,
    name: &'static str,
    answers: Answers,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<usize> {
    tokio::spawn(async move {
        let mut completed = 0;
        let mut turn = 0usize;
        while !stop.load(Ordering::SeqCst) {
            let now: Vec<String> = bases.lock().unwrap().clone();
            let at = &now[turn % now.len()];
            let next = &now[(turn + 1) % now.len()];
            turn += 1;
            let claimed = claim(&client, at, &token, name, 30).await;
            if let Ok((code, _)) = &claimed {
                answers.note("claim", *code, &Value::Null);
            }
            let Ok((_, Some(item))) = claimed else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            let beat = heartbeat(&client, at, &token, name, &item).await;
            answers.note_answer("heartbeat", &beat);
            loop {
                let done = complete(&client, next, &token, &item).await;
                answers.note_answer("complete", &done);
                match done {
                    Ok((200, _)) => {
                        completed += 1;
                        break;
                    }
                    // The store was unavailable; the lease still holds.
                    Ok((503, _)) | Err(_) => {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    Ok((st, body)) => panic!("completion of {} refused: {st} {body}", item.id),
                }
            }
        }
        completed
    })
}
