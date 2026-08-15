//! Error type for admin handlers.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use maud::html;

/// Wraps [`anyhow::Error`] so handlers can use `?` and still render HTML.
pub struct WebError {
    status: StatusCode,
    error: anyhow::Error,
}

impl WebError {
    pub fn new(status: StatusCode, error: anyhow::Error) -> Self {
        Self { status, error }
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, anyhow::anyhow!("{what} not found"))
    }
}

impl<E> From<E> for WebError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.into())
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        // `{:#}` includes the anyhow context chain, which is where the useful
        // detail lives ("failed to write config at ...: permission denied").
        let detail = format!("{:#}", self.error);
        let body = super::views::page(
            "Error",
            html! {
                section class="card error-page" {
                    h1 { "Something went wrong" }
                    p class="error" { (detail) }
                    p { a class="button" href="/" { "Back to dashboard" } }
                }
            },
        );
        (self.status, body).into_response()
    }
}
