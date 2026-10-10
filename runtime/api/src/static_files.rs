//! Serves the embedded Web Companion SPA.
//!
//! In release builds, `web/dist/` is compiled into the binary via `rust-embed`.
//! A browser page load (GET or HEAD) on an unmatched path falls back to
//! `index.html` for client-side routing. Anything API-shaped that matched no
//! route gets a JSON 404 instead: see [`is_page_load`].

use axum::http::{header, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rust_embed::Embed;
use serde_json::json;

#[derive(Embed)]
#[folder = "../../web/dist"]
struct Assets;

/// Shown to any client whose request matched no route. Clients that hit this
/// are almost always on a wrong path, most often an `/api/v1` prefix that the
/// runtime has never served.
const NOT_FOUND_HINT: &str = "no route matches this request. The JamJet runtime serves its \
API at bare paths such as /workflows, /executions and /agents, with no /api or /api/v1 \
prefix. Two JamJet Java clients sent /api/v1: jamjet-sdk before 0.5.1 (upgrade it), and the \
JamjetRuntimeClient in jamjet-spring-boot-starter 0.2.0 and earlier (no release of the starter \
uses the bare paths yet; call them directly or through jamjet-sdk 0.5.1).";

/// Whether an unmatched request may be answered with the SPA.
///
/// Only a browser page load qualifies: a GET or HEAD whose path is not under
/// the `/api` segment. Every other method is an API call by construction (a
/// browser navigation never POSTs to a client-side route), and `/api/...` is
/// where wrong-prefix clients land. Answering those with `200 text/html` made
/// a wrong path look like a success that failed to parse.
fn is_page_load(method: &Method, path: &str) -> bool {
    let reserved_api = path == "/api" || path.starts_with("/api/");
    (method == Method::GET || method == Method::HEAD) && !reserved_api
}

fn json_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not found", "hint": NOT_FOUND_HINT })),
    )
        .into_response()
}

/// Fallback handler: serves static assets or `index.html` for SPA routing, and
/// a JSON 404 for anything that is not a page load.
pub async fn serve_spa(method: Method, uri: Uri) -> Response {
    if !is_page_load(&method, uri.path()) {
        return json_not_found();
    }

    let path = uri.path().trim_start_matches('/');

    // Try exact file match (JS, CSS, images, etc.)
    if !path.is_empty() {
        if let Some(asset) = Assets::get(path) {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            return (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (
                        header::CACHE_CONTROL,
                        "public, max-age=31536000, immutable".to_string(),
                    ),
                ],
                asset.data.to_vec(),
            )
                .into_response();
        }
    }

    // If the path looks like a file request (has an extension), return 404
    // instead of falling back to index.html (which would cause confusing 200 OK).
    if !path.is_empty() && path.contains('.') {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
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
        None => (
            StatusCode::NOT_FOUND,
            "Web companion not built. Run: cd web && npm run build",
        )
            .into_response(),
    }
}
