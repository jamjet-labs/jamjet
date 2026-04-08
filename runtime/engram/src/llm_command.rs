//! `CommandLlmClient` — the shell-out extensibility escape hatch.
//!
//! Spawns a user-supplied command per extraction call, writes a JSON request
//! to its stdin, and reads a JSON response from its stdout. Use this to plug
//! in any provider Engram does not ship natively — an internal corporate LLM
//! gateway, an exotic model behind a custom RPC, a local inference binary,
//! or a quick wrapper over any HTTP API with whatever auth scheme you need.
//!
//! # The contract
//!
//! Engram invokes your command through `sh -c <command>` (so `$HOME`,
//! pipes, redirects, and environment variables all work). The command must:
//!
//! 1. Read exactly one JSON object from stdin, matching:
//!    ```json
//!    {"system": "...", "user": "...", "structured": true}
//!    ```
//!    `structured` is `true` when Engram wants JSON output (extraction,
//!    consolidation), `false` when it wants plain text.
//!
//! 2. Write exactly one JSON value to stdout. Either:
//!    - Plain: the JSON value that Engram should use directly.
//!    - Enveloped: `{"content": <value>}` or `{"error": "message"}`.
//!
//! 3. Exit with code 0 on success, non-zero on failure. On non-zero exit,
//!    stderr is surfaced to the caller.
//!
//! # Example wrapper (Python, ~15 lines)
//!
//! ```text
//! #!/usr/bin/env python3
//! # my-llm.py — wraps any Python LLM SDK for Engram.
//! import json, sys
//! from my_llm_sdk import chat  # your SDK
//!
//! req = json.loads(sys.stdin.read())
//! resp = chat(system=req["system"], user=req["user"],
//!             json_mode=req.get("structured", False))
//! # Either write the raw response:
//! sys.stdout.write(json.dumps(resp))
//! # Or envelope it:
//! # sys.stdout.write(json.dumps({"content": resp}))
//! ```
//!
//! Then point Engram at it:
//! ```text
//! ENGRAM_LLM_PROVIDER=command \
//! ENGRAM_LLM_COMMAND="python /path/to/my-llm.py" \
//! engram serve
//! ```
//!
//! # Security
//!
//! `CommandLlmClient` runs **arbitrary commands** as the Engram process user.
//! Never expose it in a multi-tenant deployment where untrusted users can
//! control `ENGRAM_LLM_COMMAND`. It is a local and single-tenant feature.

use crate::llm::LlmClient;
use crate::llm_util::extract_json_payload;
use crate::store::MemoryError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// A shell-out LLM client. Clone-cheap: stores only the command string and
/// timeout configuration.
#[derive(Clone, Debug)]
pub struct CommandLlmClient {
    command: String,
    timeout_secs: u64,
}

impl CommandLlmClient {
    /// Construct a client that will run `command` via `sh -c` on every call.
    /// Default timeout is 120 seconds.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            timeout_secs: 120,
        }
    }

    /// Override the per-call timeout in seconds.
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }
}

#[derive(Serialize)]
struct CommandRequest<'a> {
    system: &'a str,
    user: &'a str,
    structured: bool,
}

/// Optional envelope wrapping the command's output. Commands MAY return a
/// bare JSON value or this envelope. If both `content` and `error` are
/// present, `error` wins.
#[derive(Deserialize)]
struct CommandEnvelope {
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<String>,
}

impl CommandLlmClient {
    async fn call(
        &self,
        system: &str,
        user: &str,
        structured: bool,
    ) -> Result<serde_json::Value, MemoryError> {
        if self.command.trim().is_empty() {
            return Err(MemoryError::Database(
                "CommandLlmClient: command is empty".into(),
            ));
        }

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| MemoryError::Database(format!("command spawn failed: {e}")))?;

