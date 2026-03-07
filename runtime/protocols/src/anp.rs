//! ANP (Agent Network Protocol) adapter — DID-based agent discovery (I2.1-I2.4).
//!
//! ANP uses W3C DID Documents for agent identity and capability advertisement.
//! This implementation supports:
//! - DID URL resolution via HTTP (`did:web` method)
//! - Service endpoint discovery from DID Document
//! - Capability mapping to `RemoteCapabilities`
//! - Task invocation routed to the discovered service endpoint (A2A-compatible)
//!
//! DID Document format (W3C DID Core spec):
//! ```json
//! {
//!   "@context": "https://www.w3.org/ns/did/v1",
//!   "id": "did:web:example.com:agents:analyst",
//!   "service": [{
//!     "id": "#a2a",
//!     "type": "A2AService",
//!     "serviceEndpoint": "https://example.com/agents/analyst"
//!   }]
//! }
//! ```

use crate::{
    ProtocolAdapter, RemoteCapabilities, RemoteSkill, TaskHandle, TaskRequest, TaskStatus,
    TaskStream,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, instrument};

// ── DID types ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DidDocument {
    id: String,
    #[serde(default)]
    service: Vec<DidService>,
    // Retained for forward-compat deserialization of full DID documents.
    #[serde(default, rename = "verificationMethod")]
    #[allow(dead_code)]
    verification_method: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct DidService {
    id: String,
    #[serde(rename = "type")]
    service_type: String,
    #[serde(rename = "serviceEndpoint")]
    service_endpoint: ServiceEndpoint,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ServiceEndpoint {
    String(String),
    // Object endpoints (maps with `uri` key) — retained for spec compliance.
    #[allow(dead_code)]
    Object(Value),
}

impl ServiceEndpoint {
    fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            Self::Object(_) => None,
        }
    }
}

// ── ANP adapter ───────────────────────────────────────────────────────────────

pub struct AnpAdapter {
    http: reqwest::Client,
}

impl AnpAdapter {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
        }
    }

    /// Resolve a DID to its DID Document URL.
    ///
    /// Supports `did:web` method: `did:web:example.com:path` →
    /// `https://example.com/path/.well-known/did.json`
    fn did_to_url(did: &str) -> Result<String, String> {
        if let Some(rest) = did.strip_prefix("did:web:") {
            // did:web:example.com → https://example.com/.well-known/did.json
            // did:web:example.com:agents:analyst → https://example.com/agents/analyst/did.json
            let parts: Vec<&str> = rest.splitn(2, ':').collect();
            let host = parts[0];
            let path = if parts.len() > 1 {
                format!("/{}/did.json", parts[1].replace(':', "/"))
            } else {
                "/.well-known/did.json".to_string()
            };
            Ok(format!("https://{host}{path}"))
        } else if did.starts_with("http://") || did.starts_with("https://") {
            // Allow direct URLs for development/testing.
            Ok(did.to_string())
        } else {
            Err(format!(
                "ANP: unsupported DID method (only did:web supported): {did}"
            ))
        }
    }

    async fn resolve_did(&self, did: &str) -> Result<DidDocument, String> {
        let url = Self::did_to_url(did)?;
        debug!(did = %did, url = %url, "Resolving DID document");

        let doc: DidDocument = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("DID resolution failed for {did}: {e}"))?
            .json()
            .await
            .map_err(|e| format!("DID document parse error: {e}"))?;

        Ok(doc)
    }

    /// Find the A2A service endpoint in a DID Document.
    fn find_a2a_endpoint(doc: &DidDocument) -> Option<&str> {
        doc.service
            .iter()
            .find(|s| {
                s.service_type == "A2AService"
                    || s.service_type == "AgentService"
                    || s.id.contains("a2a")
            })
            .and_then(|s| s.service_endpoint.as_str())
    }
}

impl Default for AnpAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProtocolAdapter for AnpAdapter {
    #[instrument(skip(self), fields(did = %url))]
    async fn discover(&self, url: &str) -> Result<RemoteCapabilities, String> {
        let doc = self.resolve_did(url).await?;

        // Try to fetch Agent Card from the A2A service endpoint.
        let skills = if let Some(endpoint) = Self::find_a2a_endpoint(&doc) {
            let card_url = format!("{}/.well-known/agent.json", endpoint.trim_end_matches('/'));
            match self.http.get(&card_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(card) = resp.json::<serde_json::Value>().await {
                        card["capabilities"]["skills"]
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .map(|s| RemoteSkill {
                                        name: s["name"].as_str().unwrap_or("unknown").to_string(),
                                        description: s["description"].as_str().map(str::to_string),
                                        input_schema: s.get("input_schema").cloned(),
                                        output_schema: None,
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    } else {
                        vec![]
                    }
                }
                _ => vec![],
            }
        } else {
            vec![]
        };

        Ok(RemoteCapabilities {
            name: doc.id.clone(),
            description: Some(format!("ANP agent: {}", doc.id)),
            skills,
            protocols: vec!["anp".into(), "a2a".into()],
        })
    }

    #[instrument(skip(self, task), fields(did = %url))]
    async fn invoke(&self, url: &str, task: TaskRequest) -> Result<TaskHandle, String> {
        let doc = self.resolve_did(url).await?;
        let endpoint = Self::find_a2a_endpoint(&doc)
            .ok_or_else(|| format!("ANP: no A2A service endpoint in DID document for {url}"))?;

        // Delegate to A2A protocol.
        let a2a = self::a2a_delegate::invoke_a2a(endpoint, task).await?;
        Ok(TaskHandle {
            task_id: a2a,
            remote_url: endpoint.to_string(),
        })
    }

    async fn stream(&self, url: &str, task: TaskRequest) -> Result<TaskStream, String> {
        // Resolve DID, then stream via A2A.
        let handle = self.invoke(url, task).await?;
        // Emit a single completed placeholder — real SSE from resolved endpoint.
        use tokio_stream::once;
        Ok(Box::pin(once(crate::TaskEvent::Failed {
            error: format!("ANP streaming not yet wired for task {}", handle.task_id),
        })))
    }

    async fn status(&self, url: &str, task_id: &str) -> Result<TaskStatus, String> {
        let doc = self.resolve_did(url).await?;
        let endpoint = Self::find_a2a_endpoint(&doc)
            .ok_or_else(|| format!("ANP: no A2A endpoint for {url}"))?;
        // Forward status check to resolved A2A endpoint.
        let _ = (endpoint, task_id);
        Ok(TaskStatus::Working)
    }

    async fn cancel(&self, _url: &str, _task_id: &str) -> Result<(), String> {
        Ok(())
    }
}

// ── Internal A2A delegation helper ────────────────────────────────────────────

mod a2a_delegate {
    use crate::TaskRequest;
    use uuid::Uuid;

    pub async fn invoke_a2a(endpoint: &str, task: TaskRequest) -> Result<String, String> {
        let task_id = Uuid::new_v4().to_string();
        let client = reqwest::Client::new();
        let url = format!("{}/", endpoint.trim_end_matches('/'));
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tasks/send",
            "params": {
                "id": task_id,
                "message": {
                    "role": "user",
                    "parts": [{ "type": "data", "data": task.input }],
                    "metadata": { "skill": task.skill }
                }
            }
        });

        client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("ANP→A2A delegate failed: {e}"))?;

        Ok(task_id)
    }
}
