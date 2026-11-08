//! `jamjet-server` as a process: the `token create` command and the startup
//! checks. These run the real binary, because the behaviour under test is
//! what `main` does with its arguments and environment.
//!
//! Every run gets its own temp directory as working directory (so no `.env`
//! and no `.jamjet/runtime.db` of a developer's is read), a cleared
//! environment, and port 0 when it would listen.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditEnricher, NoopAuditBackend};
use jamjet_state::{SqliteBackend, StateBackend};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

const BIN: &str = env!("CARGO_BIN_EXE_jamjet-server");
const WAIT: Duration = Duration::from_secs(30);

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jjcli-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The binary with only `PATH` and `HOME` inherited, run in `dir`.
fn server(dir: &PathBuf, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.args(args).current_dir(dir).env_clear();
    for key in ["PATH", "HOME", "TMPDIR"] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    cmd.env("RUST_LOG", "info");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Wait for `child` to exit; kill it and return `None` if it is still
/// running after [`WAIT`] (a server that should have refused to start).
fn wait_exit(child: &mut Child) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    while start.elapsed() < WAIT {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

fn read_all(r: Option<impl Read>) -> String {
    let mut s = String::new();
    if let Some(mut r) = r {
        r.read_to_string(&mut s).unwrap();
    }
    s
}

fn state_on(backend: Arc<SqliteBackend>) -> AppState {
    let audit: Arc<dyn jamjet_audit::AuditBackend> = Arc::new(NoopAuditBackend);
    let for_fn = backend.clone();
    AppState {
        backend: backend.clone(),
        backend_for_fn: Arc::new(move |t: &jamjet_state::TenantId| {
            Arc::new(for_fn.for_tenant(t.clone())) as Arc<dyn StateBackend>
        }),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        enricher: Arc::new(AuditEnricher::new(Arc::clone(&audit))),
        audit,
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
        approval_policy: Default::default(),
    }
}

/// `token create` prints a token once, stores only its hash, and the token
/// authenticates against a server on that database with the role asked for.
/// Before this command the binary ignored its arguments and served, so this
/// run never exited.
#[tokio::test]
async fn token_create_issues_a_token_the_server_accepts() {
    let dir = temp_dir();
    let url = format!("sqlite://{}", dir.join("runtime.db").display());
    let mut child = server(
        &dir,
        &["token", "create", "--role", "developer", "--name", "ci"],
        &[("DATABASE_URL", &url)],
    )
    .spawn()
    .unwrap();
    let status = wait_exit(&mut child).expect("`token create` must exit, not serve");
    let stdout = read_all(child.stdout.take());
    let stderr = read_all(child.stderr.take());
    assert!(status.success(), "stderr: {stderr}");

    let token = stdout.trim();
    assert_eq!(
        stdout.lines().count(),
        1,
        "stdout is the token alone: {stdout:?}"
    );
    assert!(token.starts_with("jj_") && token.len() == 67, "{token:?}");
    assert!(stderr.contains("shown once"), "{stderr}");

    let backend = Arc::new(SqliteBackend::open(&url).await.unwrap());
    let info = backend
        .validate_token(token)
        .await
        .unwrap()
        .expect("the printed token validates");
    assert_eq!(info.role, "developer");
    assert_eq!(info.name, "ci");
    assert_eq!(info.tenant_id, "default");

    // Only the hash is stored.
    let rows: Vec<(String,)> = sqlx::query_as("SELECT token_hash FROM api_tokens")
        .fetch_all(&backend.pool())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_ne!(rows[0].0, token);
    assert!(
        !rows[0].0.contains(&token[3..]),
        "the token text is not stored"
    );

    // The router with authentication on accepts it, and refuses no token.
    let router = build_router_with_opts(state_on(backend), false);
    let authed = router
        .clone()
        .oneshot(
            Request::get("/executions")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authed.status(), StatusCode::OK);
    let anonymous = router
        .oneshot(Request::get("/executions").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A token for a tenant that does not exist is refused, and nothing is
/// written.
#[tokio::test]
async fn token_create_refuses_an_unknown_tenant() {
    let dir = temp_dir();
    let url = format!("sqlite://{}", dir.join("runtime.db").display());
    let mut child = server(
        &dir,
        &["token", "create", "--role", "viewer", "--tenant", "acme"],
        &[("DATABASE_URL", &url)],
    )
    .spawn()
    .unwrap();
    let status = wait_exit(&mut child).expect("exits");
    let stderr = read_all(child.stderr.take());
    assert!(!status.success());
    assert!(stderr.contains("no tenant"), "{stderr}");
    assert!(read_all(child.stdout.take()).trim().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// With the memory backend no other process can hold a token, so the
/// command refuses instead of printing one that works nowhere.
#[test]
fn token_create_refuses_the_memory_backend() {
    let dir = temp_dir();
    let mut child = server(
        &dir,
        &["token", "create", "--role", "viewer"],
        &[("STORAGE_BACKEND", "memory")],
    )
    .spawn()
    .unwrap();
    let status = wait_exit(&mut child).expect("exits");
    assert!(!status.success());
    assert!(read_all(child.stderr.take()).contains("STORAGE_BACKEND=memory"));
    assert!(!dir.join(".jamjet").exists(), "no database is created");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Dev mode has no authentication, so on 0.0.0.0 it must refuse to start.
/// Before this it served, and the run below only ended at the timeout.
#[test]
fn dev_mode_on_a_remote_bind_refuses_to_start() {
    let dir = temp_dir();
    let mut child = server(
        &dir,
        &[],
        &[
            ("JAMJET_DEV_MODE", "1"),
            ("JAMJET_BIND", "0.0.0.0"),
            ("JAMJET_PORT", "0"),
            ("STORAGE_BACKEND", "memory"),
        ],
    )
    .spawn()
    .unwrap();
    let status = wait_exit(&mut child).expect("dev mode on 0.0.0.0 must not serve");
    let stderr = read_all(child.stderr.take());
    assert!(!status.success());
    assert!(stderr.contains("JAMJET_DEV_MODE_ALLOW_REMOTE"), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// An unknown argument is an error, not ignored: a mistyped command must not
/// start a server.
#[test]
fn an_unknown_argument_does_not_serve() {
    let dir = temp_dir();
    let mut child = server(&dir, &["serve-please"], &[("STORAGE_BACKEND", "memory")])
        .spawn()
        .unwrap();
    let status = wait_exit(&mut child).expect("exits");
    assert_eq!(status.code(), Some(2));
    assert!(read_all(child.stderr.take()).contains("Usage"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Start the server (memory backend, loopback, port 0), read its log until it
/// says where it listens, then stop it. Returns everything it logged.
fn startup_log(env: &[(&str, &str)]) -> String {
    let dir = temp_dir();
    let mut base = vec![
        ("STORAGE_BACKEND", "memory"),
        ("JAMJET_BIND", "127.0.0.1"),
        ("JAMJET_PORT", "0"),
    ];
    base.extend_from_slice(env);
    let mut child = server(&dir, &[], &base).spawn().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    for pipe in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let mut log = String::new();
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                let done = line.contains("listening on");
                log.push_str(&line);
                log.push('\n');
                if done {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        log.contains("listening on"),
        "the server never came up: {log}"
    );
    log
}

/// `JAMJET_EMBED_WORKERS=0` means off. The variable used to be checked for
/// presence only, so `0` started the pool.
#[test]
fn embed_workers_zero_does_not_start_the_pool() {
    let log = startup_log(&[("JAMJET_EMBED_WORKERS", "0")]);
    assert!(!log.contains("Embedded worker pool started"), "{log}");
}

/// The control for the test above: with `1` the pool starts and the log says
/// what started it (it used to say "dev mode" whatever did).
#[test]
fn embed_workers_one_starts_the_pool_and_says_why() {
    let log = startup_log(&[("JAMJET_EMBED_WORKERS", "1")]);
    let line = log
        .lines()
        .find(|l| l.contains("Embedded worker pool started"))
        .unwrap_or_else(|| panic!("{log}"));
    assert!(line.contains("JAMJET_EMBED_WORKERS"), "{line}");
    assert!(!line.contains("dev mode"), "{line}");
}

/// A value that is neither on nor off fails startup instead of being guessed.
#[test]
fn an_unreadable_embed_flag_fails_startup() {
    let dir = temp_dir();
    let mut child = server(
        &dir,
        &[],
        &[
            ("STORAGE_BACKEND", "memory"),
            ("JAMJET_BIND", "127.0.0.1"),
            ("JAMJET_PORT", "0"),
            ("JAMJET_EMBED_WORKERS", "enabled"),
        ],
    )
    .spawn()
    .unwrap();
    let status = wait_exit(&mut child).expect("must not serve");
    assert!(!status.success());
    assert!(read_all(child.stderr.take()).contains("JAMJET_EMBED_WORKERS"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The database URL is not logged at info.
#[test]
fn the_database_url_is_not_logged_at_info() {
    let dir = temp_dir();
    let url = format!("sqlite://{}", dir.join("secret-name.db").display());
    let mut child = server(
        &dir,
        &[],
        &[
            ("DATABASE_URL", &url),
            ("JAMJET_BIND", "127.0.0.1"),
            ("JAMJET_PORT", "0"),
        ],
    )
    .spawn()
    .unwrap();
    // Give it time to open the database and listen, then read what it said.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) && !dir.join("secret-name.db").exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(500));
    let _ = child.kill();
    let _ = child.wait();
    let log = read_all(child.stdout.take()) + &read_all(child.stderr.take());
    assert!(
        dir.join("secret-name.db").exists(),
        "the server opened it: {log}"
    );
    assert!(log.contains("Connecting to state backend"), "{log}");
    assert!(!log.contains("secret-name"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}
