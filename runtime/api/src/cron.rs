use crate::error::ApiError;
use crate::state::AppState;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use jamjet_state::TenantId;
use jamjet_timers::{cron_next, CronJob};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Deserialize)]
pub struct CreateCronRequest {
    name: String,
    cron_expression: String,
    workflow_id: String,
    workflow_version: Option<String>,
    input: Option<Value>,
    enabled: Option<bool>,
}

fn store(state: &AppState) -> Result<&std::sync::Arc<jamjet_timers::CronStore>, ApiError> {
    state
        .cron_store
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("cron scheduling requires the sqlite backend".into()))
}

/// `POST /cron` — create or update a cron job.
pub async fn create_cron(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<CreateCronRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let store = store(&state)?;
    // Defaults to the SDK compiler's default workflow version ("0.1.0"), which is
    // what `jamjet deploy` registers. This intentionally differs from
    // start_execution's "1.0.0" fallback; reconciling one canonical default across
    // the API is tracked as separate runtime work. The deploy flow always sends the
    // version explicitly, so this default only applies to raw POST /cron callers.
    let version = body.workflow_version.unwrap_or_else(|| "0.1.0".into());

    // The target workflow must be registered (deploy registers first).
    let backend = state.backend_for(&tenant_id);
    backend
        .get_workflow(&body.workflow_id, &version)
        .await?
        .ok_or_else(|| {
            ApiError::BadRequest(format!(
                "workflow {} v{} is not registered",
                body.workflow_id, version
            ))
        })?;

    // Validate the expression with the runtime's own parser.
    let next_run_at = cron_next(&body.cron_expression, Utc::now())
        .map_err(|e| ApiError::BadRequest(format!("invalid cron expression: {e}")))?;

    let job = CronJob {
        id: Uuid::new_v4(),
        name: body.name.clone(),
        cron_expression: body.cron_expression,
        workflow_id: body.workflow_id,
        workflow_version: version,
        input: body.input.unwrap_or_else(|| json!({})),
        enabled: body.enabled.unwrap_or(true),
        last_run_at: None,
        next_run_at,
        created_at: Utc::now(),
    };
    store.upsert(&job).await.map_err(ApiError::Internal)?;

    Ok((
        StatusCode::CREATED,
        Json(json!({ "name": job.name, "next_run_at": next_run_at.to_rfc3339() })),
    ))
}

/// `GET /cron` — list all cron jobs.
///
/// NOTE: cron jobs are GLOBAL, not tenant-scoped, in this version. The embedded
/// cron scheduler is dev-only (a single `default` tenant) and the `cron_jobs`
/// table has no tenant column, so listing returns all jobs. Tenant-scoped cron
/// (a `tenant_id` column, filtered listing, and tenant context threaded through
/// the scheduler to `/executions`) is part of the deferred runtime
/// tenant-threading work.
pub async fn list_cron(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let store = store(&state)?;
    let jobs = store.list_all().await.map_err(ApiError::Internal)?;
    Ok(Json(json!({ "cron_jobs": jobs })))
}

/// `DELETE /cron/:name` — remove a cron job.
pub async fn delete_cron(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let store = store(&state)?;
    store.delete(&name).await.map_err(ApiError::Internal)?;
    Ok(Json(json!({ "name": name, "deleted": true })))
}

/// The initiator recorded on runs the embedded cron scheduler starts.
///
/// A system principal, not a token: API token principals are `token:<id>`, so
/// no human token can ever equal it, and separation of duties lets any
/// reviewer approve a cron run while still having an initiator to check.
pub const CRON_PRINCIPAL: &str = "system:cron";

/// Starts cron runs inside the server through the same code as
/// `POST /executions`, with [`CRON_PRINCIPAL`] as initiator.
///
/// The embedded scheduler used to POST to the server's own `/executions`
/// with no token, which outside dev mode is a 401, so `JAMJET_EMBED_CRON`
/// never started a run on an authenticated server. Cron jobs are global in
/// this version, so runs start in the default tenant (see `list_cron`).
pub struct InProcessCronLauncher {
    state: AppState,
}

impl InProcessCronLauncher {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl jamjet_timers::CronLauncher for InProcessCronLauncher {
    async fn start(&self, job: &CronJob) -> Result<(), String> {
        crate::routes::start_run(
            &self.state,
            &TenantId::default(),
            job.workflow_id.clone(),
            Some(job.workflow_version.clone()),
            job.input.clone(),
            Some(CRON_PRINCIPAL.to_string()),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
    }
}
