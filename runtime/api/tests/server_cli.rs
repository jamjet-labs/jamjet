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
    // `dotenvy` walks up from the working directory to the first `.env`; an
    // empty one here stops it reading a stray one in a parent.
    std::fs::write(dir.join(".env"), "").unwrap();
    dir
}

/// A migrated database in `dir`, as a server that has started once leaves it:
/// the state schema and the audit log.
async fn server_database(dir: &std::path::Path) -> (String, Arc<SqliteBackend>) {
    let url = format!("sqlite://{}", dir.join("runtime.db").display());
    let backend = Arc::new(SqliteBackend::open(&url).await.unwrap());
    jamjet_audit::SqliteAuditBackend::new(backend.pool())
        .migrate()
        .await
        .unwrap();
    (url, backend)
}

/// The audit log's entries, oldest first (the order `GET /audit` lists them
/// in, reversed).
async fn audit_entries(backend: &SqliteBackend) -> Vec<jamjet_audit::AuditLogEntry> {
    use jamjet_audit::AuditBackend;
    let q = jamjet_audit::AuditQuery {
        limit: 1000,
        ..Default::default()
    };
    let mut entries = jamjet_audit::SqliteAuditBackend::new(backend.pool())
        .query(&q)
        .await
        .unwrap();
    entries.reverse();
    entries
}

