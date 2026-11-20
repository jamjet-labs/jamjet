//! What a node's work leaves behind, on Postgres: the tool effects a commit
//! recorded, the reservations that keep two attempts of a node from firing
//! one effect twice, and the content-addressed artifact store.
//!
//! None of these takes a run's lock: each has its own key, and each write is
//! one statement that the key decides. A reservation is one upsert that
//! takes the key only while no live reservation holds it, so there is no
//! moment between "nobody holds it" and "it is mine"; when it lapses is the
//! database's time. An artifact keeps the first row stored for its tenant
//! and hash.
//!
//! A tenant's view keeps these under its tenant, and the unscoped store
//! under `default`, as the two SQLite stores do. The effects a commit
//! records carry the run's tenant (see `commit.rs`), so the unscoped store
//! reads every tenant's and a view its own.

use super::runs::{decode, new_row_tenant};
use super::tx::{no_nul, statement_error};
use super::PgBackend;
use crate::artifact::ArtifactRef;
use crate::backend::{BackendResult, ReserveOutcome, MAX_RESERVATION_TTL};
use chrono::{DateTime, Utc};
use jamjet_core::workflow::ExecutionId;
use sqlx::Row;
use std::time::Duration;

// ── Tool effects ─────────────────────────────────────────────────────────────

/// The recorded result for `key`. A key with a NUL cannot have been
/// recorded, since no row can hold one.
pub(super) async fn get_tool_effect(
    b: &PgBackend,
    key: &str,
) -> BackendResult<Option<serde_json::Value>> {
    if key.contains('\0') {
        return Ok(None);
    }
    let query = match b.scope() {
        None => {
            sqlx::query_scalar("SELECT result_json FROM tool_effects WHERE idempotency_key = $1")
                .bind(key)
        }
        Some(tenant) => sqlx::query_scalar(
            "SELECT result_json FROM tool_effects WHERE idempotency_key = $1 AND tenant_id = $2",
        )
        .bind(key)
        .bind(tenant),
    };
    let result: Option<String> = query
        .fetch_optional(&b.pool)
        .await
        .map_err(statement_error)?;
    Ok(result.map(|r| serde_json::from_str(&r)).transpose()?)
}

// ── Reservations ─────────────────────────────────────────────────────────────

/// One upsert. It takes the key when nobody holds it, when the reservation
/// there has lapsed by the database's clock, or when it is this attempt's
/// own (the same owner under the same fence), which renews it. The expiry is
/// the database's time plus `ttl`, held to [`MAX_RESERVATION_TTL`]. A live
/// reservation of anyone else changes nothing and is answered `Held`, with
/// its holder and expiry.
pub(super) async fn reserve_tool_effect(
    b: &PgBackend,
    key: &str,
    execution_id: &ExecutionId,
    node_id: &str,
    owner: &str,
    lease_fence: i64,
    ttl: Duration,
) -> BackendResult<ReserveOutcome> {
    no_nul("idempotency key", key)?;
    no_nul("node id", node_id)?;
    no_nul("owner", owner)?;
    let tenant = new_row_tenant(b);
    let held_for = ttl.min(MAX_RESERVATION_TTL).as_secs_f64();
    let taken = sqlx::query(
        "INSERT INTO tool_reservations (tenant_id, idempotency_key, execution_id, node_id, owner, \
           lease_fence, expires_at, reserved_at) \
         VALUES ($1, $2, $3, $4, $5, $6, statement_timestamp() + make_interval(secs => $7), \
           statement_timestamp()) \
         ON CONFLICT (tenant_id, idempotency_key) DO UPDATE SET owner = EXCLUDED.owner, \
           lease_fence = EXCLUDED.lease_fence, expires_at = EXCLUDED.expires_at, \
           reserved_at = EXCLUDED.reserved_at \
         WHERE tool_reservations.expires_at <= statement_timestamp() \
           OR (tool_reservations.owner = EXCLUDED.owner \
               AND tool_reservations.lease_fence = EXCLUDED.lease_fence)",
    )
    .bind(tenant)
    .bind(key)
    .bind(execution_id.0)
    .bind(node_id)
    .bind(owner)
    .bind(lease_fence)
    .bind(held_for)
    .execute(&b.pool)
    .await
    .map_err(statement_error)?
    .rows_affected();
    if taken > 0 {
        return Ok(ReserveOutcome::Acquired);
    }
    // A live reservation blocked the upsert: say whose, and until when, so
    // the caller can bound its wait.
    let holder: Option<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT owner, expires_at FROM tool_reservations \
         WHERE tenant_id = $1 AND idempotency_key = $2",
    )
    .bind(tenant)
    .bind(key)
    .fetch_optional(&b.pool)
    .await
    .map_err(statement_error)?;
    Ok(match holder {
        Some((owner, expires_at)) => ReserveOutcome::Held {
            owner,
            expires_at: expires_at.to_rfc3339(),
        },
        // Released in between. The upsert was refused, so the key is not
        // ours: refuse rather than hand out a key someone held.
        None => ReserveOutcome::Held {
            owner: "unknown".to_string(),
            expires_at: Utc::now().to_rfc3339(),
        },
    })
}

