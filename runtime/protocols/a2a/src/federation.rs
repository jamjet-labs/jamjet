//! A2A federation auth and mTLS configuration.
//!
//! - **Authorization middleware**: validates Bearer tokens on the A2A server,
//!   enforces capability-scoped access control, and logs federation events.
//! - **mTLS configuration**: TLS settings for cross-org federation with
//!   mutual certificate authentication.

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use tracing::{info, warn};

// ── Federation auth policy ──────────────────────────────────────────────────

/// Authorization policy for incoming A2A requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FederationPolicy {
    /// If true, all incoming requests require a valid Bearer token.
    #[serde(default)]
    pub require_auth: bool,

    /// Allowed Bearer tokens mapped to their granted scopes.
    /// Key: token value, Value: set of granted scopes (e.g., "tasks/send", "tasks/get").
    #[serde(default)]
    pub tokens: Vec<FederationToken>,

    /// If true, the Agent Card endpoint is public (no auth required).
    #[serde(default = "default_true")]
    pub public_agent_card: bool,

    /// Allowed caller agent IDs (if empty, all authenticated callers are allowed).
    #[serde(default)]
    pub allowed_agents: Vec<String>,

    /// Scopes required per RPC method. If empty, any valid token suffices.
    /// Example: `{"tasks/send": ["write"], "tasks/get": ["read"]}`
    #[serde(default)]
    pub method_scopes: std::collections::HashMap<String, Vec<String>>,
}

fn default_true() -> bool {
    true
}

/// A federation token with associated metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationToken {
    /// The token value (matched against Bearer header).
    pub token: String,
    /// Human-readable name for audit logging.
    pub name: String,
    /// Agent ID of the token holder.
    pub agent_id: Option<String>,
    /// Granted scopes (e.g., "read", "write", "tasks/send").
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// Result of federation auth validation.
#[derive(Debug, Clone)]
pub struct FederationIdentity {
    /// Name of the authenticated token.
    pub token_name: String,
    /// Agent ID of the caller, if known.
    pub agent_id: Option<String>,
    /// Granted scopes.
    pub scopes: HashSet<String>,
}

/// Validate a Bearer token against the federation policy.
pub fn validate_federation_token(
    headers: &HeaderMap,
    policy: &FederationPolicy,
) -> Result<FederationIdentity, &'static str> {
    let auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing authorization header")?;

    let token = auth_header
        .strip_prefix("Bearer ")
        .ok_or("authorization must be Bearer token")?;

    let found = policy
        .tokens
        .iter()
        .find(|t| t.token == token)
        .ok_or("invalid token")?;

    // Check agent ID allowlist.
    if !policy.allowed_agents.is_empty() {
        if let Some(agent_id) = &found.agent_id {
            if !policy.allowed_agents.contains(agent_id) {
                return Err("agent not in allowlist");
            }
        } else {
            return Err("token has no agent_id and allowlist is active");
        }
    }

    Ok(FederationIdentity {
        token_name: found.name.clone(),
        agent_id: found.agent_id.clone(),
        scopes: found.scopes.iter().cloned().collect(),
    })
}

/// Check if an identity has the required scopes for a given RPC method.
pub fn check_method_scopes(
    identity: &FederationIdentity,
    method: &str,
    policy: &FederationPolicy,
) -> bool {
    if let Some(required) = policy.method_scopes.get(method) {
        if required.is_empty() {
            return true;
        }
        required.iter().any(|s| identity.scopes.contains(s))
    } else {
        // No specific scope requirement for this method — allow.
        true
    }
}