async fn token_rows(backend: &SqliteBackend) -> Vec<(String, Option<String>)> {
    sqlx::query_as("SELECT id, revoked_at FROM api_tokens")
        .fetch_all(&backend.pool())
        .await
        .unwrap()
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
    let (url, backend) = server_database(&dir).await;
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
    let (url, backend) = server_database(&dir).await;
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
    assert!(token_rows(&backend).await.is_empty(), "nothing is written");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A token for a tenant that exists is stored in that tenant.
#[tokio::test]
async fn token_create_issues_a_token_for_an_existing_tenant() {
    let dir = temp_dir();
    let (url, backend) = server_database(&dir).await;
    let now = chrono::Utc::now();
    backend
        .for_tenant(jamjet_state::TenantId::from("acme"))
        .create_tenant(jamjet_state::Tenant {
            id: "acme".into(),
            name: "acme".into(),
            status: jamjet_state::TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let mut child = server(
        &dir,
        &["token", "create", "--role", "viewer", "--tenant", "acme"],
        &[("DATABASE_URL", &url)],
    )
    .spawn()
    .unwrap();
    let status = wait_exit(&mut child).expect("exits");
    let stderr = read_all(child.stderr.take());
    assert!(status.success(), "{stderr}");
    assert!(
        stderr.contains("single-tenant"),
        "the tenant caveat is shown: {stderr}"
    );
    let token = read_all(child.stdout.take());
    let info = backend
        .validate_token(token.trim())
        .await
        .unwrap()
        .expect("validates");
    assert_eq!(info.tenant_id, "acme");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The token commands open the server's database; they do not create one.
/// A mistyped path used to get a fresh database and a token no server reads.
#[test]
fn token_create_refuses_a_database_that_does_not_exist() {
    let dir = temp_dir();
    let missing = dir.join("nope").join("runtime.db");
    let url = format!("sqlite://{}", missing.display());
    let mut child = server(
        &dir,
        &["token", "create", "--role", "viewer"],
        &[("DATABASE_URL", &url)],
    )
    .spawn()
    .unwrap();
    let status = wait_exit(&mut child).expect("exits");
    assert!(!status.success());
    assert!(read_all(child.stdout.take()).trim().is_empty());
    assert!(!missing.exists(), "no database is created");
    for url in ["sqlite::memory:", "sqlite://"] {
        let mut child = server(
            &dir,
            &["token", "create", "--role", "viewer"],
            &[("DATABASE_URL", url)],
        )
        .spawn()
        .unwrap();
        let status = wait_exit(&mut child).expect("exits");
        assert!(!status.success(), "{url}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `token revoke <id>` revokes the token; it is refused from then on, and a
/// second revoke reports that there is nothing to revoke.
#[tokio::test]
async fn token_revoke_revokes_the_token() {
    let dir = temp_dir();
    let (url, backend) = server_database(&dir).await;
    let mut create = server(
        &dir,
        &["token", "create", "--role", "developer"],
        &[("DATABASE_URL", &url)],
    )
    .spawn()
    .unwrap();
    assert!(wait_exit(&mut create).expect("exits").success());
    let token = read_all(create.stdout.take()).trim().to_string();
    let id = token_rows(&backend).await[0].0.clone();
    assert!(backend.validate_token(&token).await.unwrap().is_some());

    let mut revoke = server(&dir, &["token", "revoke", &id], &[("DATABASE_URL", &url)])
        .spawn()
        .unwrap();
    let status = wait_exit(&mut revoke).expect("exits");
    assert!(status.success(), "{}", read_all(revoke.stderr.take()));
    assert!(
        backend.validate_token(&token).await.unwrap().is_none(),
        "a revoked token is refused"
    );

    let mut again = server(&dir, &["token", "revoke", &id], &[("DATABASE_URL", &url)])
        .spawn()
        .unwrap();
    assert!(!wait_exit(&mut again).expect("exits").success());
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

/// Dev mode on loopback answers only requests addressed to the local
/// machine, so a page that rebinds its own host name to 127.0.0.1 cannot
/// drive it.
#[test]
fn dev_mode_refuses_requests_for_another_host_name() {
    let dir = temp_dir();
    let mut child = server(
        &dir,
        &[],
        &[
            ("JAMJET_DEV_MODE", "1"),
            ("STORAGE_BACKEND", "memory"),
            ("JAMJET_BIND", "127.0.0.1"),
            ("JAMJET_PORT", "0"),
        ],
    )
    .spawn()
    .unwrap();
    let mut addr = None;
    for line in BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map_while(Result::ok)
    {
        if let Some(rest) = line.split("listening on ").nth(1) {
            let a: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ':')
                .collect();
            addr = Some(a);
            break;
        }
    }
    let addr = addr.expect("the server reports where it listens");
    let port = addr.rsplit(':').next().unwrap().to_string();
    let status_line = |host: &str| -> String {
        use std::io::Write;
        let mut conn = std::net::TcpStream::connect(&addr).unwrap();
        write!(
            conn,
            "GET /health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        conn.read_to_string(&mut response).unwrap();
        response.lines().next().unwrap_or("").to_string()
    };
    let rebound = status_line(&format!("rebound.evil.example:{port}"));
    let local = status_line(&format!("localhost:{port}"));
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(rebound.contains("403"), "{rebound}");
    assert!(local.contains("200"), "{local}");
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

/// Issuing and revoking a token is on the audit record: separation of duties
/// is keyed on the token id, so a reviewer token minted for oneself must show.
/// Both entries are sealed into the chain the server writes, and with the
/// server's key they verify.
#[tokio::test]
async fn token_create_and_revoke_are_sealed_into_the_audit_log() {
    let dir = temp_dir();
    let (url, backend) = server_database(&dir).await;
    let key = (
        "JAMJET_AUDIT_SIGNING_KEY",
        "a-real-secret-for-this-test-only",
    );
    let mut create = server(
        &dir,
        &[
            "token", "create", "--role", "reviewer", "--name", "me-again",
        ],
        &[("DATABASE_URL", &url), key],
    )
    .spawn()
    .unwrap();
    assert!(
        wait_exit(&mut create).expect("exits").success(),
        "{}",
        read_all(create.stderr.take())
    );
    let id = token_rows(&backend).await[0].0.clone();
    let mut revoke = server(
        &dir,
        &["token", "revoke", &id],
        &[("DATABASE_URL", &url), key],
    )
    .spawn()
    .unwrap();
    assert!(
        wait_exit(&mut revoke).expect("exits").success(),
        "{}",
        read_all(revoke.stderr.take())
    );

    let entries = audit_entries(&backend).await;
    let kinds: Vec<&str> = entries.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(kinds, ["token_issued", "token_revoked"]);
    for e in &entries {
        assert_eq!(e.raw_event["token_id"], id.as_str());
        assert_eq!(e.raw_event["principal"], format!("token:{id}"));
        assert_eq!(e.raw_event["role"], "reviewer");
        assert_eq!(e.raw_event["name"], "me-again");
        assert_eq!(e.tenant_id, "default");
    }
    let signer = jamjet_audit::AuditSigner::new(key.1.as_bytes().to_vec());
    jamjet_audit::verify_chain(&entries, &signer).expect("the chain verifies");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A token the audit log cannot record is not issued.
#[tokio::test]
async fn a_token_the_audit_log_cannot_record_is_not_issued() {
    let dir = temp_dir();
    let (url, backend) = server_database(&dir).await;
    sqlx::query("DROP TABLE audit_log")
        .execute(&backend.pool())
        .await
        .unwrap();
    let mut create = server(
        &dir,
        &["token", "create", "--role", "operator"],
        &[("DATABASE_URL", &url)],
    )
    .spawn()
    .unwrap();
    let status = wait_exit(&mut create).expect("exits");
    let stderr = read_all(create.stderr.take());
    assert!(!status.success(), "{stderr}");
    assert!(
        read_all(create.stdout.take()).trim().is_empty(),
        "no token printed"
    );
    let rows = token_rows(&backend).await;
    assert!(
        rows.iter().all(|(_, revoked)| revoked.is_some()),
        "an unrecorded token is revoked: {rows:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run in a directory with no database, the token commands fail and leave
/// no `.jamjet` behind.
#[test]
fn token_commands_do_not_create_a_jamjet_directory() {
    let dir = temp_dir();
    for args in [
        &["token", "create", "--role", "viewer"][..],
        &["token", "revoke", "some-id"][..],
    ] {
        let mut child = server(&dir, args, &[]).spawn().unwrap();
        let status = wait_exit(&mut child).expect("exits");
        assert!(!status.success(), "{args:?}");
        assert!(!dir.join(".jamjet").exists(), "{args:?} created .jamjet");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A switch set to something that is not a boolean stops the server, naming
/// it. `JAMJET_ALLOW_SELF_APPROVAL=enabled` used to read as off and serve.
#[test]
fn a_switch_that_is_not_a_boolean_stops_startup() {
    for name in [
        "JAMJET_ALLOW_SELF_APPROVAL",
        "JAMJET_TENANT_TOKENS_AS_DEFAULT",
        "JAMJET_AUDIT_ALLOW_INSECURE_KEY",
        "JAMJET_DEV_MODE",
    ] {
        let dir = temp_dir();
        let mut child = server(
            &dir,
            &[],
            &[
                (name, "enabled"),
                ("STORAGE_BACKEND", "memory"),
                ("JAMJET_BIND", "127.0.0.1"),
                ("JAMJET_PORT", "0"),
            ],
        )
        .spawn()
        .unwrap();
        let status = wait_exit(&mut child).expect("must not serve");
        let stderr = read_all(child.stderr.take());
        assert!(!status.success(), "{name}");
        assert!(stderr.contains(name), "{name}: {stderr}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// `JAMJET_DEV_MODE=yes` turns dev mode on, as `1` and `true` do. It used to
/// read as off.
#[test]
fn dev_mode_yes_is_dev_mode() {
    let log = startup_log(&[("JAMJET_DEV_MODE", "yes")]);
    // Dev mode logs in colour; drop the escape sequences.
    let mut plain = String::new();
    let mut chars = log.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(c);
        }
    }
    assert!(
        plain.contains("dev_mode: true") || plain.contains("dev_mode=true"),
        "{plain}"
    );
}

/// A dev-mode server's address and a raw `GET /health` (or body-less `POST`)
/// to it with these headers, returning the status line.
fn dev_status(addr: &str, method: &str, headers: &[(&str, &str)]) -> String {
    use std::io::Write;
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    let path = if method == "POST" {
        "/executions/exec_00000000000000000000000000000000/cancel"
    } else {
        "/health"
    };
    let mut req = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\nContent-Length: 0\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    conn.write_all(req.as_bytes()).unwrap();
    let mut response = String::new();
    conn.read_to_string(&mut response).unwrap();
    response.lines().next().unwrap_or("").to_string()
}

/// Start a dev-mode server with `env`; return it and a loopback address to
/// reach it on.
fn dev_server(dir: &PathBuf, env: &[(&str, &str)]) -> (Child, String) {
    let mut base = vec![
        ("JAMJET_DEV_MODE", "1"),
        ("STORAGE_BACKEND", "memory"),
        ("JAMJET_PORT", "0"),
    ];
    base.extend_from_slice(env);
    let mut child = server(dir, &[], &base).spawn().unwrap();
    let mut port = None;
    for line in BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map_while(Result::ok)
    {
        if let Some(rest) = line.split("listening on ").nth(1) {
            let a: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ':')
                .collect();
            port = a.rsplit(':').next().map(str::to_string);
            break;
        }
    }
    let port = port.expect("the server reports where it listens");
    (child, format!("127.0.0.1:{port}"))
}

/// `JAMJET_DEV_MODE_ALLOW_REMOTE` allows a remote bind; it does not turn the
/// Host check off. In Docker the bind is 0.0.0.0, and without the check a
/// rebound page drove a dev-mode runtime with no token. Before this the
/// rebound host below got 200.
#[test]
fn a_remote_dev_bind_still_checks_the_host() {
    let dir = temp_dir();
    let (mut child, addr) = dev_server(
        &dir,
        &[
            ("JAMJET_BIND", "0.0.0.0"),
            ("JAMJET_DEV_MODE_ALLOW_REMOTE", "1"),
        ],
    );
    let port = addr.rsplit(':').next().unwrap().to_string();
    let rebound = dev_status(
        &addr,
        "GET",
        &[("Host", &format!("rebound.evil.example:{port}"))],
    );
    let docker = dev_status(
        &addr,
        "GET",
        &[("Host", &format!("host.docker.internal:{port}"))],
    );
    let local = dev_status(&addr, "GET", &[("Host", &format!("localhost:{port}"))]);
    let _ = child.kill();
    let _ = child.wait();
    assert!(rebound.contains("403"), "{rebound}");
    assert!(docker.contains("403"), "{docker}");
    assert!(local.contains("200"), "{local}");

    // A name listed in JAMJET_DEV_ALLOWED_HOSTS is answered.
    let (mut child, addr) = dev_server(
        &dir,
        &[
            ("JAMJET_BIND", "0.0.0.0"),
            ("JAMJET_DEV_MODE_ALLOW_REMOTE", "1"),
            ("JAMJET_DEV_ALLOWED_HOSTS", "host.docker.internal"),
        ],
    );
    let port = addr.rsplit(':').next().unwrap().to_string();
    let docker = dev_status(
        &addr,
        "GET",
        &[("Host", &format!("host.docker.internal:{port}"))],
    );
    let rebound = dev_status(
        &addr,
        "GET",
        &[("Host", &format!("rebound.evil.example:{port}"))],
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(docker.contains("200"), "{docker}");
    assert!(rebound.contains("403"), "{rebound}");
}

/// A tunnel that rewrites `Host` to localhost (ngrok `--host-header=rewrite`,
/// cloudflared) still sends forwarding headers; a page of another site sends
/// its `Origin`. Dev mode refuses both.
#[test]
fn dev_mode_refuses_tunnels_and_cross_site_requests() {
    let dir = temp_dir();
    let (mut child, addr) = dev_server(&dir, &[("JAMJET_BIND", "127.0.0.1")]);
    let port = addr.rsplit(':').next().unwrap().to_string();
    let host = format!("localhost:{port}");
    let tunnel = dev_status(
        &addr,
        "GET",
        &[("Host", &host), ("X-Forwarded-Host", "abc.ngrok.app")],
    );
    let cross = dev_status(
        &addr,
        "POST",
        &[("Host", &host), ("Origin", "https://evil.example")],
    );
    let fetch = dev_status(
        &addr,
        "POST",
        &[("Host", &host), ("Sec-Fetch-Site", "cross-site")],
    );
    let local = dev_status(&addr, "GET", &[("Host", &host)]);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(tunnel.contains("403"), "{tunnel}");
    assert!(cross.contains("403"), "{cross}");
    assert!(fetch.contains("403"), "{fetch}");
    assert!(local.contains("200"), "{local}");
}
