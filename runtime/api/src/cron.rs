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
        // The caller's tenant: the job is listed, replaced and deleted only
        // there, and its runs start there.
        tenant_id: tenant_id.as_str().to_string(),
    };
    store.upsert(&job).await.map_err(ApiError::Internal)?;

    Ok((
        StatusCode::CREATED,
        Json(json!({ "name": job.name, "next_run_at": next_run_at.to_rfc3339() })),
    ))
}

/// `GET /cron` — the caller's tenant's cron jobs.
///
/// Jobs are keyed by `(tenant_id, name)`. They used to be global: any tenant
/// could list every job with its input and creator, and replace or delete one
/// by name.
pub async fn list_cron(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
) -> Result<Json<Value>, ApiError> {
    let store = store(&state)?;
    let jobs = store
        .list_for_tenant(tenant_id.as_str())
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "cron_jobs": jobs })))
}

/// `DELETE /cron/:name` — remove the caller's tenant's cron job of that name.
pub async fn delete_cron(
    State(state): State<AppState>,
    Extension(tenant_id): Extension<TenantId>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let store = store(&state)?;
    store
        .delete(tenant_id.as_str(), &name)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "name": name, "deleted": true })))
}

/// Starts cron runs inside the server through the same code as
/// `POST /executions`, with the job's creator as initiator.
///
/// The embedded scheduler used to POST to the server's own `/executions`
/// with no token, which outside dev mode is a 401, so `JAMJET_EMBED_CRON`
/// never started a run on an authenticated server. Each run starts in the
/// job's own tenant, under that tenant's policy.
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

/// The tenant a cron job's run starts in, under the single-tenant rule the API
/// applies to tokens (`auth::admit_token`): `default` jobs run in `default`;
/// any other tenant's job is refused, because the engine cannot keep that
/// tenant's run separate, unless the `JAMJET_TENANT_TOKENS_AS_DEFAULT` opt-out
/// is on, in which case it runs in `default`. A refused job stays scheduled
/// and is refused again at each due time; the scheduler logs it.
pub fn cron_run_tenant(job_tenant: &str, as_default: bool) -> Result<TenantId, String> {
    if job_tenant == jamjet_state::DEFAULT_TENANT || as_default {
        return Ok(TenantId::default());
    }
    Err(format!(
        "cron job in tenant {job_tenant} not started: this runtime is single-tenant and \
         runs every workflow in tenant default (set {}=1 to run it in default)",
        crate::auth::TENANT_TOKENS_AS_DEFAULT_ENV
    ))
}

#[async_trait::async_trait]
impl jamjet_timers::CronLauncher for InProcessCronLauncher {
    async fn start(&self, job: &CronJob) -> Result<(), String> {
        tracing::info!(
            cron_job = %job.name,
            created_by = job.created_by.as_deref().unwrap_or("<unknown>"),
            "starting cron run in-process"
        );
        let tenant = cron_run_tenant(&job.tenant_id, crate::auth::tenant_tokens_as_default())
            .inspect_err(
                |e| tracing::warn!(cron_job = %job.name, tenant = %job.tenant_id, "{e}"),
            )?;
        crate::routes::start_run(
            &self.state,
            &tenant,
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

#[cfg(test)]
mod tenant_tests {
    use super::*;

    #[test]
    fn a_default_job_runs_in_default() {
        assert_eq!(
            cron_run_tenant("default", false).unwrap(),
            TenantId::default()
        );
    }

    #[test]
    fn another_tenants_job_is_refused() {
        let err = cron_run_tenant("acme", false).unwrap_err();
        assert!(err.contains("single-tenant"), "{err}");
    }

    /// With the opt-out, such a job runs in `default`, never in its own tenant.
    #[test]
    fn the_opt_out_runs_it_in_default() {
        assert_eq!(cron_run_tenant("acme", true).unwrap(), TenantId::default());
    }
}
