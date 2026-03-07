//! MCP transport implementations: stdio and HTTP+SSE.

use crate::types::{JsonRpcRequest, JsonRpcResponse};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use tracing::{debug, warn};

#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn send(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse, String>;
    async fn close(&self);
}

// ── Stdio transport ───────────────────────────────────────────────────────────

/// stdio transport — spawns a subprocess and communicates over stdin/stdout.
///
/// The MCP spec requires newline-delimited JSON-RPC messages. Each call to
/// `send()` writes one JSON line to stdin and reads one JSON line from stdout.
/// Requests are serialized through a mutex so they don't interleave.
pub struct StdioTransport {
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
    _child: Mutex<Child>,
}

impl StdioTransport {
    /// Spawn an MCP server subprocess and return a connected transport.
    pub async fn spawn(command: &str, args: &[&str]) -> Result<Self, String> {
        use tokio::process::Command;

        let mut child = Command::new(command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit()) // let MCP server logs go to our stderr
            .spawn()
            .map_err(|e| format!("Failed to spawn MCP server `{command}`: {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or("Failed to open MCP server stdin")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Failed to open MCP server stdout")?;

        Ok(Self {
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            _child: Mutex::new(child),
        })
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn send(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse, String> {
        let mut line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        line.push('\n');

        debug!(id = ?request.id, method = %request.method, "MCP stdio → send");

        // Write request.
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| format!("MCP stdin write error: {e}"))?;
            stdin
                .flush()
                .await
                .map_err(|e| format!("MCP stdin flush error: {e}"))?;
        }

        // Read response (one JSON line).
        let response_line = {
            let mut stdout = self.stdout.lock().await;
            let mut buf = String::new();
            stdout
                .read_line(&mut buf)
                .await
                .map_err(|e| format!("MCP stdout read error: {e}"))?;
            buf
        };

        if response_line.is_empty() {
            return Err("MCP server closed stdout unexpectedly".into());
        }

        debug!(line = %response_line.trim(), "MCP stdio ← recv");

        serde_json::from_str(&response_line)
            .map_err(|e| format!("MCP response parse error: {e} (raw: {response_line})"))
    }

    async fn close(&self) {
        // Dropping stdin closes the pipe which signals the child to exit.
        // The child is held in _child and will be dropped with the struct.
        warn!("MCP stdio transport closing");
    }
}

// ── HTTP+SSE transport ────────────────────────────────────────────────────────

/// HTTP+SSE transport — connects to a remote MCP HTTP server.
pub struct HttpSseTransport {
    url: String,
    client: reqwest::Client,
    /// Optional Bearer token for MCP auth (G2.6).
    bearer_token: Option<String>,
}

impl HttpSseTransport {
    pub fn new(url: String) -> Self {
        Self {
            url,
            client: reqwest::Client::new(),
            bearer_token: std::env::var("JAMJET_MCP_TOKEN").ok(),
        }
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }
}

#[async_trait]
impl McpTransport for HttpSseTransport {
    async fn send(&self, request: JsonRpcRequest) -> Result<JsonRpcResponse, String> {
        let mut builder = self.client.post(&self.url).json(&request);
        if let Some(token) = &self.bearer_token {
            builder = builder.bearer_auth(token);
        }
        let response = builder.send().await.map_err(|e| e.to_string())?;

        response
            .json::<JsonRpcResponse>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn close(&self) {}
}
