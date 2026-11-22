//! Opening the store a `DATABASE_URL` names. A `postgres://` or
//! `postgresql://` URL is the Postgres store ([`crate::startup::store_kind`]);
//! anything else is SQLite.
//!
//! On Postgres the server opens one pool, sized by `JAMJET_PG_*`
//! ([`PgSettings::from_env`]), and the state store, the audit log, the agent
//! registry and the outbox drain all use it. The schema must be the one this
//! build knows: `jamjet-server migrate` makes it so, once per deploy. The
//! token commands open a pool of their own of [`TOKEN_POOL_SIZE`].

use crate::approvals::ApprovalPolicy;
use crate::cli::TokenRequest;
use crate::state::AppState;
use jamjet_agents::PgAgentRegistry;
use jamjet_audit::{AuditBackend, AuditEnricher, PgAuditBackend, SqliteAuditBackend};
use jamjet_state::{
    ApiToken, PgBackend, PgSettings, RevokedToken, SqliteBackend, StateBackend, TenantId,
    DEFAULT_TENANT,
};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// Logged once when a server starts on Postgres.
pub const PREVIEW_WARNING: &str = "Postgres store (preview)";

/// Why a server on Postgres does not run the embedded cron scheduler.
pub const CRON_REFUSAL: &str =
    "cron is not supported on Postgres yet: each engine would fire every schedule";

/// What to do when `JAMJET_EMBED_CRON` stops a server on Postgres. The
/// Docker image sets it, so the user may not know it is on.
pub const CRON_REFUSAL_FIX: &str = "set JAMJET_EMBED_CRON=0 or unset it";

/// How often the outbox drain runs. It also runs once at startup.
pub const DRAIN_EVERY: Duration = Duration::from_secs(2);

/// The most outbox rows one drain seals.
pub const DRAIN_LIMIT: u32 = 100;

/// Connections the token commands' pool holds.
pub const TOKEN_POOL_SIZE: u32 = 2;

/// The server's Postgres store: one pool from `JAMJET_PG_*`, refused unless
/// the schema is current.
pub async fn open_postgres(url: &str) -> anyhow::Result<PgBackend> {
    let settings = PgSettings::from_env().map_err(anyhow::Error::msg)?;
    PgBackend::connect(url, settings)
        .await
        .map_err(|why| anyhow::anyhow!("refusing to start on Postgres: {why}"))
}

/// The server's state on `pg`: the state store, its tenant views, the audit
/// log and the agent registry, all on `pg`'s one pool. No cron store, so
/// `/cron` answers as it does on the memory store.
pub async fn postgres_state(
    pg: &PgBackend,
    approval_policy: ApprovalPolicy,
) -> anyhow::Result<AppState> {
    let audit: Arc<dyn AuditBackend> = Arc::new(PgAuditBackend::new(pg.pool()));
    let enricher = Arc::new(AuditEnricher::new(Arc::clone(&audit)));
    // A rotated key: on record before anything else is sealed.
    if crate::cli::seal_key_change(audit.as_ref(), &enricher).await? {
        tracing::warn!(
            "the audit signing key changed since the last entry; recorded as audit_key_changed"
        );
    }
    let scoped = pg.clone();
    Ok(AppState {
        backend: Arc::new(pg.clone()),
        backend_for_fn: Arc::new(move |tenant_id: &TenantId| {
            Arc::new(scoped.for_tenant(tenant_id.clone())) as Arc<dyn StateBackend>
        }),
        agents: Arc::new(PgAgentRegistry::new(pg.pool())),
        audit,
        enricher,
        protocols: crate::state::default_protocol_registry(),
        cron_store: None,
        approval_policy,
    })
}

/// Seal what the inline seals missed: run the outbox drain now and then
/// every [`DRAIN_EVERY`], for as long as the process runs. A drain that
/// fails logs why and the next one tries again.
pub fn spawn_outbox_drain(
    pg: PgBackend,
    enricher: Arc<AuditEnricher>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(DRAIN_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // The first tick is at once: the drain at startup.
            every.tick().await;
            let report = jamjet_audit::outbox::drain(
                &pg,
                &enricher,
                jamjet_audit::outbox::DUE_AFTER,
                DRAIN_LIMIT,
            )
            .await;
            if report.sealed > 0 || report.failed > 0 {
                info!(
                    sealed = report.sealed,
                    failed = report.failed,
                    gaps_recorded = report.gaps_recorded,
                    "audit outbox drained"
                );
            }
        }
    })
}

