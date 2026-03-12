//! OAuth 2.0 delegated agent authorization (Phase 4.17–4.21).
//!
//! Implements the IETF OAuth 2.0 pattern for agents acting on behalf of users:
//!
//! - **RFC 8693 token exchange** — agent exchanges user's token for a narrowly-scoped
//!   agent token with the authorization server.
//! - **Scope narrowing** — agent tokens always have equal or fewer permissions than
//!   the user who triggered the workflow.
//! - **Per-step scoping** — different workflow nodes request different scope sets.
//! - **Audit trail** — every token exchange and API call is logged.
//! - **Revocation handling** — expired/revoked tokens trigger clean errors with
//!   escalation to human.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tracing::{info, warn};

// ── OAuth configuration ─────────────────────────────────────────────────────

/// OAuth 2.0 delegated auth configuration for an agent or workflow node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthConfig {
    /// OAuth 2.0 authorization server token endpoint.
    pub token_endpoint: String,

    /// Grant type. Default: RFC 8693 token exchange.
    #[serde(default = "default_grant_type")]
    pub grant_type: String,

    /// Client ID for this agent (env var name or literal).
    pub client_id: String,

    /// Client secret for this agent (env var name or literal).
    pub client_secret: String,

    /// Where the subject (user) token comes from.
    /// Values: "workflow_context", "header", "env"
    #[serde(default = "default_subject_token_source")]
    pub subject_token_source: String,

    /// Scopes to request from the authorization server.
    #[serde(default)]
    pub requested_scopes: Vec<String>,

    /// Audience for the token exchange (target service).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
}

fn default_grant_type() -> String {
    "urn:ietf:params:oauth:grant-type:token-exchange".to_string()
}

fn default_subject_token_source() -> String {
    "workflow_context".to_string()
}

// ── Token exchange request/response ─────────────────────────────────────────

/// Parameters for an RFC 8693 token exchange request.
#[derive(Debug, Clone, Serialize)]
pub struct TokenExchangeRequest {
    pub grant_type: String,
    pub subject_token: String,
    pub subject_token_type: String,
    pub requested_token_type: String,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
}

/// Response from a token exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenExchangeResponse {
    pub access_token: String,
    pub token_type: String,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
    pub issued_token_type: Option<String>,
}

/// An agent token with metadata for audit and lifecycle management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToken {
    /// The access token value.
    pub access_token: String,
    /// Token type (usually "Bearer").
    pub token_type: String,
    /// When the token expires.
    pub expires_at: Option<DateTime<Utc>>,
    /// Granted scopes.
    pub scopes: HashSet<String>,
    /// The agent that requested this token.
    pub agent_id: String,
    /// The user whose token was exchanged.
    pub user_id: Option<String>,
    /// Audience (target service).
    pub audience: Option<String>,
    /// When the exchange occurred.
    pub exchanged_at: DateTime<Utc>,
    /// Whether this token has been revoked.
    pub revoked: bool,
}

impl AgentToken {
    /// Check if the token is still valid (not expired, not revoked).
    pub fn is_valid(&self) -> bool {
        if self.revoked {
            return false;
        }
        if let Some(expires_at) = self.expires_at {
            if Utc::now() >= expires_at {
                return false;
            }
        }
        true
    }

    /// Check if the token has the required scope.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }

    /// Check if the token has all required scopes.
    pub fn has_all_scopes(&self, required: &[&str]) -> bool {
        required.iter().all(|s| self.scopes.contains(*s))
    }
}

// ── Scope narrowing ─────────────────────────────────────────────────────────

