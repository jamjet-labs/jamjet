//! Serves the embedded Web Companion SPA.
//!
//! In release builds, `web/dist/` is compiled into the binary via `rust-embed`.
//! All non-API paths fall back to `index.html` for client-side routing.

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../../web/dist"]
struct Assets;

/// Fallback handler: serves static assets or `index.html` for SPA routing.
pub async fn serve_spa(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    // Try exact file match (JS, CSS, images, etc.)
    if !path.is_empty() {
        if let Some(asset) = Assets::get(path) {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            return (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (header::CACHE_CONTROL, "public, max-age=31536000, immutable".to_string()),
                ],
                asset.data.to_vec(),
            )
                .into_response();
        }
    }

    // Fallback to index.html for SPA client-side routing
    match Assets::get("index.html") {
        Some(asset) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/html".to_string()),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
            asset.data.to_vec(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "Web companion not built. Run: cd web && npm run build")
            .into_response(),
    }
}
