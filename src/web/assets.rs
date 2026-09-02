//! Static assets, compiled into the binary.
//!
//! `include_str!` rather than `rust-embed` or `tower-http`'s `ServeDir`: there
//! are two files, and both alternatives would either add a proc macro or
//! require the assets to exist on disk at runtime, which would defeat the
//! single-binary install.

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};

/// htmx 2.0.10, vendored from https://unpkg.com/htmx.org@2.0.10/dist/htmx.min.js
/// sha256 71ea67185bfa8c98c39d31717c6fce5d852370fcdfd129db4543774d3145c0de
const HTMX: &str = include_str!("assets/htmx.min.js");
const CSS: &str = include_str!("assets/app.css");
const LOG_JS: &str = include_str!("assets/log.js");

/// Serve one of the embedded assets.
///
/// An explicit match rather than a path join, so there is no traversal surface.
pub async fn asset(Path(file): Path<String>) -> Response {
    let (body, content_type) = match file.as_str() {
        "htmx.min.js" => (HTMX, "application/javascript; charset=utf-8"),
        "app.css" => (CSS, "text/css; charset=utf-8"),
        "log.js" => (LOG_JS, "application/javascript; charset=utf-8"),
        _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };

    (
        [
            (header::CONTENT_TYPE, content_type),
            // Safe to cache forever: the layout appends ?v=<crate version>, so
            // an upgrade requests a different URL.
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
    )
        .into_response()
}