/// Enforce scope narrowing: the requested scopes must be a subset of the
/// user's original scopes.
///
/// Returns the narrowed scopes (intersection of requested and allowed).
pub fn narrow_scopes(
    requested: &[String],
    user_scopes: &[String],
) -> Result<Vec<String>, OAuthError> {
    let user_set: HashSet<&str> = user_scopes.iter().map(|s| s.as_str()).collect();

    let mut narrowed = Vec::new();
    let mut rejected = Vec::new();

    for scope in requested {
        if user_set.contains(scope.as_str()) {
            narrowed.push(scope.clone());
        } else {
            rejected.push(scope.clone());
        }
    }

    if !rejected.is_empty() {
        warn!(
            rejected = ?rejected,
            "Scope narrowing: agent requested scopes not in user's token"
        );
    }

    if narrowed.is_empty() && !requested.is_empty() {
        return Err(OAuthError::ScopeNarrowingFailed {
            requested: requested.to_vec(),
            available: user_scopes.to_vec(),
        });
    }

    Ok(narrowed)
}

// ── Per-step scope configuration ────────────────────────────────────────────

/// Per-node OAuth scope configuration embedded in workflow IR.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeOAuthScopes {
    /// Scopes this node requires.
    pub required_scopes: Vec<String>,
    /// Additional audience override for this node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
}

/// Resolve the effective scopes for a workflow node.
///
/// Merges agent-level config with node-level overrides, then narrows
/// against the user's available scopes.
pub fn resolve_node_scopes(
    agent_config: &OAuthConfig,
    node_scopes: Option<&NodeOAuthScopes>,
    user_scopes: &[String],
) -> Result<Vec<String>, OAuthError> {
    let requested = match node_scopes {
        Some(ns) if !ns.required_scopes.is_empty() => &ns.required_scopes,
        _ => &agent_config.requested_scopes,
    };

    narrow_scopes(requested, user_scopes)
}

// ── Token exchange (HTTP) ───────────────────────────────────────────────────

/// Perform an RFC 8693 token exchange with the authorization server.
///
/// This is a blocking HTTP call — designed to be called from async context.
pub async fn exchange_token(
    config: &OAuthConfig,
    subject_token: &str,
    effective_scopes: &[String],
    agent_id: &str,
) -> Result<AgentToken, OAuthError> {
    let scope_str = effective_scopes.join(" ");

    let params = [
        ("grant_type", config.grant_type.as_str()),
        ("subject_token", subject_token),
        (
            "subject_token_type",
            "urn:ietf:params:oauth:token-type:access_token",
        ),
        (
            "requested_token_type",
            "urn:ietf:params:oauth:token-type:access_token",
        ),
        ("scope", &scope_str),
        ("client_id", &config.client_id),
        ("client_secret", &config.client_secret),
    ];

    let client = reqwest::Client::new();
    let mut request = client.post(&config.token_endpoint).form(&params);

    if let Some(audience) = &config.audience {
        request = request.query(&[("audience", audience)]);
    }

    let response = request
        .send()
        .await
        .map_err(|e| OAuthError::NetworkError(e.to_string()))?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        return Err(OAuthError::TokenExchangeFailed { status, body });
    }

    let token_response: TokenExchangeResponse = response
        .json()
        .await
        .map_err(|e| OAuthError::ParseError(e.to_string()))?;

    let expires_at = token_response
        .expires_in
        .map(|secs| Utc::now() + Duration::seconds(secs as i64));

    let scopes: HashSet<String> = token_response
        .scope
        .as_deref()
        .unwrap_or(&scope_str)
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();

    info!(
        agent_id = agent_id,
        scopes = ?scopes,
        expires_in = ?token_response.expires_in,
        "Token exchange successful"
    );

    Ok(AgentToken {
        access_token: token_response.access_token,
        token_type: token_response.token_type,
        expires_at,
        scopes,
        agent_id: agent_id.to_string(),
        user_id: None,
        audience: config.audience.clone(),
        exchanged_at: Utc::now(),
        revoked: false,
    })
}

// ── Token revocation handling ───────────────────────────────────────────────

