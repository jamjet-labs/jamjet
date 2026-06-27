use jamjet_agents::{InMemoryAgentRegistry, SqliteAgentRegistry};
use jamjet_api::{config::ApiConfig, routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditEnricher, NoopAuditBackend, SqliteAuditBackend};
use jamjet_state::{InMemoryBackend, SqliteBackend};
use std::sync::Arc;
use tracing::info;

/// Parse a `JAMJET_STORE_TERM` value: a non-negative integer (the promotion
/// generation). A present-but-invalid value is an error, not a silent no-op.
fn parse_store_term(raw: &str) -> anyhow::Result<i64> {
    let trimmed = raw.trim();
    let term = trimmed
        .parse::<i64>()
        .map_err(|e| anyhow::anyhow!("invalid JAMJET_STORE_TERM `{trimmed}`: {e}"))?;
    anyhow::ensure!(
        term >= 0,
        "JAMJET_STORE_TERM must be non-negative, got {term}"
    );
    Ok(term)
}

/// The store's failover generation from `JAMJET_STORE_TERM`. Orchestrators (a
/// LiteFS-aware entrypoint, k8s, Fly) set this to the current promotion generation
/// (LiteFS lease epoch / Postgres timeline ID) on every (re)start, so the lease
/// fence is failover-safe. Absent => `Ok(None)` (not configured). Present-but-invalid
/// => `Err` (fail startup loudly): a typo like `JAMJET_STORE_TERM=abc` must NOT be
/// silently treated as "not configured", which would leave the term at 0 and re-open
/// the failover-safety gap the operator believed they had closed.
fn store_term_from_env() -> anyhow::Result<Option<i64>> {
    match std::env::var("JAMJET_STORE_TERM") {
        Ok(raw) => parse_store_term(&raw).map(Some),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => anyhow::bail!("failed to read JAMJET_STORE_TERM: {e}"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env if present
    dotenvy::dotenv().ok();

    let config = ApiConfig::default();

    // Telemetry — installs OTLP trace + metric pipelines when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set, otherwise falls back to plain tracing.
    let otel_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();
    jamjet_telemetry::init(config.dev_mode, otel_endpoint.as_deref());
    let storage_backend = std::env::var("STORAGE_BACKEND").unwrap_or_default();
    // Parse the failover generation up front so a misconfiguration fails startup
    // before we open any backend (rather than silently disabling failover-safety).
    let configured_store_term = store_term_from_env()?;
    let storage_label = if storage_backend == "memory" {
        "memory"
    } else {
        "sqlite"
    };

    info!(
        port = config.port,
        dev_mode = config.dev_mode,
        storage = %storage_label,
        "Starting JamJet runtime"
    );

    let state = if storage_backend == "memory" {
        info!("Using in-memory storage (ephemeral, no persistence)");

        let backend = Arc::new(InMemoryBackend::new());
        if let Some(term) = configured_store_term {
            let applied = backend.set_store_term_at_least(term);
            info!(
                store_term = applied,
                "Applied failover generation from JAMJET_STORE_TERM"
            );
        }
        let backend_clone = backend.clone();
        let audit: Arc<dyn jamjet_audit::AuditBackend> = Arc::new(NoopAuditBackend);
        let enricher = Arc::new(AuditEnricher::new(Arc::clone(&audit)));

        AppState {
            backend: backend.clone() as Arc<dyn jamjet_state::StateBackend>,
            backend_for_fn: Arc::new(move |_tenant_id: &jamjet_state::TenantId| {
                backend_clone.clone() as Arc<dyn jamjet_state::StateBackend>
            }),
            agents: Arc::new(InMemoryAgentRegistry::new()),
            audit,
            enricher,
            protocols: jamjet_api::state::default_protocol_registry(),
            cron_store: None,
        }
    } else {
        // Determine database URL: env > config > SQLite dev default
        let database_url = std::env::var("DATABASE_URL")
            .ok()
            .or_else(|| config.database_url.clone())
            .unwrap_or_else(|| {
                // Local dev: store in .jamjet/ directory
                let dir = std::path::PathBuf::from(".jamjet");
                std::fs::create_dir_all(&dir).ok();
                format!("sqlite://{}", dir.join("runtime.db").display())
            });

        info!(%database_url, "Connecting to state backend");

        let backend = SqliteBackend::open(&database_url)
            .await
            .map_err(|e| anyhow::anyhow!("failed to open state backend: {e}"))?;

        // Failover-safety: raise the store's failover generation to the promotion
        // generation supplied by the orchestrator, so a lease fence minted under a
        // previous primary is rejected after a failover (the fence packs term in
        // its high bits). Without this the term stays 0 and only the per-item epoch
        // protects, which a lost-tail failover can corrupt.
        if let Some(term) = configured_store_term {
            let applied = backend
                .set_store_term_at_least(term)
                .await
                .map_err(|e| anyhow::anyhow!("failed to set store term: {e}"))?;
            info!(
                store_term = applied,
                "Applied failover generation from JAMJET_STORE_TERM"
            );
        }

        let agents = SqliteAgentRegistry::connect(&database_url)
            .await
            .map_err(|e| anyhow::anyhow!("failed to open agent registry: {e}"))?;

        // Audit log uses the same SQLite file as the state backend.
        let audit_backend = SqliteAuditBackend::open(&database_url)
            .await
            .map_err(|e| anyhow::anyhow!("failed to open audit backend: {e}"))?;
        audit_backend
            .migrate()
            .await
            .map_err(|e| anyhow::anyhow!("failed to migrate audit log: {e}"))?;
        let audit: Arc<dyn jamjet_audit::AuditBackend> = Arc::new(audit_backend);
        let enricher = Arc::new(AuditEnricher::new(Arc::clone(&audit)));

        let sqlite = Arc::new(backend);
        let sqlite_clone = sqlite.clone();

        AppState {
            backend: sqlite.clone() as Arc<dyn jamjet_state::StateBackend>,
            backend_for_fn: Arc::new(move |tenant_id: &jamjet_state::TenantId| {
                Arc::new(sqlite_clone.for_tenant(tenant_id.clone()))
                    as Arc<dyn jamjet_state::StateBackend>
            }),
            agents: Arc::new(agents),
            audit,
            enricher,
            protocols: jamjet_api::state::default_protocol_registry(),
            cron_store: Some(Arc::new(jamjet_timers::CronStore::new(sqlite.pool()))),
        }
    };

    // Spawn the scheduler as a background task — auto-chains nodes after completion.
    let scheduler = jamjet_scheduler::Scheduler::new(state.backend.clone());
    tokio::spawn(async move { scheduler.run().await });
    info!("Scheduler started");

    // Spawn the async projector alongside the scheduler.  Maintains the durable
    // proj_approvals read-model so the approvals endpoint serves from a projection
    // instead of replaying the event log on every request.
    // No shutdown signal wired today — matches the scheduler; follow-up: F-2h-shutdown.
    let projector = jamjet_scheduler::Projector::new(state.backend.clone());
    tokio::spawn(async move { projector.run().await });
    info!("Projector started");

    // In dev mode (or when JAMJET_EMBED_WORKERS is set), run an in-process worker
    // pool so submitted workflows actually execute. In production, run dedicated
    // worker processes against the same state backend instead. The base backend
    // claims across all tenants, so workers drain every execution's queue.
    if config.dev_mode || std::env::var("JAMJET_EMBED_WORKERS").is_ok() {
        let model_registry = Arc::new(
            jamjet_models::registry::registry_from_env_checked()
                .await
                .map_err(|e| anyhow::anyhow!("model seam coverage guard failed: {e}"))?,
        );
        let pool = jamjet_worker::default_pool(state.backend.clone())
            .with_executor(
                "model",
                Arc::new(jamjet_worker::ModelNodeExecutor::new(
                    model_registry.clone(),
                )),
            )
            .with_executor(
                "eval",
                Arc::new(jamjet_worker::EvalExecutor::new(model_registry.clone())),
            )
            .with_executor(
                "condition",
                Arc::new(jamjet_worker::ConditionNodeExecutor::new()),
            );
        // Detaching the handles lets the workers run for the lifetime of the process
        // (same pattern as the scheduler above).
        let _worker_handles = pool.spawn();
        info!("Embedded worker pool started (dev mode)");
    }

    // In dev mode (or when JAMJET_EMBED_CRON is set), run the cron scheduler in
    // process so YAML-declared schedules fire. It POSTs to this server's own
    // /executions endpoint. Production should run a dedicated cron process.
    if (config.dev_mode || std::env::var("JAMJET_EMBED_CRON").is_ok())
        && storage_backend != "memory"
    {
        let host = if config.bind == "0.0.0.0" {
            "127.0.0.1".to_string()
        } else {
            config.bind.clone()
        };
        let self_url = format!("http://{}:{}", host, config.port);
        let database_url = std::env::var("DATABASE_URL")
            .ok()
            .or_else(|| config.database_url.clone())
            .unwrap_or_else(|| {
                format!(
                    "sqlite://{}",
                    std::path::PathBuf::from(".jamjet")
                        .join("runtime.db")
                        .display()
                )
            });
        match jamjet_state::SqliteBackend::open(&database_url).await {
            Ok(b) => {
                let cron = jamjet_timers::CronScheduler::new(b.pool(), self_url);
                tokio::spawn(cron.run());
                info!("Cron scheduler started (dev mode)");
            }
            Err(e) => tracing::warn!(error = %e, "cron scheduler: failed to open pool; skipping"),
        }
    }

    let router = build_router_with_opts(state, config.dev_mode);
    let addr = format!("{}:{}", config.bind, config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    info!("JamJet runtime listening on {addr}");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("Shutting down");
    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl-C handler");
}

#[cfg(test)]
mod tests {
    use super::parse_store_term;

    #[test]
    fn parses_valid_non_negative() {
        assert_eq!(parse_store_term("5").unwrap(), 5);
        assert_eq!(parse_store_term("  0 ").unwrap(), 0);
    }

    #[test]
    fn rejects_invalid_and_negative() {
        // A typo must fail loud, not silently disable failover-safety.
        assert!(parse_store_term("abc").is_err());
        assert!(parse_store_term("-1").is_err());
        assert!(parse_store_term("").is_err());
    }
}
