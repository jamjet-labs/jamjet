//! Axum REST API for Engram memory operations.

use crate::config::LlmBackend;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use engram::context::{ContextConfig, OutputFormat};
use engram::extract::{ExtractionConfig, Message};
use engram::memory::{Memory, RecallQuery};
use engram::scope::Scope;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AppState {
    pub memory: Arc<Memory>,
    pub llm_backend: LlmBackend,
}

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AddRequest {
    pub messages: Vec<MessagePayload>,
    pub user_id: Option<String>,
    pub org_id: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Deserialize)]
pub struct MessagePayload {
    pub role: String,
    pub content: String,
}

#[derive(Deserialize)]
pub struct RecallParams {
    pub q: String,
    pub user_id: Option<String>,
    pub org_id: Option<String>,
    pub max_results: Option<usize>,
}

#[derive(Deserialize)]
pub struct ContextRequest {
    pub query: String,
    pub user_id: Option<String>,
    pub org_id: Option<String>,
    pub token_budget: Option<usize>,
    pub format: Option<String>,
}

#[derive(Deserialize)]
pub struct SearchParams {
    pub q: String,
    pub user_id: Option<String>,
    pub org_id: Option<String>,
    pub top_k: Option<usize>,
}

#[derive(Deserialize)]
pub struct ForgetRequest {
    pub reason: Option<String>,
}

#[derive(Deserialize)]
pub struct ConsolidateRequest {
    pub user_id: Option<String>,
    pub org_id: Option<String>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_scope(org_id: Option<&str>, user_id: Option<&str>, session_id: Option<&str>) -> Scope {
    let org = org_id.unwrap_or("default");
    match user_id {
        Some(uid) => match session_id {
            Some(sid) => Scope::session(org, uid, sid),
            None => Scope::user(org, uid),
        },
        None => Scope::org(org),
    }
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: msg.into() }))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /v1/memory
async fn add_handler(
    State(state): State<AppState>,
    Json(body): Json<AddRequest>,
) -> impl IntoResponse {
    let messages: Vec<Message> = body
        .messages
        .iter()
        .map(|m| Message {
            role: m.role.clone(),
            content: m.content.clone(),
        })
        .collect();

    if messages.is_empty() {
        return err(StatusCode::BAD_REQUEST, "messages must not be empty").into_response();
    }

    let scope = parse_scope(
        body.org_id.as_deref(),
        body.user_id.as_deref(),
        body.session_id.as_deref(),
    );

    match state
        .memory
        .add_messages(
            &messages,
            scope,
            state.llm_backend.build(),
            ExtractionConfig::default(),
        )
        .await
    {
        Ok(ids) => {
            let fact_ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "success": true,
                    "fact_count": ids.len(),
                    "fact_ids": fact_ids,
                })),
            )
                .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// GET /v1/memory/recall?q=...
async fn recall_handler(
    State(state): State<AppState>,
    Query(params): Query<RecallParams>,
) -> impl IntoResponse {
    let scope = parse_scope(params.org_id.as_deref(), params.user_id.as_deref(), None);

    let query = RecallQuery {
        query: params.q,
        scope: Some(scope),
        max_results: params.max_results.unwrap_or(10),
        as_of: None,
        min_score: None,
    };

    match state.memory.recall(&query).await {
        Ok(facts) => {
            let results: Vec<serde_json::Value> = facts
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "fact_id": f.id.to_string(),
                        "text": f.text,
                        "tier": f.tier,
                        "category": f.category,
                        "confidence": f.confidence,
                    })
                })
                .collect();
            Json(serde_json::json!({ "results": results, "total": results.len() })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// POST /v1/memory/context
async fn context_handler(
    State(state): State<AppState>,
    Json(body): Json<ContextRequest>,
) -> impl IntoResponse {
    let scope = parse_scope(body.org_id.as_deref(), body.user_id.as_deref(), None);

    let format = match body.format.as_deref() {
        Some("markdown") => OutputFormat::Markdown,
        Some("raw") => OutputFormat::Raw,
        _ => OutputFormat::SystemPrompt,
    };

    let config = ContextConfig {
        token_budget: body.token_budget.unwrap_or(2000),
        format,
        ..Default::default()
    };

    match state.memory.context(&body.query, &scope, config).await {
        Ok(block) => Json(serde_json::json!({
            "text": block.text,
            "token_count": block.token_count,
            "facts_included": block.facts_included,
            "facts_omitted": block.facts_omitted,
            "tier_breakdown": block.tier_breakdown,
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// DELETE /v1/memory/facts/:id
async fn forget_handler(
    State(state): State<AppState>,
    Path(fact_id): Path<String>,
    body: Option<Json<ForgetRequest>>,
) -> impl IntoResponse {
    let id = match uuid::Uuid::parse_str(&fact_id) {
        Ok(id) => id,
        Err(e) => {
            return err(StatusCode::BAD_REQUEST, format!("invalid fact_id: {e}")).into_response()
        }
    };

    let reason = body.and_then(|b| b.reason.clone());

    match state.memory.forget(id, reason.as_deref()).await {
        Ok(()) => Json(serde_json::json!({
            "success": true,
            "deleted_fact_id": fact_id,
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// GET /v1/memory/search?q=...
async fn search_handler(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let scope = parse_scope(params.org_id.as_deref(), params.user_id.as_deref(), None);
    let top_k = params.top_k.unwrap_or(10);

    match state
        .memory
        .fact_store()
        .keyword_search(&params.q, &scope, top_k)
        .await
    {
        Ok(facts) => {
            let results: Vec<serde_json::Value> = facts
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "fact_id": f.id.to_string(),
                        "text": f.text,
                        "tier": f.tier,
                        "category": f.category,
                    })
                })
                .collect();
            Json(serde_json::json!({ "results": results, "total": results.len() })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// GET /v1/memory/stats
async fn stats_handler(State(state): State<AppState>) -> impl IntoResponse {
    match state.memory.stats(None).await {
        Ok(stats) => Json(serde_json::json!({
            "total_facts": stats.total_facts,
            "valid_facts": stats.valid_facts,
            "invalidated_facts": stats.invalidated_facts,
            "total_entities": stats.total_entities,
            "total_relationships": stats.total_relationships,
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// POST /v1/memory/consolidate
async fn consolidate_handler(
    State(state): State<AppState>,
    Json(body): Json<ConsolidateRequest>,
) -> impl IntoResponse {
    let scope = parse_scope(body.org_id.as_deref(), body.user_id.as_deref(), None);
    let config = engram::consolidation::ConsolidationConfig::default();

    match state.memory.consolidate(&scope, None, config).await {
        Ok(result) => Json(serde_json::json!(result)).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// DELETE /v1/memory/users/:id
async fn delete_user_handler(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> impl IntoResponse {
    let scope = Scope::user("default", &user_id);

    match state.memory.delete_user_data(scope).await {
        Ok(count) => Json(serde_json::json!({
            "success": true,
            "deleted_facts": count,
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// GET /health
async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "engram" }))
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the Axum router with all REST endpoints.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/v1/memory", post(add_handler))
        .route("/v1/memory/recall", get(recall_handler))
        .route("/v1/memory/context", post(context_handler))
        .route("/v1/memory/facts/:id", delete(forget_handler))
        .route("/v1/memory/search", get(search_handler))
        .route("/v1/memory/stats", get(stats_handler))
        .route("/v1/memory/consolidate", post(consolidate_handler))
        .route("/v1/memory/users/:id", delete(delete_user_handler))
        .with_state(state)
}
