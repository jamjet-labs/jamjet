//! A2A client — discover remote agents, submit tasks, stream results.

use crate::types::*;
use jamjet_agents::card::AgentCard;
use tracing::{debug, info};

pub struct A2aClient {
    http: reqwest::Client,
    /// Optional Bearer token for A2A auth (G2.5).
    bearer_token: Option<String>,
}

impl A2aClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            bearer_token: std::env::var("JAMJET_A2A_TOKEN").ok(),
        }
    }

    /// Set a Bearer token to send with all requests.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let mut builder = self.http.request(method, url);
        if let Some(token) = &self.bearer_token {
            builder = builder.bearer_auth(token);
        }
        builder
    }

    /// Fetch and parse an Agent Card from a remote URL.
    pub async fn discover(&self, base_url: &str) -> Result<AgentCard, String> {
        let url = format!("{}/.well-known/agent.json", base_url.trim_end_matches('/'));
        debug!(url = %url, "Fetching Agent Card");

        let response = self
            .request(reqwest::Method::GET, &url)
            .send()
            .await
            .map_err(|e| format!("Failed to fetch Agent Card from {url}: {e}"))?;

        if !response.status().is_success() {
            return Err(format!(
                "Agent Card fetch returned {}: {}",
                response.status(),
                url
            ));
        }

        let card: AgentCard = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse Agent Card: {e}"))?;

        info!(
            agent_id = %card.id,
            name = %card.name,
            skills = card.capabilities.skills.len(),
            "Discovered A2A agent"
        );
        Ok(card)
    }

    /// Submit a task to a remote A2A agent.
    pub async fn send_task(
        &self,
        base_url: &str,
        request: SendTaskRequest,
    ) -> Result<A2aTask, String> {
        let url = format!("{}/", base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tasks/send",
            "params": request
        });

        let response = self
            .request(reqwest::Method::POST, &url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let rpc: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;

        if let Some(error) = rpc.get("error") {
            return Err(format!("A2A tasks/send error: {error}"));
        }

        serde_json::from_value(rpc["result"].clone()).map_err(|e| e.to_string())
    }

    /// Get the current status of a submitted task.
    pub async fn get_task(&self, base_url: &str, task_id: &str) -> Result<A2aTask, String> {
        let url = format!("{}/", base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tasks/get",
            "params": { "id": task_id }
        });

        let response = self
            .request(reqwest::Method::POST, &url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let rpc: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;

        if let Some(error) = rpc.get("error") {
            return Err(format!("A2A tasks/get error: {error}"));
        }

        serde_json::from_value(rpc["result"].clone()).map_err(|e| e.to_string())
    }

    /// Cancel a task.
    pub async fn cancel_task(&self, base_url: &str, task_id: &str) -> Result<(), String> {
        let url = format!("{}/", base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tasks/cancel",
            "params": { "id": task_id }
        });

        self.request(reqwest::Method::POST, &url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Subscribe to SSE task progress events.
    ///
    /// Returns a stream of `A2aStreamEvent` items. The stream ends when the
    /// task reaches a terminal state (`completed`, `failed`, `canceled`).
    pub async fn subscribe_task(
        &self,
        base_url: &str,
        task_id: &str,
    ) -> Result<impl futures::Stream<Item = Result<A2aStreamEvent, String>>, String> {
        use futures::StreamExt;

        let url = format!("{}/sse/{}", base_url.trim_end_matches('/'), task_id);
        debug!(url = %url, "Subscribing to A2A SSE stream");

        let response = self
            .request(reqwest::Method::GET, &url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| format!("SSE connect failed: {e}"))?;

        if !response.status().is_success() {
            return Err(format!("SSE connect returned {}", response.status()));
        }

        // Parse SSE data lines into A2aStreamEvent.
        let stream = response
            .bytes_stream()
            .map(|chunk| {
                let chunk = chunk.map_err(|e| format!("SSE read error: {e}"))?;
                let text = String::from_utf8_lossy(&chunk).into_owned();
                // SSE format: "data: <json>\n\n"
                for line in text.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        if let Ok(event) = serde_json::from_str::<A2aStreamEvent>(data) {
                            return Ok(event);
                        }
                    }
                }
                Err("empty or non-data SSE line".into())
            })
            .filter(|r| {
                // Filter out parse failures for ping/comment lines.
                let keep = r.is_ok();
                futures::future::ready(keep)
            });

        Ok(stream)
    }

    /// Poll a task until it reaches a terminal state.
    ///
    /// Polls every `interval` seconds. Returns the final task state.
    pub async fn wait_for_completion(
        &self,
        base_url: &str,
        task_id: &str,
        poll_interval: std::time::Duration,
    ) -> Result<A2aTask, String> {
        loop {
            let task = self.get_task(base_url, task_id).await?;
            match &task.status.state {
                A2aTaskState::Completed | A2aTaskState::Failed | A2aTaskState::Canceled => {
                    return Ok(task)
                }
                _ => {
                    tokio::time::sleep(poll_interval).await;
                }
            }
        }
    }
}

impl Default for A2aClient {
    fn default() -> Self {
        Self::new()
    }
}