        let request_json = serde_json::to_string(&CommandRequest {
            system,
            user,
            structured,
        })
        .map_err(|e| MemoryError::Serialization(format!("CommandRequest serialize: {e}")))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(request_json.as_bytes())
                .await
                .map_err(|e| MemoryError::Database(format!("command stdin write: {e}")))?;
            // stdin dropped here, signalling EOF to the child.
        }

        let timeout = std::time::Duration::from_secs(self.timeout_secs);
        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                MemoryError::Database(format!("command timed out after {}s", self.timeout_secs))
            })?
            .map_err(|e| MemoryError::Database(format!("command wait failed: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            return Err(MemoryError::Database(format!(
                "command exited with code {code}: {}",
                stderr.trim()
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let payload = extract_json_payload(&stdout);

        if payload.is_empty() {
            return Err(MemoryError::Database(
                "command produced empty stdout".into(),
            ));
        }

        // First try to parse as an envelope and honour `error` / `content`.
        if let Ok(envelope) = serde_json::from_str::<CommandEnvelope>(payload) {
            if let Some(err) = envelope.error {
                return Err(MemoryError::Database(format!("command error: {err}")));
            }
            if let Some(content) = envelope.content {
                return Ok(content);
            }
        }

        // Fall through: treat the whole stdout as the JSON value itself.
        serde_json::from_str(payload).map_err(|e| {
            MemoryError::Serialization(format!(
                "command JSON parse: {e} (stdout head: {})",
                payload.chars().take(200).collect::<String>()
            ))
        })
    }
}

#[async_trait]
impl LlmClient for CommandLlmClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, MemoryError> {
        let value = self.call(system, user, false).await?;
        match value {
            serde_json::Value::String(s) => Ok(s),
            other => Ok(other.to_string()),
        }
    }

    async fn structured_output(
        &self,
        system: &str,
        user: &str,
    ) -> Result<serde_json::Value, MemoryError> {
        self.call(system, user, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_raw_json_output() {
        let client = CommandLlmClient::new(r#"cat > /dev/null; echo '{"facts":[]}'"#);
        let result = client.structured_output("sys", "user").await.unwrap();
        assert_eq!(result, serde_json::json!({"facts": []}));
    }

    #[tokio::test]
    async fn returns_envelope_content() {
        let client = CommandLlmClient::new(
            r#"cat > /dev/null; echo '{"content":{"facts":[{"text":"hello"}]}}'"#,
        );
        let result = client.structured_output("sys", "user").await.unwrap();
        assert_eq!(result, serde_json::json!({"facts": [{"text": "hello"}]}));
    }

    #[tokio::test]
    async fn envelope_error_surfaces() {
        let client =
            CommandLlmClient::new(r#"cat > /dev/null; echo '{"error":"deliberate failure"}'"#);
        let err = client
            .structured_output("sys", "user")
            .await
            .expect_err("should error");
        assert!(err.to_string().contains("deliberate failure"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_error() {
        let client = CommandLlmClient::new(r#"cat > /dev/null; echo 'oops' >&2; exit 7"#);
        let err = client
            .structured_output("sys", "user")
            .await
            .expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("code 7"), "expected exit code in error: {msg}");
    }

    #[tokio::test]
    async fn empty_command_rejected() {
        let client = CommandLlmClient::new("   ");
        let err = client
            .structured_output("sys", "user")
            .await
            .expect_err("should error");
        assert!(err.to_string().contains("empty"));
    }

    #[tokio::test]
    async fn complete_returns_string_text() {
        let client = CommandLlmClient::new(r#"cat > /dev/null; echo '"hello there"'"#);
        let result = client.complete("sys", "user").await.unwrap();
        assert_eq!(result, "hello there");
    }

    #[tokio::test]
    async fn command_sees_request_on_stdin() {
        // Use `wc -c` to count the stdin bytes the child receives, then echo
        // a JSON envelope back to verify bidirectional flow.
        let client =
            CommandLlmClient::new(r#"bytes=$(wc -c); echo "{\"content\":{\"bytes\":$bytes}}""#);
        let result = client
            .structured_output("system prompt", "user prompt")
            .await
            .unwrap();
        // Request JSON is non-empty — exact byte count depends on serde output,
        // but it should be > 30 bytes for the fields we pass.
        let bytes = result["bytes"].as_u64().unwrap_or(0);
        assert!(bytes > 30, "expected stdin bytes > 30, got {bytes}");
    }
}
