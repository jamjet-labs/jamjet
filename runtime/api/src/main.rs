use jamjet_agents::{InMemoryAgentRegistry, SqliteAgentRegistry};
use jamjet_api::{config::ApiConfig, routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditEnricher, NoopAuditBackend, SqliteAuditBackend};
use jamjet_state::{InMemoryBackend, SqliteBackend};
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env if present
    dotenvy::dotenv().ok();

    // Tracing
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = ApiConfig::default();
    let storage_backend = std::env::var("STORAGE_BACKEND").unwrap_or_default();
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

    // In dev mode (or when JAMJET_EMBED_WORKERS is set), run an in-process worker
    // pool so submitted workflows actually execute. In production, run dedicated
    // worker processes against the same state backend instead. The base backend
    // claims across all tenants, so workers drain every execution's queue.
    if config.dev_mode || std::env::var("JAMJET_EMBED_WORKERS").is_ok() {
        let model_registry = Arc::new(jamjet_models::registry::registry_from_env());
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
            );
        // Detaching the handles lets the workers run for the lifetime of the process
        // (same pattern as the scheduler above).
        let _worker_handles = pool.spawn();
        info!("Embedded worker pool started (dev mode)");
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