/// The store the token commands open: the server's database, which must
/// hold its schema already.
pub enum TokenStore {
    Sqlite { backend: SqliteBackend, url: String },
    Postgres(PgBackend),
}

impl TokenStore {
    /// Open the database at `url`. SQLite refuses a file without the
    /// server's schema; Postgres a schema that is not current.
    pub async fn open(url: &str) -> anyhow::Result<Self> {
        if crate::startup::is_postgres_url(url) {
            let settings = PgSettings {
                pool_size: TOKEN_POOL_SIZE,
                ..PgSettings::from_env().map_err(anyhow::Error::msg)?
            };
            let pg = PgBackend::connect(url, settings)
                .await
                .map_err(anyhow::Error::msg)?;
            return Ok(Self::Postgres(pg));
        }
        let backend = SqliteBackend::connect_existing(url)
            .await
            .map_err(anyhow::Error::msg)?;
        Ok(Self::Sqlite {
            backend,
            url: url.to_string(),
        })
    }

    /// The database's audit log.
    pub async fn audit(&self) -> anyhow::Result<Arc<dyn AuditBackend>> {
        Ok(match self {
            Self::Sqlite { url, .. } => Arc::new(
                SqliteAuditBackend::open(url)
                    .await
                    .map_err(|e| anyhow::anyhow!("cannot open the audit log in {url}: {e}"))?,
            ),
            Self::Postgres(pg) => Arc::new(PgAuditBackend::new(pg.pool())),
        })
    }

    fn unscoped(&self) -> &dyn StateBackend {
        match self {
            Self::Sqlite { backend, .. } => backend,
            Self::Postgres(pg) => pg,
        }
    }

    fn scoped(&self, tenant: TenantId) -> Box<dyn StateBackend> {
        match self {
            Self::Sqlite { backend, .. } => Box::new(backend.for_tenant(tenant)),
            Self::Postgres(pg) => Box::new(pg.for_tenant(tenant)),
        }
    }

    /// Issue a token as `req` asks. Returns the token text (shown once,
    /// never stored) and what was stored. A tenant other than `default` must
    /// exist.
    pub async fn create_token(&self, req: &TokenRequest) -> anyhow::Result<(String, ApiToken)> {
        if req.tenant == DEFAULT_TENANT {
            return self
                .unscoped()
                .create_token(&req.name, &req.role)
                .await
                .map_err(|e| anyhow::anyhow!("failed to create the token: {e}"));
        }
        let tenant = TenantId::from(req.tenant.clone());
        if self
            .unscoped()
            .get_tenant(&tenant)
            .await
            .map_err(|e| anyhow::anyhow!("failed to read tenant {:?}: {e}", req.tenant))?
            .is_none()
        {
            anyhow::bail!(
                "no tenant {:?}; create it first (POST /tenants, operator role)",
                req.tenant
            );
        }
        self.scoped(tenant)
            .create_token(&req.name, &req.role)
            .await
            .map_err(|e| anyhow::anyhow!("failed to create the token: {e}"))
    }

    /// Revoke the token with this id: what was revoked, or `None` when there
    /// is no live token with it.
    pub async fn revoke_token(&self, id: &str) -> anyhow::Result<Option<RevokedToken>> {
        match self {
            Self::Sqlite { backend, .. } => backend.revoke_token(id).await,
            Self::Postgres(pg) => pg.revoke_token(id).await,
        }
        .map_err(|e| anyhow::anyhow!("failed to revoke the token: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deployment guide states these: the drain every 2 s, and 2
    /// connections per token command in the connection budget.
    #[test]
    fn the_documented_numbers_hold() {
        assert_eq!(DRAIN_EVERY, Duration::from_secs(2));
        assert_eq!(TOKEN_POOL_SIZE, 2);
        const { assert!(TOKEN_POOL_SIZE >= jamjet_state::postgres::MIN_POOL_SIZE) };
    }
}