/// Error type for OAuth operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OAuthError {
    /// Token exchange HTTP request failed.
    NetworkError(String),
    /// Authorization server returned an error.
    TokenExchangeFailed { status: u16, body: String },
    /// Failed to parse token response.
    ParseError(String),
    /// Requested scopes exceed the user's available scopes.
    ScopeNarrowingFailed {
        requested: Vec<String>,
        available: Vec<String>,
    },
    /// Token has been revoked or expired during workflow execution.
    TokenRevoked { agent_id: String, reason: String },
    /// Token expired.
    TokenExpired {
        agent_id: String,
        expired_at: DateTime<Utc>,
    },
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NetworkError(e) => write!(f, "OAuth network error: {e}"),
            Self::TokenExchangeFailed { status, body } => {
                write!(f, "Token exchange failed (HTTP {status}): {body}")
            }
            Self::ParseError(e) => write!(f, "Token parse error: {e}"),
            Self::ScopeNarrowingFailed {
                requested,
                available,
            } => write!(
                f,
                "Scope narrowing failed: requested {requested:?}, available {available:?}"
            ),
            Self::TokenRevoked { agent_id, reason } => {
                write!(f, "Token revoked for agent {agent_id}: {reason}")
            }
            Self::TokenExpired {
                agent_id,
                expired_at,
            } => write!(f, "Token expired for agent {agent_id} at {expired_at}"),
        }
    }
}

impl std::error::Error for OAuthError {}

/// Check a token's validity before use, returning an appropriate error.
///
/// Called by the worker before each tool/model call that uses the agent token.
pub fn check_token_validity(token: &AgentToken) -> Result<(), OAuthError> {
    if token.revoked {
        return Err(OAuthError::TokenRevoked {
            agent_id: token.agent_id.clone(),
            reason: "token was revoked".to_string(),
        });
    }
    if let Some(expires_at) = token.expires_at {
        if Utc::now() >= expires_at {
            return Err(OAuthError::TokenExpired {
                agent_id: token.agent_id.clone(),
                expired_at: expires_at,
            });
        }
    }
    Ok(())
}

// ── Audit trail ─────────────────────────────────────────────────────────────

/// Audit event for OAuth token operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthAuditEntry {
    /// Type of operation: "token_exchange", "token_use", "token_revoked", "token_expired"
    pub operation: String,
    /// Agent that performed the operation.
    pub agent_id: String,
    /// User on whose behalf the agent acts.
    pub user_id: Option<String>,
    /// Scopes involved.
    pub scopes: Vec<String>,
    /// Target resource/audience.
    pub target: Option<String>,
    /// Whether the operation succeeded.
    pub success: bool,
    /// Error message if failed.
    pub error: Option<String>,
    /// Timestamp.
    pub timestamp: DateTime<Utc>,
}

impl OAuthAuditEntry {
    pub fn token_exchange(token: &AgentToken, success: bool, error: Option<String>) -> Self {
        Self {
            operation: "token_exchange".to_string(),
            agent_id: token.agent_id.clone(),
            user_id: token.user_id.clone(),
            scopes: token.scopes.iter().cloned().collect(),
            target: token.audience.clone(),
            success,
            error,
            timestamp: Utc::now(),
        }
    }

    pub fn token_use(
        agent_id: &str,
        scopes: &[String],
        target: &str,
        success: bool,
        error: Option<String>,
    ) -> Self {
        Self {
            operation: "token_use".to_string(),
            agent_id: agent_id.to_string(),
            user_id: None,
            scopes: scopes.to_vec(),
            target: Some(target.to_string()),
            success,
            error,
            timestamp: Utc::now(),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_narrowing_allows_subset() {
        let user_scopes = vec![
            "expenses:read".to_string(),
            "expenses:write".to_string(),
            "reports:read".to_string(),
        ];
        let requested = vec!["expenses:read".to_string(), "reports:read".to_string()];
        let result = narrow_scopes(&requested, &user_scopes).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"expenses:read".to_string()));
        assert!(result.contains(&"reports:read".to_string()));
    }

    #[test]
    fn scope_narrowing_rejects_escalation() {
        let user_scopes = vec!["expenses:read".to_string()];
        let requested = vec!["admin:all".to_string()];
        let result = narrow_scopes(&requested, &user_scopes);
        assert!(matches!(
            result,
            Err(OAuthError::ScopeNarrowingFailed { .. })
        ));
    }

