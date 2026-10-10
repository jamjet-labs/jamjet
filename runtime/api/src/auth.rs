//! API token authentication and RBAC middleware (G2.1, G2.2).
//!
//! ## Authentication
//! Extracts `Authorization: Bearer <token>` and validates against `api_tokens`
//! table. On success, injects `ApiToken` into request extensions.
//!
//! ## RBAC — roles and permitted actions
//! | Role       | Write workflows | Read state | Admin (tokens) |
//! |------------|----------------|------------|----------------|
//! | operator   | ✓              | ✓          | ✓              |
//! | developer  | ✓              | ✓          | ✗              |
//! | reviewer   | approve only   | ✓          | ✗              |
//! | viewer     | ✗              | ✓          | ✗              |
//!
//! Routes are tagged with a minimum `RequiredRole`. The RBAC check happens
//! after auth validation using a second `from_fn` layer applied per-router.

use axum::{
    body::Body,
    extract::{Extension, MatchedPath, State},
    http::{Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use jamjet_state::{ApiToken, StateBackend, TenantId};
use serde_json::json;
use std::sync::Arc;

// ── Role model ────────────────────────────────────────────────────────────────

/// RBAC role (ordered by privilege: operator > developer > reviewer > viewer).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Viewer = 0,
    Reviewer = 1,
    Developer = 2,
    Operator = 3,
}

impl Role {
    pub fn parse_role(s: &str) -> Self {
        match s {
            "operator" => Self::Operator,
            "developer" => Self::Developer,
            "reviewer" => Self::Reviewer,
            _ => Self::Viewer,
        }
    }

    /// True if this role can write (mutate) workflow/execution state.
    pub fn can_write(&self) -> bool {
        matches!(self, Self::Operator | Self::Developer)
    }

    /// True if this role can decide a pending approval. Reviewing is the
    /// reviewer role's whole purpose, so it may approve or reject even though
    /// it may not otherwise write.
    pub fn can_approve(&self) -> bool {
        *self >= Self::Reviewer
    }

    /// True if this role can perform admin operations (token management).
    pub fn can_admin(&self) -> bool {
        matches!(self, Self::Operator)
    }
}

// ── Auth state ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AuthState {
    pub backend: Arc<dyn StateBackend>,
}

// ── Authentication middleware ─────────────────────────────────────────────────

/// Validates Bearer token and injects `ApiToken` into request extensions.
pub async fn require_auth(
    State(auth): State<AuthState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let token = match extract_bearer(req.headers()) {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "missing Authorization: Bearer <token>" })),
            )
                .into_response();
        }
    };

    match auth.backend.validate_token(&token).await {
        Ok(Some(info)) => {
            // Inject tenant context from the token.
            let tenant_id = TenantId::from(info.tenant_id.clone());
            req.extensions_mut().insert(tenant_id);
            req.extensions_mut().insert(info);
            next.run(req).await
        }
        Ok(None) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid or expired token" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("auth error: {e}") })),
        )
            .into_response(),
    }
}

// ── RBAC middleware ───────────────────────────────────────────────────────────

/// Route template of the approval decision endpoint, the one write a reviewer
/// may make. Must match the `.route(...)` string in `routes.rs`.
pub const APPROVE_ROUTE: &str = "/executions/:id/approve";

/// The audit/approval principal for an authenticated token: `token:<id>`.
/// The id, not the name: names are operator-chosen and need not be unique.
pub fn principal_of(token: &ApiToken) -> String {
    format!("token:{}", token.id)
}

/// RBAC middleware: enforces write-permission check for mutating HTTP methods
/// (POST, PUT, PATCH, DELETE). Read-only methods (GET, HEAD) are always allowed.
///
/// One exemption: `POST` on [`APPROVE_ROUTE`] needs only [`Role::can_approve`].
/// It is matched on the router's matched-path TEMPLATE, not on the raw URI, so
/// no other route (`/executions/:id/cancel`, `POST /executions`, ...) can reach
/// it by spelling. No matched path means no exemption.
///
/// Assumes `require_auth` ran first and `ApiToken` is in extensions.
pub async fn require_write_role(req: Request<Body>, next: Next) -> Response {
    let is_mutating = matches!(
        req.method(),
        &Method::POST | &Method::PUT | &Method::PATCH | &Method::DELETE
    );

    if is_mutating {
        let role = req
            .extensions()
            .get::<ApiToken>()
            .map(|t| Role::parse_role(&t.role))
            .unwrap_or(Role::Viewer);

        let is_approve = req.method() == Method::POST
            && req
                .extensions()
                .get::<MatchedPath>()
                .is_some_and(|p| p.as_str() == APPROVE_ROUTE);

        if is_approve {
            if !role.can_approve() {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "error": "insufficient role — reviewer, developer or operator required",
                        "required": "reviewer",
                    })),
                )
                    .into_response();
            }
        } else if !role.can_write() {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": "insufficient role — developer or operator required",
                    "required": "developer",
                })),
            )
                .into_response();
        }
    }

    next.run(req).await
}

/// RBAC middleware: requires `operator` role for admin endpoints.
pub async fn require_operator_role(
    Extension(token): Extension<ApiToken>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if !Role::parse_role(&token.role).can_admin() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "insufficient role — operator required",
                "required": "operator",
            })),
        )
            .into_response();
    }
    next.run(req).await
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    value.strip_prefix("Bearer ").map(str::to_string)
}
