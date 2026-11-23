use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("forbidden: {0}")]
    Forbidden(String),

    /// 429, with `Retry-After`: the caller should try again shortly.
    #[error("too many requests: {0}")]
    TooManyRequests(String),

    /// 503, with `Retry-After`: a transient failure, worth a retry.
    #[error("unavailable: {0}")]
    Unavailable(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            Self::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "Unauthorized".into()),
            Self::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.clone()),
            Self::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
            Self::TooManyRequests(msg) => (StatusCode::TOO_MANY_REQUESTS, msg.clone()),
            Self::Unavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg.clone()),
        };
        let retry = matches!(self, Self::TooManyRequests(_) | Self::Unavailable(_));
        let mut resp = (status, Json(json!({ "error": message }))).into_response();
        if retry {
            resp.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("1"),
            );
        }
        resp
    }
}

impl From<jamjet_state::backend::StateBackendError> for ApiError {
    fn from(e: jamjet_state::backend::StateBackendError) -> Self {
        match e {
            jamjet_state::backend::StateBackendError::NotFound(msg) => Self::NotFound(msg),
            e @ jamjet_state::backend::StateBackendError::VersionConflict(_) => {
                Self::Conflict(e.to_string())
            }
            jamjet_state::backend::StateBackendError::Unavailable(msg) => Self::Unavailable(msg),
            other => Self::Internal(other.to_string()),
        }
    }
}

/// An audit log that could not be reached answers 503 with `Retry-After`,
/// as the state store does; any other audit error is a 500.
impl From<jamjet_audit::AuditError> for ApiError {
    fn from(e: jamjet_audit::AuditError) -> Self {
        match e {
            jamjet_audit::AuditError::Unavailable(msg) => Self::Unavailable(msg),
            other => Self::Internal(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jamjet_state::backend::StateBackendError;

    #[test]
    fn an_unavailable_store_answers_503_with_retry_after() {
        let resp = ApiError::from(StateBackendError::Unavailable(
            "commit outcome unknown".into(),
        ))
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::RETRY_AFTER)
                .map(|v| v.to_str().unwrap()),
            Some("1")
        );
    }

    #[test]
    fn a_database_error_still_answers_500() {
        let resp = ApiError::from(StateBackendError::Database("boom".into())).into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(resp
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .is_none());
    }
}