    #[test]
    fn scope_narrowing_partial_match() {
        let user_scopes = vec!["expenses:read".to_string(), "reports:read".to_string()];
        let requested = vec!["expenses:read".to_string(), "admin:all".to_string()];
        let result = narrow_scopes(&requested, &user_scopes).unwrap();
        // Only the valid scope is returned.
        assert_eq!(result, vec!["expenses:read".to_string()]);
    }

    #[test]
    fn agent_token_validity() {
        let valid_token = AgentToken {
            access_token: "tok-123".to_string(),
            token_type: "Bearer".to_string(),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            scopes: ["read".to_string()].into(),
            agent_id: "agent-1".to_string(),
            user_id: Some("user-1".to_string()),
            audience: None,
            exchanged_at: Utc::now(),
            revoked: false,
        };
        assert!(valid_token.is_valid());
        assert!(valid_token.has_scope("read"));
        assert!(!valid_token.has_scope("write"));
        assert!(check_token_validity(&valid_token).is_ok());
    }

    #[test]
    fn expired_token_is_invalid() {
        let expired_token = AgentToken {
            access_token: "tok-exp".to_string(),
            token_type: "Bearer".to_string(),
            expires_at: Some(Utc::now() - Duration::hours(1)),
            scopes: HashSet::new(),
            agent_id: "agent-1".to_string(),
            user_id: None,
            audience: None,
            exchanged_at: Utc::now() - Duration::hours(2),
            revoked: false,
        };
        assert!(!expired_token.is_valid());
        assert!(matches!(
            check_token_validity(&expired_token),
            Err(OAuthError::TokenExpired { .. })
        ));
    }

    #[test]
    fn revoked_token_is_invalid() {
        let revoked_token = AgentToken {
            access_token: "tok-rev".to_string(),
            token_type: "Bearer".to_string(),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            scopes: HashSet::new(),
            agent_id: "agent-1".to_string(),
            user_id: None,
            audience: None,
            exchanged_at: Utc::now(),
            revoked: true,
        };
        assert!(!revoked_token.is_valid());
        assert!(matches!(
            check_token_validity(&revoked_token),
            Err(OAuthError::TokenRevoked { .. })
        ));
    }

    #[test]
    fn per_node_scope_resolution() {
        let agent_config = OAuthConfig {
            token_endpoint: "https://auth.example.com/token".to_string(),
            grant_type: default_grant_type(),
            client_id: "agent-1".to_string(),
            client_secret: "secret".to_string(),
            subject_token_source: "workflow_context".to_string(),
            requested_scopes: vec!["expenses:read".to_string(), "expenses:write".to_string()],
            audience: None,
        };

        let user_scopes = vec![
            "expenses:read".to_string(),
            "expenses:write".to_string(),
            "reports:read".to_string(),
        ];

        // Node with specific scope override
        let node_scopes = NodeOAuthScopes {
            required_scopes: vec!["reports:read".to_string()],
            audience: None,
        };

        let result = resolve_node_scopes(&agent_config, Some(&node_scopes), &user_scopes).unwrap();
        assert_eq!(result, vec!["reports:read".to_string()]);

        // No node override → uses agent config
        let result = resolve_node_scopes(&agent_config, None, &user_scopes).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn audit_entry_creation() {
        let token = AgentToken {
            access_token: "tok-123".to_string(),
            token_type: "Bearer".to_string(),
            expires_at: None,
            scopes: ["read".to_string(), "write".to_string()].into(),
            agent_id: "expense-processor".to_string(),
            user_id: Some("user-42".to_string()),
            audience: Some("https://api.example.com".to_string()),
            exchanged_at: Utc::now(),
            revoked: false,
        };

        let entry = OAuthAuditEntry::token_exchange(&token, true, None);
        assert_eq!(entry.operation, "token_exchange");
        assert_eq!(entry.agent_id, "expense-processor");
        assert_eq!(entry.user_id, Some("user-42".to_string()));
        assert!(entry.success);
        assert_eq!(entry.scopes.len(), 2);
    }
}