/// Delete the reservation only while `owner` holds it under `lease_fence`.
/// Anything else is a silent no-op.
pub(super) async fn release_tool_reservation(
    b: &PgBackend,
    key: &str,
    owner: &str,
    lease_fence: i64,
) -> BackendResult<()> {
    if key.contains('\0') || owner.contains('\0') {
        // No row can hold it, so there is nothing to release.
        return Ok(());
    }
    sqlx::query(
        "DELETE FROM tool_reservations \
         WHERE tenant_id = $1 AND idempotency_key = $2 AND owner = $3 AND lease_fence = $4",
    )
    .bind(new_row_tenant(b))
    .bind(key)
    .bind(owner)
    .bind(lease_fence)
    .execute(&b.pool)
    .await
    .map_err(statement_error)?;
    Ok(())
}

// ── Artifacts ────────────────────────────────────────────────────────────────

/// Store `bytes` under their SHA-256. A row already there for the tenant and
/// hash is kept as it is, so the answer carries the media type stored first,
/// read back in a statement of its own: a row another engine inserted while
/// this insert waited is not visible to the insert's own statement.
pub(super) async fn put_artifact(
    b: &PgBackend,
    bytes: &[u8],
    media_type: Option<&str>,
) -> BackendResult<ArtifactRef> {
    if let Some(media) = media_type {
        no_nul("media type", media)?;
    }
    let hash = crate::hashing::sha256_hex(bytes);
    let size = bytes.len() as u64;
    let tenant = new_row_tenant(b);
    sqlx::query(
        "INSERT INTO artifacts (tenant_id, hash, bytes, size, media_type, created_at) \
         VALUES ($1, $2, $3, $4, $5, statement_timestamp()) \
         ON CONFLICT (tenant_id, hash) DO NOTHING",
    )
    .bind(tenant)
    .bind(&hash)
    .bind(bytes)
    .bind(i64::try_from(size).unwrap_or(i64::MAX))
    .bind(media_type)
    .execute(&b.pool)
    .await
    .map_err(statement_error)?;
    let row = sqlx::query("SELECT media_type FROM artifacts WHERE tenant_id = $1 AND hash = $2")
        .bind(tenant)
        .bind(&hash)
        .fetch_one(&b.pool)
        .await
        .map_err(statement_error)?;
    Ok(ArtifactRef {
        hash,
        size,
        media_type: row.try_get("media_type").map_err(decode)?,
    })
}

/// The artifact's bytes in this view's tenant. A hash with a NUL is never
/// stored.
pub(super) async fn get_artifact(b: &PgBackend, hash: &str) -> BackendResult<Option<Vec<u8>>> {
    if hash.contains('\0') {
        return Ok(None);
    }
    sqlx::query_scalar("SELECT bytes FROM artifacts WHERE tenant_id = $1 AND hash = $2")
        .bind(new_row_tenant(b))
        .bind(hash)
        .fetch_optional(&b.pool)
        .await
        .map_err(statement_error)
}

/// The size recorded when the artifact was stored, without reading it.
pub(super) async fn artifact_size(b: &PgBackend, hash: &str) -> BackendResult<Option<u64>> {
    if hash.contains('\0') {
        return Ok(None);
    }
    let size: Option<i64> =
        sqlx::query_scalar("SELECT size FROM artifacts WHERE tenant_id = $1 AND hash = $2")
            .bind(new_row_tenant(b))
            .bind(hash)
            .fetch_optional(&b.pool)
            .await
            .map_err(statement_error)?;
    Ok(size.map(|s| s.max(0) as u64))
}
