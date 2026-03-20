use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// REST client for calling Python coordinator strategies.
pub struct StrategyBridge {
    client: Client,
    base_url: String,
}

#[derive(Debug, Serialize)]
pub struct DiscoverRequest {
    pub task: String,
    pub required_skills: Vec<String>,
    pub preferred_skills: Vec<String>,
    pub trust_domain: Option<String>,
    pub strategy_name: String,
    pub context: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct DiscoverResponse {
    pub candidates: Vec<AgentCandidate>,
    pub filtered_out: Vec<FilteredAgent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCandidate {
    pub uri: String,
    pub agent_card: serde_json::Value,
    pub skills: Vec<String>,
    pub latency_class: Option<String>,
    pub cost_class: Option<String>,
    pub trust_domain: Option<String>,
    #[serde(default)]
    pub reasoning_modes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilteredAgent {
    pub uri: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct ScoreRequest {
    pub task: String,
    pub candidates: Vec<AgentCandidate>,
    pub weights: serde_json::Value,
    pub context: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct ScoreResponse {
    pub rankings: Vec<ScoringEntry>,
    pub spread: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringEntry {
    pub uri: String,
    pub scores: serde_json::Value,
    pub composite: f64,
}

#[derive(Debug, Serialize)]
pub struct DecideRequest {
    pub task: String,
    pub top_candidates: Vec<ScoringEntry>,
    pub threshold: f64,
    pub tiebreaker_model: String,
    pub context: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct DecideResponse {
    pub selected_uri: Option<String>,
    pub method: String,
    pub reasoning: Option<String>,
    pub confidence: f64,
    pub rejected: Vec<serde_json::Value>,
    pub tiebreaker_tokens: Option<serde_json::Value>,
    pub tiebreaker_cost: Option<f64>,
}

impl StrategyBridge {
    pub fn new(base_url: String) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");
        Self { client, base_url }
    }

    pub async fn discover(
        &self,
        req: DiscoverRequest,
    ) -> Result<DiscoverResponse, StrategyBridgeError> {
        let url = format!("{}/coordinator/discover", self.base_url);
        let resp = self.client.post(&url).json(&req).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(StrategyBridgeError::StrategyError {
                status: status.as_u16(),
                body,
            });
        }
        Ok(resp.json().await?)
    }

    pub async fn score(&self, req: ScoreRequest) -> Result<ScoreResponse, StrategyBridgeError> {
        let url = format!("{}/coordinator/score", self.base_url);
        let resp = self.client.post(&url).json(&req).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(StrategyBridgeError::StrategyError {
                status: status.as_u16(),
                body,
            });
        }
        Ok(resp.json().await?)
    }

    pub async fn decide(&self, req: DecideRequest) -> Result<DecideResponse, StrategyBridgeError> {
        let url = format!("{}/coordinator/decide", self.base_url);
        let resp = self.client.post(&url).json(&req).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(StrategyBridgeError::StrategyError {
                status: status.as_u16(),
                body,
            });
        }
        Ok(resp.json().await?)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StrategyBridgeError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Strategy returned error status {status}: {body}")]
    StrategyError { status: u16, body: String },
}
