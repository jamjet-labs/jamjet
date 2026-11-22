//! `jamjet-server` stops cleanly on SIGTERM, the signal Docker, Kubernetes
//! and systemd send. It listened for Ctrl-C only, so SIGTERM killed it with
//! work in flight.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    lines: mpsc::Receiver<String>,
    log: String,
    dir: std::path::PathBuf,
}

impl Server {
    fn start(env: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!("jjstop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "").unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_jamjet-server"));
        cmd.current_dir(&dir).env_clear();
        for key in ["PATH", "HOME", "TMPDIR"] {
            if let Ok(v) = std::env::var(key) {
                cmd.env(key, v);
            }
        }
        cmd.env("RUST_LOG", "info")
            .env("STORAGE_BACKEND", "memory")
            .env("JAMJET_BIND", "127.0.0.1")
            .env("JAMJET_PORT", "0")
            .env("JAMJET_EMBED_WORKERS", "1");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, lines) = mpsc::channel::<String>();
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
        let mut server = Server {
            child,
            lines,
            log: String::new(),
            dir,
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !server.log.contains("listening on") {
            server.read_for(Duration::from_millis(200));
        }
        assert!(
            server.log.contains("listening on"),
            "the server never came up: {}",
            server.log
        );
        server
    }

    fn read_for(&mut self, d: Duration) {
        if let Ok(line) = self.lines.recv_timeout(d) {
            self.log.push_str(&line);
            self.log.push('\n');
        }
    }

    fn addr(&self) -> String {
        let rest = self.log.split("listening on ").nth(1).unwrap();
        rest.chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ':')
            .collect()
    }

    /// SIGTERM, then the exit status within `within`, or `None`.
    fn terminate(&mut self, within: Duration) -> Option<std::process::ExitStatus> {
        let killed = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        let deadline = Instant::now() + within;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(200)) {
            self.log.push_str(&line);
            self.log.push('\n');
        }
        let _ = std::fs::remove_dir_all(&self.dir);
        status
    }
}

#[test]
fn sigterm_shuts_the_server_down_cleanly() {
    let mut server = Server::start(&[]);
    let status = server
        .terminate(Duration::from_secs(30))
        .unwrap_or_else(|| panic!("still running 30 s after SIGTERM: {}", server.log));
    assert!(
        status.success(),
        "SIGTERM must end in a clean exit, not a kill: {status:?}\n{}",
        server.log
    );
    assert!(server.log.contains("SIGTERM received"), "{}", server.log);
    assert!(server.log.contains("Shutting down"), "{}", server.log);
}

/// An open MCP SSE stream never ends on its own. Shutdown used to wait for
/// every HTTP connection before it stopped the workers, so one attached MCP
/// client kept the server (and its workers) running until SIGKILL.
#[test]
fn an_open_sse_stream_does_not_hold_shutdown() {
    // Dev mode on loopback, so the stream opens without a token.
    let mut server = Server::start(&[("JAMJET_DEV_MODE", "1")]);
    let mut sse = std::net::TcpStream::connect(server.addr()).unwrap();
    write!(
        sse,
        "GET /mcp/sse HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n"
    )
    .unwrap();
    // Let the server accept it and start streaming.
    std::thread::sleep(Duration::from_millis(500));
    let status = server.terminate(Duration::from_secs(30));
    drop(sse);
    let status =
        status.unwrap_or_else(|| panic!("an SSE stream held shutdown open: {}", server.log));
    assert!(status.success(), "{status:?}\n{}", server.log);
    assert!(server.log.contains("Shutting down"), "{}", server.log);
}

/// A model sidecar that answers `/health` and takes `/v1/complete` without
/// ever answering, so a model turn stays in flight. Says on `called` when the
/// first completion arrives.
fn hanging_sidecar() -> (String, mpsc::Receiver<()>) {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, called) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        for conn in listener.incoming().map_while(Result::ok) {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut conn = conn;
                let mut buf = [0u8; 4096];
                let n = conn.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                if head.starts_with("GET /health") {
                    let body = r#"{"ok":true}"#;
                    let _ = write!(
                        conn,
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                } else {
                    let _ = tx.send(());
                    std::thread::sleep(Duration::from_secs(120));
                }
            });
        }
    });
    (format!("http://{addr}"), called)
}

fn http(addr: &str, request: &str) -> std::io::Result<String> {
    use std::io::Read;
    let mut conn = std::net::TcpStream::connect(addr)?;
    conn.set_read_timeout(Some(Duration::from_secs(5)))?;
    conn.write_all(request.as_bytes())?;
    let mut out = String::new();
    conn.read_to_string(&mut out)?;
    Ok(out)
}

