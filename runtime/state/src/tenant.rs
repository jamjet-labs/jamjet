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

/// The fields a tenant policy may have. Anything else is refused: a misspelt
/// field (`blocked_tool`) would otherwise be dropped and its rules lost.
pub const TENANT_POLICY_FIELDS: &[&str] =
    &["blocked_tools", "require_approval_for", "model_allowlist"];

/// Parse a tenant policy strictly: `null` is no policy; otherwise it must be an
/// object with only [`TENANT_POLICY_FIELDS`], each of the right type.
pub fn parse_tenant_policy(
    value: &serde_json::Value,
) -> Result<Option<jamjet_ir::workflow::PolicySetIr>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let obj = value
        .as_object()
        .ok_or_else(|| "a tenant policy must be a JSON object".to_string())?;
    if let Some(field) = obj
        .keys()
        .find(|k| !TENANT_POLICY_FIELDS.contains(&k.as_str()))
    {
        return Err(format!(
            "unknown tenant policy field {field:?}; the fields are {}",
            TENANT_POLICY_FIELDS.join(", ")
        ));
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|e| format!("tenant policy does not parse: {e}"))
}

impl Tenant {
    /// The tenant's policy set, parsed strictly by [`parse_tenant_policy`].
    ///
    /// `Err` for a stored policy that does not parse. It used to read as "no
    /// policy" (`.ok()`), which silently dropped the one layer a developer
    /// cannot rewrite; callers on the enforcement path refuse instead.
    pub fn policy_set(&self) -> Result<Option<jamjet_ir::workflow::PolicySetIr>, String> {
        match &self.policy {
            None => Ok(None),
            Some(v) => parse_tenant_policy(v),
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

#[cfg(test)]
mod policy_field_tests {
    use super::*;

    /// `TENANT_POLICY_FIELDS` re-declares `PolicySetIr`'s fields. If they drift,
    /// a new policy field is refused at `PUT /tenants` and a stored policy using
    /// it blocks every node of the tenant. Pin them together.
    #[test]
    fn tenant_policy_fields_are_exactly_policy_set_ir_fields() {
        let ir = serde_json::to_value(jamjet_ir::workflow::PolicySetIr::default()).unwrap();
        let mut from_ir: Vec<&str> = ir.as_object().unwrap().keys().map(String::as_str).collect();
        let mut declared: Vec<&str> = TENANT_POLICY_FIELDS.to_vec();
        from_ir.sort_unstable();
        declared.sort_unstable();
        assert_eq!(declared, from_ir);
    }
}
