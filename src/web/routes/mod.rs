//! Route table for the admin UI.
//!
//! Full pages live under their natural paths; htmx fragments are namespaced
//! under `/f/` so they can render fragment-shaped errors instead of swapping a
//! whole error page into a `<div>`.
//!
//! Every state-changing route is POST. That is not only REST tidiness: the CSRF
//! guard in [`super::same_origin_only`] exempts GET, and browsers prefetch
//! links, so a GET that deploys would be both prefetchable and forgeable.

pub mod browse;
pub mod dashboard;
pub mod projects;
pub mod runs;
pub mod settings;

use axum::{
    routing::{get, post},
    Router,
};
use maud::html;

use super::views;
use super::AppState;

/// Full HTML pages.
pub fn pages() -> Router<AppState> {
    Router::new()
        // `/healthz` deliberately lives in `server::webhook_router`, which is
        // merged in alongside this — so both ports answer the same probe.
        .route("/", get(dashboard::index))
        .route("/projects", get(projects::list).post(projects::create))
        .route("/projects/new", get(projects::new_form))
        .route("/projects/{id}", get(projects::detail).post(projects::update))
        .route("/projects/{id}/edit", get(projects::edit_form))
        .route(
            "/projects/{id}/delete",
            get(projects::delete_confirm).post(projects::delete),
        )
        .route("/projects/{id}/secret/rotate", post(projects::rotate_secret))
        .route("/projects/{id}/deploy", post(projects::deploy))
        .route("/projects/{id}/update-app", post(projects::update_app))
        .route("/runs", get(runs::index))
        .route("/runs/{run_id}", get(runs::detail))
        .route("/runs/{run_id}/raw", get(runs::raw))
        .route("/settings", get(settings::index).post(settings::save))
        .route(
            "/settings/cloudflare",
            get(settings::cloudflare_form).post(settings::cloudflare_save),
        )
        // POST, like every other state-changing route: the CSRF guard exempts
        // GET, and a prefetched link must never replace the binary.
        .route("/settings/check-update", post(settings::check_update))
        .route("/settings/update", post(settings::self_update))
}

/// htmx fragments: bare markup, no page chrome.
pub fn fragments() -> Router<AppState> {
    Router::new()
        .route("/f/projects", get(dashboard::projects_fragment))
        .route("/f/projects/{id}/secret", get(projects::reveal_secret))
        .route("/f/runs/{run_id}/log", get(runs::log_fragment))
        .route("/f/deploy-fields", get(projects::deploy_fields))
        .route("/f/slug", get(projects::slug_preview))
        .route("/f/path-check", get(browse::check))
        .route("/f/browse", get(browse::list))
        .route("/f/browse/select", get(browse::select))
}

/// HTML 404 for anything unrouted.
pub async fn not_found() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        axum::http::StatusCode::NOT_FOUND,
        views::page(
            "Not found",
            html! {
                section class="card" {
                    h1 { "Not found" }
                    p { "That page does not exist." }
                    p { a class="button" href="/" { "Back to dashboard" } }
                }
            },
        ),
    )
        .into_response()
}
