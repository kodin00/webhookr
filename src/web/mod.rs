//! Browser admin UI.
//!
//! Server-rendered HTML with [htmx](https://htmx.org) for the interactive bits,
//! served on its own port from the same process as the webhook listener. There
//! is no login: the UI is expected to sit behind Cloudflare Access, and it is
//! disabled unless explicitly switched on.
//!
//! Handlers deliberately read and write `config.json` per request (via
//! [`crate::config::update_config`]) rather than holding an in-memory copy, so
//! the CLI, the TUI and this UI never disagree about the current state.
//!
//! Page markup lives beside its handler in `routes::*`; [`views`] holds only
//! the shared layout and the components used by more than one page.

mod assets;
mod error;
mod forms;
mod routes;
mod views;

use anyhow::{Context, Result};
use axum::{
    extract::Request,
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::Response,
    Router,
};

pub use error::WebError;

/// Content-Security-Policy for every HTML response.
///
/// `script-src 'self'` forbids inline script, which is why nothing in this UI
/// uses htmx's `hx-on:*` attributes — they evaluate inline JavaScript. Every
/// interaction here is expressible with `hx-get`/`hx-post`/`hx-target`/
/// `hx-swap`/`hx-confirm`/`hx-trigger` plus out-of-band swaps.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
                   img-src 'self' data:; form-action 'self'; base-uri 'none'; \
                   frame-ancestors 'none'";

/// Whether an unauthenticated Access header check is required, captured once at
/// startup so handlers don't re-read config just to answer it.
#[derive(Clone)]
pub struct AppState {
    pub require_access_header: bool,
}

/// Build the admin router.
pub fn router(state: AppState) -> Router {
    let require_access = state.require_access_header;

    Router::new()
        .merge(routes::pages())
        .merge(routes::fragments())
        .route("/static/{file}", axum::routing::get(assets::asset))
        .fallback(routes::not_found)
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn(same_origin_only))
        .layer(middleware::from_fn(move |req, next| {
            access_header_gate(require_access, req, next)
        }))
        .with_state(state)
}

/// Bind and serve the admin UI. Returns only on error or shutdown.
pub async fn serve(addr: &str, state: AppState) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind admin UI on {addr}"))?;
    axum::serve(listener, router(state))
        .await
        .context("admin UI listener stopped")
}

/// Reject state-changing requests that did not originate from this UI.
///
/// Cloudflare Access does not prevent CSRF: its cookie rides along on
/// cross-site form POSTs, so without this a page the operator visits elsewhere
/// could create a project whose command is arbitrary shell and then trigger it.
/// `Sec-Fetch-Site` is set by the browser and cannot be forged from script.
///
/// A missing header is rejected rather than allowed — every browser that can
/// run this UI sends it, so absence means a non-browser client.
async fn same_origin_only(req: Request, next: Next) -> Result<Response, StatusCode> {
    if matches!(*req.method(), Method::GET | Method::HEAD) {
        return Ok(next.run(req).await);
    }
    match req
        .headers()
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
    {
        // "none" covers a form submitted from a page the user typed directly.
        Some("same-origin") | Some("none") => Ok(next.run(req).await),
        _ => Err(StatusCode::FORBIDDEN),
    }
}

/// Optional presence check for Cloudflare Access' JWT header.
///
/// Deliberately *not* a JWT validation: it only catches the case where the
/// admin port is reached without passing through Access at all.
async fn access_header_gate(
    required: bool,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if required && !req.headers().contains_key("cf-access-jwt-assertion") {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(req).await)
}

/// Apply hardening headers to every response that isn't a cacheable asset.
async fn security_headers(req: Request, next: Next) -> Response {
    let is_asset = req.uri().path().starts_with("/static/");
    let mut response = next.run(req).await;
    let headers = response.headers_mut();

    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if !is_asset {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP),
        );
        // Pages can contain webhook secrets; never let them sit in a cache.
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        );
    }
    response
}