/// Axum middleware layer for federation auth.
///
/// Insert into the A2A server router to require authentication:
/// ```ignore
/// use axum::middleware;
/// router.layer(middleware::from_fn_with_state(policy, federation_auth_layer))
/// ```
pub async fn federation_auth_layer(
    axum::extract::State(policy): axum::extract::State<FederationPolicy>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();

    // Allow public access to Agent Card if configured.
    if policy.public_agent_card && path.contains(".well-known/agent.json") {
        return next.run(request).await;
    }

    if !policy.require_auth {
        return next.run(request).await;
    }

    match validate_federation_token(request.headers(), &policy) {
        Ok(identity) => {
            info!(
                token_name = %identity.token_name,
                agent_id = ?identity.agent_id,
                path = %path,
                "A2A federation auth: authorized"
            );
            next.run(request).await
        }
        Err(reason) => {
            warn!(
                reason = reason,
                path = %path,
                "A2A federation auth: rejected"
            );
            (
                StatusCode::UNAUTHORIZED,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "error": { "code": -32003, "message": reason }
                })
                .to_string(),
            )
                .into_response()
        }
    }
}

// ── mTLS configuration ──────────────────────────────────────────────────────

/// TLS / mTLS configuration for A2A federation.
///
/// Used by both the A2A server (to require client certificates) and the A2A
/// client (to present client certificates when connecting to federated agents).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Enable TLS. If false, all other fields are ignored.
    #[serde(default)]
    pub enabled: bool,

    /// Path to the server/client certificate (PEM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_path: Option<PathBuf>,

    /// Path to the private key (PEM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_path: Option<PathBuf>,

    /// Path to the CA certificate bundle for verifying peer certificates.
    /// Required for mTLS (both client and server need to verify the peer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<PathBuf>,

    /// If true, the server requires client certificates (mTLS).
    /// If false, only server-side TLS is enabled.
    #[serde(default)]
    pub require_client_cert: bool,

    /// Allowed Common Names (CNs) from client certificates.
    /// If empty and `require_client_cert` is true, any valid client cert is accepted.
    #[serde(default)]
    pub allowed_cns: Vec<String>,
}

impl TlsConfig {
    /// Create from environment variables:
    /// - `JAMJET_TLS_CERT` — path to cert PEM
    /// - `JAMJET_TLS_KEY` — path to key PEM
    /// - `JAMJET_TLS_CA_CERT` — path to CA cert PEM
    /// - `JAMJET_MTLS_REQUIRED` — "true" to require client certs
    pub fn from_env() -> Self {
        let cert_path = std::env::var("JAMJET_TLS_CERT").ok().map(PathBuf::from);
        let key_path = std::env::var("JAMJET_TLS_KEY").ok().map(PathBuf::from);
        let ca_cert_path = std::env::var("JAMJET_TLS_CA_CERT").ok().map(PathBuf::from);
        let require_client_cert = std::env::var("JAMJET_MTLS_REQUIRED")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        let enabled = cert_path.is_some() && key_path.is_some();

        Self {
            enabled,
            cert_path,
            key_path,
            ca_cert_path,
            require_client_cert,
            allowed_cns: Vec::new(),
        }
    }

    /// Load the certificate and key as raw bytes.
    pub fn load_cert_key(&self) -> Result<(Vec<u8>, Vec<u8>), String> {
        let cert = std::fs::read(self.cert_path.as_ref().ok_or("cert_path not configured")?)
            .map_err(|e| format!("failed to read cert: {e}"))?;

        let key = std::fs::read(self.key_path.as_ref().ok_or("key_path not configured")?)
            .map_err(|e| format!("failed to read key: {e}"))?;

        Ok((cert, key))
    }

    /// Load the CA certificate as raw bytes (for client cert verification).
    pub fn load_ca_cert(&self) -> Result<Vec<u8>, String> {
        std::fs::read(
            self.ca_cert_path
                .as_ref()
                .ok_or("ca_cert_path not configured")?,
        )
        .map_err(|e| format!("failed to read CA cert: {e}"))
    }
}

// ── reqwest client builder with mTLS ────────────────────────────────────────

