//! `jamjet-server` on a serverless container platform: the platform keeps
//! files in memory and loses them when an instance stops, so a SQLite file is
//! refused before anything is opened or created. These run the real binary.
#![cfg(unix)]

use jamjet_state::SqliteBackend;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_jamjet-server");
const WAIT: Duration = Duration::from_secs(30);

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jjplat-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    // An empty `.env` stops `dotenvy` reading a stray one in a parent.
    std::fs::write(dir.join(".env"), "").unwrap();
    dir
}

/// The server binary in `dir` with a cleared environment, port 0 and `env`.
fn server(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.current_dir(dir).env_clear().args(args);
    for key in ["PATH", "HOME", "TMPDIR"] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    cmd.env("JAMJET_BIND", "127.0.0.1").env("JAMJET_PORT", "0");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn read_all(pipe: Option<impl Read>) -> String {
    let mut out = String::new();
    if let Some(mut p) = pipe {
        let _ = p.read_to_string(&mut out);
    }
    out
}

/// The exit status within [`WAIT`]. A server that is still running is
/// killed, and `None` is returned.
fn wait_exit(child: &mut Child) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Runs the server to its end: its status, stdout and stderr.
fn run(
    dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> (Option<std::process::ExitStatus>, String, String) {
    let mut child = server(dir, args, env).spawn().unwrap();
    let status = wait_exit(&mut child);
    (
        status,
        read_all(child.stdout.take()),
        read_all(child.stderr.take()),
    )
}

#[test]
fn the_server_refuses_sqlite_and_creates_no_file() {
    let dir = temp_dir();
    let (status, stdout, stderr) = run(&dir, &[], &[("K_SERVICE", "svc")]);
    let status = status.expect("the server must refuse and exit, not serve");
    assert!(!status.success(), "{stdout}\n{stderr}");
    let text = format!("{stdout}\n{stderr}");
    assert!(text.contains("DATABASE_URL"), "{text}");
    assert!(text.contains("JAMJET_ALLOW_EPHEMERAL_STORE"), "{text}");
    assert!(
        !dir.join(".jamjet").exists(),
        "a .jamjet directory was made"
    );
    let mut left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, vec![".env".to_string()], "files left behind");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_database_url_file_is_refused_too_and_not_created() {
    let dir = temp_dir();
    let url = format!("sqlite://{}", dir.join("runtime.db").display());
    let (status, stdout, stderr) = run(&dir, &[], &[("K_SERVICE", "svc"), ("DATABASE_URL", &url)]);
    let status = status.expect("the server must refuse and exit, not serve");
    assert!(!status.success(), "{stdout}\n{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("JAMJET_ALLOW_EPHEMERAL_STORE"),
        "{stdout}{stderr}"
    );
    assert!(!dir.join("runtime.db").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A flag set to `0` is off: it must not count as present.
#[test]
fn the_allow_flag_set_to_zero_still_refuses() {
    let dir = temp_dir();
    let (status, stdout, stderr) = run(
        &dir,
        &[],
        &[("K_SERVICE", "svc"), ("JAMJET_ALLOW_EPHEMERAL_STORE", "0")],
    );
    let status = status.expect("the server must refuse and exit, not serve");
    assert!(!status.success(), "{stdout}\n{stderr}");
    assert!(!dir.join(".jamjet").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_allow_flag_lets_the_server_start_with_a_warning() {
    let dir = temp_dir();
    let mut child = server(
        &dir,
        &[],
        &[
            ("K_SERVICE", "svc"),
            ("JAMJET_ALLOW_EPHEMERAL_STORE", "1"),
            ("RUST_LOG", "info"),
        ],
    )
    .spawn()
    .unwrap();
    // Still serving after a few seconds: it did not refuse.
    std::thread::sleep(Duration::from_secs(3));
    let ended = child.try_wait().unwrap();
    let _ = child.kill();
    let _ = child.wait();
    let stderr = read_all(child.stderr.take());
    let stdout = read_all(child.stdout.take());
    assert!(ended.is_none(), "it exited: {stdout}\n{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("JAMJET_ALLOW_EPHEMERAL_STORE"),
        "no warning: {stdout}{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_allow_flag_is_a_strict_boolean() {
    let dir = temp_dir();
    let (status, stdout, stderr) = run(
        &dir,
        &[],
        &[
            ("K_SERVICE", "svc"),
            ("JAMJET_ALLOW_EPHEMERAL_STORE", "maybe"),
        ],
    );
    let status = status.expect("a bad flag must stop the server");
    assert!(!status.success(), "{stdout}\n{stderr}");
    assert!(
        format!("{stdout}{stderr}").contains("JAMJET_ALLOW_EPHEMERAL_STORE"),
        "{stdout}{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn token_create_is_not_guarded() {
    let dir = temp_dir();
    let url = format!("sqlite://{}", dir.join("runtime.db").display());
    let backend = SqliteBackend::open(&url).await.unwrap();
    jamjet_audit::SqliteAuditBackend::new(backend.pool())
        .migrate()
        .await
        .unwrap();
    let (status, stdout, stderr) = run(
        &dir,
        &["token", "create", "--role", "viewer"],
        &[("K_SERVICE", "svc"), ("DATABASE_URL", &url)],
    );
    let status = status.expect("`token create` must exit");
    assert!(status.success(), "stderr: {stderr}");
    assert!(stdout.trim().starts_with("jj_"), "{stdout:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