fn post_json(addr: &str, path: &str, body: &str) -> String {
    http(
        addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .unwrap()
}

/// Registers a one-node model workflow and starts a run of it, so a model
/// turn is in flight once the sidecar is called.
fn start_model_run(addr: &str) {
    let ir = serde_json::json!({
        "workflow_id": "drain-wf", "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "talk",
        "nodes": { "talk": { "id": "talk", "kind": {
            "type": "model", "model_ref": "anthropic/claude-sonnet-4-6",
            "prompt_ref": "hello", "output_schema": ""
        }, "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {} } },
        "edges": [ { "from": "talk", "to": "end", "condition": null } ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    });
    let reg = post_json(
        addr,
        "/workflows",
        &serde_json::json!({ "ir": ir }).to_string(),
    );
    assert!(reg.starts_with("HTTP/1.1 2"), "{reg}");
    let run = post_json(
        addr,
        "/executions",
        r#"{"workflow_id":"drain-wf","workflow_version":"0.1.0","input":{}}"#,
    );
    assert!(run.starts_with("HTTP/1.1 2"), "{run}");
}

/// While the in-process workers drain, the server still answers: external
/// workers heartbeat and complete through it. The listener used to close at
/// the signal, so their leases lapsed during the drain and the work ran
/// again.
#[test]
fn the_server_answers_while_the_workers_drain() {
    let (seam, called) = hanging_sidecar();
    let mut server = Server::start(&[
        ("JAMJET_DEV_MODE", "1"),
        ("JAMJET_MODEL_SEAM_URL", &seam),
        ("JAMJET_SHUTDOWN_GRACE_SECS", "3"),
    ]);
    let addr = server.addr();
    start_model_run(&addr);
    called
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("the model turn never started: {}", server.log));

    // One second after the SIGTERM `terminate` sends: inside the 3 s grace,
    // with the model turn still in flight.
    let probe_addr = addr.clone();
    let probe = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1000));
        http(
            &probe_addr,
            "GET /health HTTP/1.1\r\nHost: localhost\r\nconnection: close\r\n\r\n",
        )
    });
    let status = server
        .terminate(Duration::from_secs(30))
        .unwrap_or_else(|| panic!("still running: {}", server.log));
    let during = probe.join().unwrap();
    let during = during.unwrap_or_else(|e| panic!("refused during the drain: {e}\n{}", server.log));
    assert!(during.starts_with("HTTP/1.1 200"), "{during}");
    assert!(status.success(), "{status:?}\n{}", server.log);
}

/// On a serverless platform the stop timeout is 10 s. A model call that never
/// answers must not hold the process past it: the grace is 4 s by default,
/// then the scheduler and HTTP drains, and the item is left for lease expiry.
#[test]
fn a_platform_stop_ends_within_ten_seconds_with_a_model_call_in_flight() {
    let (seam, called) = hanging_sidecar();
    let mut server = Server::start(&[
        ("K_SERVICE", "svc"),
        ("STORAGE_BACKEND", "memory"),
        ("JAMJET_DEV_MODE", "1"),
        ("JAMJET_MODEL_SEAM_URL", &seam),
    ]);
    start_model_run(&server.addr());
    called
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("the model turn never started: {}", server.log));
    let signalled = Instant::now();
    let status = server
        .terminate(Duration::from_secs(30))
        .unwrap_or_else(|| panic!("still running 30 s after SIGTERM: {}", server.log));
    let took = signalled.elapsed();
    assert!(
        took < Duration::from_secs(10),
        "took {took:?} to stop, past the platform's 10 s\n{}",
        server.log
    );
    assert!(status.success(), "{status:?}\n{}", server.log);
    assert!(
        server
            .log
            .contains("their items are reclaimed when the lease expires"),
        "{}",
        server.log
    );
}

/// A claim that waits for its item answers `draining` at the stop signal,
/// not when its wait ends (up to 10 s later).
#[test]
fn a_waiting_claim_answers_draining_at_the_stop_signal() {
    let mut server = Server::start(&[("JAMJET_DEV_MODE", "1"), ("JAMJET_EMBED_WORKERS", "0")]);
    let addr = server.addr();
    let node = |id: &str| {
        serde_json::json!({
            "id": id, "kind": { "type": "external_fn", "queue": "jobs", "handler": id,
                "output_schema": "", "claim": "by_execution" },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
        })
    };
    let ir = serde_json::json!({
        "workflow_id": "claim-wf", "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "first",
        "nodes": { "first": node("first"), "second": node("second") },
        "edges": [ { "from": "first", "to": "second", "condition": null },
                   { "from": "second", "to": "end", "condition": null } ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    });
    let reg = post_json(
        &addr,
        "/workflows",
        &serde_json::json!({ "ir": ir }).to_string(),
    );
    assert!(reg.starts_with("HTTP/1.1 2"), "{reg}");
    let run = post_json(
        &addr,
        "/executions",
        r#"{"workflow_id":"claim-wf","workflow_version":"0.1.0","input":{}}"#,
    );
    assert!(run.starts_with("HTTP/1.1 2"), "{run}");
    let started: serde_json::Value =
        serde_json::from_str(run.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let execution_id = started["execution_id"].as_str().unwrap().to_string();

    // The second node has no item until the first completes, which nobody
    // does: the claim waits.
    let (tx, answers) = mpsc::channel::<(Instant, String)>();
    let claim_addr = addr.clone();
    std::thread::spawn(move || {
        let body = serde_json::json!({
            "worker_id": "w1", "queue_types": ["jobs"], "execution_id": execution_id,
            "node_id": "second", "wait_ms": 10000
        })
        .to_string();
        let mut conn = std::net::TcpStream::connect(&claim_addr).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        write!(
            conn,
            "POST /work-items/claim HTTP/1.1\r\nHost: localhost\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        let _ = std::io::Read::read_to_string(&mut conn, &mut out);
        let _ = tx.send((Instant::now(), out));
    });
    // The race is open: half a second in, the claim is still waiting.
    assert!(
        answers.recv_timeout(Duration::from_millis(500)).is_err(),
        "the claim answered before the stop signal: {}",
        server.log
    );
    let signalled = Instant::now();
    let status = server.terminate(Duration::from_secs(30));
    let (answered, out) = answers
        .recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|_| panic!("the claim never answered: {}", server.log));
    let late = answered.saturating_duration_since(signalled);
    assert!(
        out.contains(r#""reason":"draining""#) && out.contains(r#""claimed":false"#),
        "{out}\n{}",
        server.log
    );
    assert!(
        late < Duration::from_secs(1),
        "answered {late:?} after the signal\n{}",
        server.log
    );
    assert!(status.is_some_and(|s| s.success()), "{}", server.log);
}