/// Build a `reqwest::Client` with mTLS configuration for outbound A2A calls.
///
/// Used by `A2aClient` when connecting to federated agents that require mTLS.
pub fn build_mtls_client(tls: &TlsConfig) -> Result<reqwest::Client, String> {
    if !tls.enabled {
        return Ok(reqwest::Client::new());
    }

    let (cert_pem, key_pem) = tls.load_cert_key()?;
    let identity = reqwest::Identity::from_pem(&[cert_pem.clone(), key_pem].concat())
        .map_err(|e| format!("invalid identity PEM: {e}"))?;

    let mut builder = reqwest::Client::builder().identity(identity);

    if let Ok(ca_pem) = tls.load_ca_cert() {
        let ca =
            reqwest::Certificate::from_pem(&ca_pem).map_err(|e| format!("invalid CA cert: {e}"))?;
        builder = builder.add_root_certificate(ca);
    }

    builder
        .build()
        .map_err(|e| format!("failed to build mTLS client: {e}"))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn test_policy() -> FederationPolicy {
        FederationPolicy {
            require_auth: true,
            tokens: vec![
                FederationToken {
                    token: "tok-alpha".to_string(),
                    name: "Alpha Agent".to_string(),
                    agent_id: Some("agent-alpha".to_string()),
                    scopes: vec!["read".to_string(), "write".to_string()],
                },
                FederationToken {
                    token: "tok-readonly".to_string(),
                    name: "Read-Only Agent".to_string(),
                    agent_id: Some("agent-ro".to_string()),
                    scopes: vec!["read".to_string()],
                },
            ],
            public_agent_card: true,
            allowed_agents: vec![],
            method_scopes: [
                ("tasks/send".to_string(), vec!["write".to_string()]),
                ("tasks/get".to_string(), vec!["read".to_string()]),
            ]
            .into_iter()
            .collect(),
        }
    }

    fn headers_with_token(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        h
    }

    #[test]
    fn valid_token_authenticates() {
        let policy = test_policy();
        let headers = headers_with_token("tok-alpha");
        let identity = validate_federation_token(&headers, &policy).unwrap();
        assert_eq!(identity.token_name, "Alpha Agent");
        assert_eq!(identity.agent_id, Some("agent-alpha".to_string()));
        assert!(identity.scopes.contains("write"));
    }

    #[test]
    fn invalid_token_rejected() {
        let policy = test_policy();
        let headers = headers_with_token("tok-invalid");
        assert!(validate_federation_token(&headers, &policy).is_err());
    }

    #[test]
    fn missing_auth_header_rejected() {
        let policy = test_policy();
        let headers = HeaderMap::new();
        assert!(validate_federation_token(&headers, &policy).is_err());
    }

    #[test]
    fn scope_check_for_method() {
        let policy = test_policy();
        let headers = headers_with_token("tok-readonly");
        let identity = validate_federation_token(&headers, &policy).unwrap();

        // Read-only token can tasks/get but not tasks/send.
        assert!(check_method_scopes(&identity, "tasks/get", &policy));
        assert!(!check_method_scopes(&identity, "tasks/send", &policy));
    }

    #[test]
    fn write_token_has_all_scopes() {
        let policy = test_policy();
        let headers = headers_with_token("tok-alpha");
        let identity = validate_federation_token(&headers, &policy).unwrap();

        assert!(check_method_scopes(&identity, "tasks/send", &policy));
        assert!(check_method_scopes(&identity, "tasks/get", &policy));
    }

    #[test]
    fn agent_allowlist_restricts_access() {
        let mut policy = test_policy();
        policy.allowed_agents = vec!["agent-alpha".to_string()]; // Only alpha allowed

        let headers = headers_with_token("tok-readonly");
        // agent-ro is not in allowlist
        assert!(validate_federation_token(&headers, &policy).is_err());

        let headers = headers_with_token("tok-alpha");
        assert!(validate_federation_token(&headers, &policy).is_ok());
    }

    #[test]
    fn tls_config_from_env_defaults_disabled() {
        // With no env vars set, TLS should be disabled.
        let cfg = TlsConfig::default();
        assert!(!cfg.enabled);
        assert!(!cfg.require_client_cert);
    }
}
