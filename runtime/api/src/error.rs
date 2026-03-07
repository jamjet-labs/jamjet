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

    #[error("internal error: {0}")]
    Internal(String),

    #[error("unauthorized")]
    Unauthorized,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "Unauthorized".into()),
            Self::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<jamjet_state::backend::StateBackendError> for ApiError {
    fn from(e: jamjet_state::backend::StateBackendError) -> Self {
        match e {
            jamjet_state::backend::StateBackendError::NotFound(msg) => Self::NotFound(msg),
            other => Self::Internal(other.to_string()),
        }
    }
}
