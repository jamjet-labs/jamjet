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
            let info = match admit_token(info, tenant_tokens_as_default()) {
                Ok(info) => info,
                Err(refusal) => {
                    return (StatusCode::FORBIDDEN, Json(json!({ "error": refusal })))
                        .into_response();
                }
            };
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

// ── Single-tenant engine ─────────────────────────────────────────────────────

/// Why a token of any tenant but `default` is refused.
pub const SINGLE_TENANT_REFUSAL: &str = "multi-tenant tokens are not supported by this runtime \
     yet; the engine runs every workflow in tenant default";

/// The opt-out: `JAMJET_TENANT_TOKENS_AS_DEFAULT=1` makes such a token act in
/// `default`, as every token did before tokens carried their tenant. It gives
/// NO isolation: the token sees and changes the default tenant's data.
pub const TENANT_TOKENS_AS_DEFAULT_ENV: &str = "JAMJET_TENANT_TOKENS_AS_DEFAULT";

static TENANT_TOKENS_AS_DEFAULT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Whether the opt-out is on. Read from the environment once, at the first
/// call; `main` makes that call at startup and logs a warning when it is on.
pub fn tenant_tokens_as_default() -> bool {
    *TENANT_TOKENS_AS_DEFAULT.get_or_init(|| {
        std::env::var(TENANT_TOKENS_AS_DEFAULT_ENV)
            .map(|v| matches!(v.as_str(), "1" | "true"))
            .unwrap_or(false)
    })
}

/// Admit a validated token, or refuse it with [`SINGLE_TENANT_REFUSAL`].
///
/// The API layer is tenant-scoped, but the engine under it is not: the
/// scheduler, the in-process workers, workflow lookup and its cache, work-item
/// creation, segment rollover and the agent registry all run in `default`. A
/// token in another tenant could register a workflow version that default's
/// runs were then scheduled from, and its own runs lost their tenant (and its
/// tenant policy) after the start node. Until the engine carries the tenant,
/// only `default` tokens are admitted; `as_default` admits the others as
/// `default` instead.
pub fn admit_token(mut token: ApiToken, as_default: bool) -> Result<ApiToken, &'static str> {
    if token.tenant_id == jamjet_state::DEFAULT_TENANT {
        return Ok(token);
    }
    if as_default {
        token.tenant_id = jamjet_state::DEFAULT_TENANT.to_string();
        return Ok(token);
    }
    Err(SINGLE_TENANT_REFUSAL)
}

/// Request extension present only on the dev-mode router (no auth layer).
///
/// Handlers that relax a rule in dev mode check for THIS marker, never for
/// "no token in the extensions": a missing token outside dev mode is a
/// misconfiguration to refuse, not permission to relax.
#[derive(Debug, Clone, Copy)]
pub struct DevMode;

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

#[cfg(test)]
mod admit_tests {
    use super::*;

    fn token(tenant: &str) -> ApiToken {
        ApiToken {
            id: "t1".into(),
            name: "n".into(),
            role: "developer".into(),
            created_at: chrono::Utc::now(),
            expires_at: None,
            tenant_id: tenant.into(),
        }
    }

    #[test]
    fn a_default_tenant_token_is_admitted_unchanged() {
        assert_eq!(
            admit_token(token("default"), false).unwrap().tenant_id,
            "default"
        );
    }

    #[test]
    fn another_tenants_token_is_refused() {
        assert_eq!(
            admit_token(token("acme"), false).unwrap_err(),
            SINGLE_TENANT_REFUSAL
        );
    }

    /// The opt-out admits it, but in `default`: never in its own tenant,
    /// which the engine cannot keep separate.
    #[test]
    fn the_opt_out_admits_it_as_default() {
        assert_eq!(
            admit_token(token("acme"), true).unwrap().tenant_id,
            "default"
        );
    }
}
