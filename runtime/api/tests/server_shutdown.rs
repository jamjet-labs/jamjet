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
        &addr,
        "/workflows",
        &serde_json::json!({ "ir": ir }).to_string(),
    );
    assert!(reg.starts_with("HTTP/1.1 2"), "{reg}");
    let run = post_json(
        &addr,
        "/executions",
        r#"{"workflow_id":"drain-wf","workflow_version":"0.1.0","input":{}}"#,
    );
    assert!(run.starts_with("HTTP/1.1 2"), "{run}");
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
