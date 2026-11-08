//! `engram serve --mode rest` as a process: it refuses to listen off
//! loopback without a token, and with one it enforces it. It used to bind
//! 0.0.0.0 with no auth.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_engram");

/// The child, killed when dropped, so a failed assertion does not leave a
/// server running.
struct Engram(Child);

impl Drop for Engram {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl std::ops::Deref for Engram {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for Engram {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

/// Read `child`'s stderr until a line contains `listening on`, then keep
/// draining it on a thread so later writes do not hit a closed pipe.
fn listening_addr(child: &mut Child) -> Option<String> {
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut addr = None;
    for line in lines.by_ref().map_while(Result::ok) {
        if let Some(rest) = line.split("listening on ").nth(1) {
            addr = Some(rest.trim().to_string());
            break;
        }
    }
    std::thread::spawn(move || for _ in lines.map_while(Result::ok) {});
    addr
}

fn engram(env: &[(&str, &str)]) -> Engram {
    let mut cmd = Command::new(BIN);
    cmd.args([
        "serve",
        "--mode",
        "rest",
        "--port",
        "0",
        "--db",
        ":memory:",
        "--llm-provider",
        "mock",
        "--embedding-provider",
        "mock",
    ])
    .env_clear();
    for key in ["PATH", "HOME", "TMPDIR"] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    Engram(
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn rest_off_loopback_without_a_token_refuses_to_start() {
    let mut child = engram(&[("ENGRAM_HOST", "0.0.0.0")]);
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break Some(s);
        }
        if start.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let status = status.unwrap_or_else(|| panic!("it served on 0.0.0.0 with no auth: {stderr}"));
    assert!(!status.success());
    assert!(stderr.contains("ENGRAM_AUTH_TOKEN"), "{stderr}");
}

#[test]
fn rest_with_a_token_refuses_requests_without_it() {
    let token = "3f9a1c0e7b2d4a6f8e1c3b5d7f9a2c4e";
    let mut child = engram(&[("ENGRAM_AUTH_TOKEN", token)]);
    let addr = listening_addr(&mut child).expect("the server reports where it listens");
    assert!(
        addr.starts_with("127.0.0.1:"),
        "loopback by default: {addr}"
    );
    let status_line = |auth: Option<&str>| -> String {
        let mut conn = std::net::TcpStream::connect(&addr).unwrap();
        let header = auth
            .map(|a| format!("Authorization: {a}\r\n"))
            .unwrap_or_default();
        write!(
            conn,
            "GET /v1/memory/stats HTTP/1.1\r\nHost: localhost\r\n{header}Connection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        conn.read_to_string(&mut response).unwrap();
        response.lines().next().unwrap_or("").to_string()
    };
    let without = status_line(None);
    let with = status_line(Some(&format!("Bearer {token}")));
    drop(child);
    assert!(without.contains("401"), "{without}");
    assert!(with.contains("200"), "{with}");
}

/// `ENGRAM_ALLOW_UNAUTHENTICATED` takes yes/no values. clap's plain bool
/// parser refused `1` (the form the README gives) as an invalid value, and
/// `0` must mean no.
#[test]
fn the_allow_unauthenticated_env_takes_yes_and_no() {
    // `=0` off loopback: refused for want of auth, not as a parse error.
    let mut child = engram(&[
        ("ENGRAM_HOST", "0.0.0.0"),
        ("ENGRAM_ALLOW_UNAUTHENTICATED", "0"),
    ]);
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() && start.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let status = child.wait().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(!status.success());
    assert!(stderr.contains("refusing to serve"), "{stderr}");

    // `=1` parses (on loopback, so nothing is exposed) and the server starts.
    let mut child = engram(&[("ENGRAM_ALLOW_UNAUTHENTICATED", "1")]);
    assert!(
        listening_addr(&mut child).is_some(),
        "`=1` must parse and start"
    );
}
