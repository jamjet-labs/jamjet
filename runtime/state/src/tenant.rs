//! Multi-tenant isolation types and constants.
//!
//! All tenant-scoped data is partitioned by `TenantId`. The default
//! tenant (`"default"`) ensures backward compatibility with single-tenant
//! deployments.

use serde::{Deserialize, Serialize};

/// The default tenant for single-tenant and backward-compatible deployments.
pub const DEFAULT_TENANT: &str = "default";

/// Strongly typed tenant identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantId(pub String);

impl TenantId {
    pub fn default_tenant() -> Self {
        Self(DEFAULT_TENANT.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TenantId {
    fn default() -> Self {
        Self::default_tenant()
    }
}

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&str> for TenantId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for TenantId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Tenant metadata stored in the `tenants` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    pub id: TenantId,
    pub name: String,
    pub status: TenantStatus,
    /// Per-tenant policy set (JSON, applied between global and workflow policies).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<serde_json::Value>,
    /// Per-tenant resource limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits: Option<TenantLimits>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Tenant lifecycle status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantStatus {
    Active,
    Suspended,
    Archived,
}

impl TenantStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Archived => "archived",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "suspended" => Self::Suspended,
            "archived" => Self::Archived,
            _ => Self::Active,
        }
    }
}

/// Per-tenant resource and cost limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantLimits {
    /// Maximum concurrent workflow executions for this tenant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_executions: Option<u32>,
    /// Maximum workflow definitions this tenant can create.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_workflows: Option<u32>,
    /// Monthly token budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens_per_month: Option<u64>,
    /// Monthly cost budget in USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cost_per_month_usd: Option<f64>,
}
