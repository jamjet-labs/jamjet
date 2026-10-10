use crate::auth::{principal_of, DevMode};
use crate::error::ApiError;
use crate::state::AppState;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use jamjet_state::{ApiToken, TenantId};
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
///
/// The caller's token principal is recorded as the job's `created_by` and
/// becomes the initiator of every run the job starts: choosing the workflow,
/// the input and the schedule is starting the run, so separation of duties
/// must refuse the creator's approval. Replacing a job by name replaces its
/// creator too. Outside dev mode a request with no token is refused rather
/// than stored as a job nobody created.
pub async fn create_cron(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    caller: Option<Extension<ApiToken>>,
    dev: Option<Extension<DevMode>>,
    Json(body): Json<CreateCronRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let store = store(&state)?;
    let created_by = match (caller, dev) {
        (Some(Extension(token)), _) => Some(principal_of(&token)),
        (None, Some(_)) => None,
        (None, None) => return Err(ApiError::Unauthorized),
    };
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
        created_by,
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

/// Starts cron runs inside the server through the same code as
/// `POST /executions`, with the job's creator as initiator.
///
/// The embedded scheduler used to POST to the server's own `/executions`
/// with no token, which outside dev mode is a 401, so `JAMJET_EMBED_CRON`
/// never started a run on an authenticated server. Cron jobs are global in
/// this version, so runs start in the default tenant (see `list_cron`).
///
/// The initiator is [`CronJob::created_by`], never a system principal: a
/// system initiator would let the person who scheduled the run approve it.
/// A job with no recorded creator starts a run with no initiator, and
/// separation of duties refuses every approval on such a run outside dev
/// mode.
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
        tracing::info!(
            cron_job = %job.name,
            created_by = job.created_by.as_deref().unwrap_or("<unknown>"),
            "starting cron run in-process"
        );
        crate::routes::start_run(
            &self.state,
            &TenantId::default(),
            job.workflow_id.clone(),
            Some(job.workflow_version.clone()),
            job.input.clone(),
            job.created_by.clone(),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
    }
}
