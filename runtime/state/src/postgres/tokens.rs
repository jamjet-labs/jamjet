//! Who may call the engine, on Postgres: API tokens and tenant rows.
//!
//! A token is stored as the hash of its text, the same function SQLite
//! uses, so a token issued on one store validates the same way on the
//! other. A token is created in the view's tenant, or `default`, and looked
//! up by its hash across tenants, as both SQLite stores do: the token's own
//! row says which tenant it acts in. A validation stamps when the token was
//! last used in the statement that finds it, and judges its expiry by the
//! database's clock.
//!
//! Tenants are read by either role. Creating and changing them is a view's
//! job, as on SQLite, where only the tenant-scoped store does it: the
//! unscoped store answers what the trait's default answers. A view may
//! create or change any tenant, as the tenant routes do through the default
//! tenant's view.

use super::runs::{decode, new_row_tenant};
use super::tx::{no_nul, statement_error};
use super::PgBackend;
use crate::backend::{ApiToken, BackendResult, StateBackendError};
use crate::tenant::{Tenant, TenantId, TenantStatus};
use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::Row;

// ── API tokens ───────────────────────────────────────────────────────────────

pub(super) async fn create_token(
    b: &PgBackend,
    name: &str,
    role: &str,
) -> BackendResult<(String, ApiToken)> {
    no_nul("token name", name)?;
    no_nul("role", role)?;
    let token = crate::api_token::generate();
    let id = uuid::Uuid::new_v4().to_string();
    let tenant = new_row_tenant(b);
    let created_at: DateTime<Utc> = sqlx::query_scalar(
        "INSERT INTO api_tokens (id, token_hash, name, role, tenant_id, created_at) \
         VALUES ($1, $2, $3, $4, $5, statement_timestamp()) RETURNING created_at",
    )
    .bind(&id)
    .bind(crate::api_token::hash(&token))
    .bind(name)
    .bind(role)
    .bind(tenant)
    .fetch_one(&b.pool)
    .await
    .map_err(statement_error)?;
    let info = ApiToken {
        id,
        name: name.to_string(),
        role: role.to_string(),
        created_at,
        expires_at: None,
        tenant_id: tenant.to_string(),
    };
    Ok((token, info))
}

/// The token's row, if it is neither revoked nor expired, with its last use
/// stamped in the same statement.
pub(super) async fn validate_token(b: &PgBackend, token: &str) -> BackendResult<Option<ApiToken>> {
    let row = sqlx::query(
        "UPDATE api_tokens SET last_used_at = statement_timestamp() \
         WHERE token_hash = $1 AND revoked_at IS NULL \
           AND (expires_at IS NULL OR expires_at > statement_timestamp()) \
         RETURNING id, name, role, created_at, expires_at, tenant_id",
    )
    .bind(crate::api_token::hash(token))
    .fetch_optional(&b.pool)
    .await
    .map_err(statement_error)?;
    let Some(row) = row else { return Ok(None) };
    Ok(Some(ApiToken {
        id: row.try_get("id").map_err(decode)?,
        name: row.try_get("name").map_err(decode)?,
        role: row.try_get("role").map_err(decode)?,
        created_at: row.try_get("created_at").map_err(decode)?,
        expires_at: row.try_get("expires_at").map_err(decode)?,
        // The token's own tenant, never a default: a token moved into
        // `default` by a read error would act on that tenant's data.
        tenant_id: row.try_get("tenant_id").map_err(decode)?,
    }))
}

// ── Tenants ──────────────────────────────────────────────────────────────────

/// What the trait's default answers for a tenant write: the unscoped store
/// leaves tenant writes to a view.
fn not_on_the_unscoped_store() -> StateBackendError {
    StateBackendError::Database("tenant management not supported".into())
}

fn row_to_tenant(row: &PgRow) -> BackendResult<Tenant> {
    let policy: Option<String> = row.try_get("policy_json").map_err(decode)?;
    let limits: Option<String> = row.try_get("limits_json").map_err(decode)?;
    let status: String = row.try_get("status").map_err(decode)?;
    Ok(Tenant {
        id: TenantId(row.try_get("id").map_err(decode)?),
        name: row.try_get("name").map_err(decode)?,
        status: TenantStatus::parse(&status),
        policy: policy.map(|p| serde_json::from_str(&p)).transpose()?,
        limits: limits.map(|l| serde_json::from_str(&l)).transpose()?,
        created_at: row.try_get("created_at").map_err(decode)?,
        updated_at: row.try_get("updated_at").map_err(decode)?,
    })
}

/// The tenant's policy and limits as stored text.
fn tenant_json(tenant: &Tenant) -> BackendResult<(Option<String>, Option<String>)> {
    no_nul("tenant id", tenant.id.as_str())?;
    no_nul("tenant name", &tenant.name)?;
    let policy = tenant
        .policy
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let limits = tenant
        .limits
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    Ok((policy, limits))
}

pub(super) async fn create_tenant(b: &PgBackend, tenant: Tenant) -> BackendResult<()> {
    if b.scope().is_none() {
        return Err(not_on_the_unscoped_store());
    }
    let (policy, limits) = tenant_json(&tenant)?;
    sqlx::query(
        "INSERT INTO tenants (id, name, status, policy_json, limits_json, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(tenant.id.as_str())
    .bind(&tenant.name)
    .bind(tenant.status.as_str())
    .bind(policy)
    .bind(limits)
    .bind(tenant.created_at)
    .bind(tenant.updated_at)
    .execute(&b.pool)
    .await
    .map_err(statement_error)?;
    Ok(())
}

/// A tenant id with a NUL is never stored.
pub(super) async fn get_tenant(b: &PgBackend, id: &TenantId) -> BackendResult<Option<Tenant>> {
    if id.as_str().contains('\0') {
        return Ok(None);
    }
    let row = sqlx::query("SELECT * FROM tenants WHERE id = $1")
        .bind(id.as_str())
        .fetch_optional(&b.pool)
        .await
        .map_err(statement_error)?;
    row.map(|r| row_to_tenant(&r)).transpose()
}

/// Every tenant, oldest first.
pub(super) async fn list_tenants(b: &PgBackend) -> BackendResult<Vec<Tenant>> {
    let rows = sqlx::query("SELECT * FROM tenants ORDER BY created_at, id")
        .fetch_all(&b.pool)
        .await
        .map_err(statement_error)?;
    rows.iter().map(row_to_tenant).collect()
}

/// `NotFound` for a tenant that is not there.
pub(super) async fn update_tenant(b: &PgBackend, tenant: Tenant) -> BackendResult<()> {
    if b.scope().is_none() {
        return Err(not_on_the_unscoped_store());
    }
    let (policy, limits) = tenant_json(&tenant)?;
    let updated = sqlx::query(
        "UPDATE tenants SET name = $2, status = $3, policy_json = $4, limits_json = $5, \
         updated_at = $6 WHERE id = $1",
    )
    .bind(tenant.id.as_str())
    .bind(&tenant.name)
    .bind(tenant.status.as_str())
    .bind(policy)
    .bind(limits)
    .bind(tenant.updated_at)
    .execute(&b.pool)
    .await
    .map_err(statement_error)?
    .rows_affected();
    if updated == 0 {
        return Err(StateBackendError::NotFound(tenant.id.0));
    }
    Ok(())
}
