use jamjet_agents::SqliteAgentRegistry;
use jamjet_api::{config::ApiConfig, routes::build_router, state::AppState};
use jamjet_audit::{AuditEnricher, SqliteAuditBackend};
use jamjet_state::SqliteBackend;
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

    info!(
        port = config.port,
        dev_mode = config.dev_mode,
        "Starting JamJet runtime"
    );

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

    let state = AppState {
        backend: Arc::new(backend),
        agents: Arc::new(agents),
        audit,
        enricher,
        protocols: jamjet_api::state::default_protocol_registry(),
    };

    let router = build_router(state);
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
