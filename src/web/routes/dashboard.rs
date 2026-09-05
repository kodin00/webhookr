//! Dashboard and health probe.

use maud::{html, Markup};
use std::collections::HashMap;

use crate::config;
use crate::state::{self, RunRecord};
use crate::web::views;
use crate::web::WebError;

/// Newest run per project, for the status badges.
fn latest_by_project() -> HashMap<String, RunRecord> {
    let mut latest: HashMap<String, RunRecord> = HashMap::new();
    // `load_runs` is newest-first, so the first entry seen per project wins.
    for run in state::load_runs() {
        latest.entry(run.project_id.clone()).or_insert(run);
    }
    latest
}

pub async fn index() -> Result<Markup, WebError> {
    let cfg = config::load_config()?;
    let body = html! {
        section class="page-head" {
            h1 { "Dashboard" }
            a class="button primary" href="/projects/new" { "Add project" }
        }

        @if cfg.projects.is_empty() {
            section class="card empty" {
                h2 { "No projects yet" }
                p { "Add a project to give it a webhook URL and a deploy command." }
                p { a class="button primary" href="/projects/new" { "Add your first project" } }
            }
        } @else {
            // Polls itself so in-flight deploys update without a manual refresh.
            div id="project-grid" hx-get="/f/projects" hx-trigger="every 5s" hx-swap="outerHTML" {
                (project_grid(&cfg))
            }
        }

        section class="card" {
            h2 { "Listener" }
            (views::code_field("Webhooks", &cfg.listen_addr))
            @if cfg.web.enabled {
                (views::code_field("Admin UI", &cfg.web.listen_addr))
            }
            @match &cfg.cloudflare {
                Some(tunnel) => {
                    (views::code_field("Public webhook host", &tunnel.hostname))
                    @match &tunnel.admin_hostname {
                        Some(host) => (views::code_field("Public admin host", host)),
                        None => p class="muted" {
                            "The admin UI is not routed through the tunnel yet. "
                            a href="/settings/cloudflare" { "Configure it" }
                            "."
                        }
                    }
                }
                None => p class="muted" {
                    "No Cloudflare Tunnel configured. "
                    a href="/settings/cloudflare" { "Set one up" }
                    " to get an HTTPS webhook URL."
                }
            }
        }
    };
    Ok(views::page("Dashboard", body))
}

/// The polled fragment. Returns the same element it replaces, so htmx keeps
/// polling without any client-side state.
pub async fn projects_fragment() -> Result<Markup, WebError> {
    let cfg = config::load_config()?;
    Ok(html! {
        div id="project-grid" hx-get="/f/projects" hx-trigger="every 5s" hx-swap="outerHTML" {
            (project_grid(&cfg))
        }
    })
}

fn project_grid(cfg: &config::AppConfig) -> Markup {
    let latest = latest_by_project();
    html! {
        div class="grid" {
            @for project in &cfg.projects {
                @let run = latest.get(&project.id);
                article class="card project-card" {
                    header class="project-card-head" {
                        h3 { a href={ "/projects/" (project.id) } { (project.name) } }
                        (views::status_badge(run))
                    }
                    p class="muted mono small" { (project.branch) " · " (project.preset_label()) }
                    @if let Some(run) = run {
                        p class="summary" { (run.message) }
                        p class="muted small" {
                            (views::jakarta_time(&run.started_at)) " · " (views::duration(run))
                            " · " a href={ "/runs/" (run.id) } { "log" }
                        }
                    } @else {
                        p class="muted small" { "Not deployed yet." }
                    }
                    footer class="actions" {
                        form method="post" action={ "/projects/" (project.id) "/update-app" } {
                            button type="submit" class="button primary" { "Pull & deploy" }
                        }
                        form method="post" action={ "/projects/" (project.id) "/deploy" } {
                            button type="submit" class="button" { "Redeploy" }
                        }
                    }
                }
            }
        }
    }
}

