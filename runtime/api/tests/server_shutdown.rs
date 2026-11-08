//! `jamjet-server` stops cleanly on SIGTERM, the signal Docker, Kubernetes
//! and systemd send. It listened for Ctrl-C only, so SIGTERM killed it with
//! work in flight.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[test]
fn sigterm_shuts_the_server_down_cleanly() {
    let dir = std::env::temp_dir().join(format!("jjstop-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_jamjet-server"));
    cmd.current_dir(&dir).env_clear();
    for key in ["PATH", "HOME", "TMPDIR"] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    let mut child = cmd
        .env("RUST_LOG", "info")
        .env("STORAGE_BACKEND", "memory")
        .env("JAMJET_BIND", "127.0.0.1")
        .env("JAMJET_PORT", "0")
        .env("JAMJET_EMBED_WORKERS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let (tx, rx) = mpsc::channel::<String>();
    for pipe in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
    }
    drop(tx);

    let mut log = String::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !log.contains("listening on") {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
            log.push_str(&line);
            log.push('\n');
        }
    }
    assert!(
        log.contains("listening on"),
        "the server never came up: {log}"
    );

    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    while let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
        log.push_str(&line);
        log.push('\n');
    }
    let _ = std::fs::remove_dir_all(&dir);
    let status = status.unwrap_or_else(|| panic!("still running 30 s after SIGTERM: {log}"));
    assert!(
        status.success(),
        "SIGTERM must end in a clean exit, not a kill: {status:?}\n{log}"
    );
    assert!(log.contains("SIGTERM received"), "{log}");
    assert!(log.contains("Shutting down"), "{log}");
}
