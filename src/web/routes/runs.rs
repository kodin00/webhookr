//! Run history and the live log view.

use axum::{
    extract::{Path, Query},
    response::{IntoResponse, Response},
};
use maud::{html, Markup};
use serde::Deserialize;

use crate::state::{self, RunRecord, TelegramDelivery};
use crate::util;
use crate::web::views;
use crate::web::WebError;

/// How much of a log to show while tailing. Build logs can reach megabytes and
/// the tail is the interesting part.
const TAIL_BYTES: u64 = 64 * 1024;

#[derive(Deserialize)]
pub struct RunFilter {
    pub project: Option<String>,
    pub status: Option<String>,
}

pub async fn index(Query(filter): Query<RunFilter>) -> Result<Markup, WebError> {
    let runs: Vec<RunRecord> = state::load_runs()
        .into_iter()
        .filter(|run| {
            filter
                .project
                .as_ref()
                .is_none_or(|want| &run.project_id == want)
                && filter
                    .status
                    .as_ref()
                    .is_none_or(|want| &run.status == want)
        })
        .collect();

    let body = html! {
        section class="page-head" { h1 { "Runs" } }
        section class="card" {
            @if runs.is_empty() {
                p class="muted" { "No runs recorded yet." }
            } @else {
                div class="table-scroll" {
                    table {
                        thead { tr {
                            th { "Status" } th { "Project" } th { "Commit" }
                            th { "Started" } th { "Duration" } th { "Summary" }
                        } }
                        tbody {
                            @for run in &runs {
                                tr {
                                    td { (views::status_badge(Some(run))) }
                                    td { a href={ "/projects/" (run.project_id) } { (run.project_id) } }
                                    td class="mono small" title=[run.commit.as_deref()] {
                                        (short_sha(run))
                                    }
                                    td class="mono small" {
                                        a href={ "/runs/" (run.id) } { (run.started_at) }
                                    }
                                    td class="mono small" { (views::duration(run)) }
                                    td class="summary" { (run.message) }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    Ok(views::page("Runs", body))
}

fn find_run(run_id: &str) -> Result<RunRecord, WebError> {
    state::load_runs()
        .into_iter()
        .find(|r| r.id == run_id)
        .ok_or_else(|| WebError::not_found("run"))
}

/// A sha clipped to the seven characters GitHub itself prints, so the column
/// stays narrow; the full value rides along in the cell's tooltip.
fn short_sha(run: &RunRecord) -> String {
    match run.commit.as_deref() {
        // Char-boundary-safe against a hand-edited history file.
        Some(sha) => sha.chars().take(7).collect(),
        None => "—".to_string(),
    }
}

/// Whether the chat was actually notified about this run, and if not, why.
///
/// Rendered from the record rather than the log because the page stops
/// polling the log before the `# telegram:` note is written.
fn telegram_field(delivery: &TelegramDelivery) -> Markup {
    html! {
        div class="field" {
            span class="field-label" { "Telegram" }
            span class="field-value" {
                @if delivery.sent {
                    span class="badge badge-ok" { "sent" }
                } @else {
                    span class="badge badge-fail" { "not sent" }
                    @if !delivery.detail.is_empty() {
                        " "
                        span class="warn-inline" { (delivery.detail) }
                    }
                }
            }
        }
    }
}

pub async fn detail(Path(run_id): Path<String>) -> Result<Markup, WebError> {
    let run = find_run(&run_id)?;
    let body = html! {
        section class="page-head" {
            h1 { "Run " span class="mono small" { (run.id) } }
            a class="button" href={ "/runs/" (run.id) "/raw" } { "Raw log" }
        }
        section class="card" {
            (views::field("Status", &run.status))
            (views::field("Project", &run.project_id))
            @if let Some(sha) = &run.commit {
                (views::code_field("Commit", sha))
            }
            (views::code_field("Started", &run.started_at))
            @if let Some(finished) = &run.finished_at {
                (views::code_field("Finished", finished))
            }
            (views::field("Duration", &views::duration(&run)))
            @if !run.message.is_empty() { (views::field("Summary", &run.message)) }
            @if let Some(delivery) = &run.telegram {
                (telegram_field(delivery))
            }
        }
        section class="card" {
            div class="card-head" {
                h2 { "Output" }
                // Autoscroll only means something while output is still
                // arriving, so finished runs get no toggle. It sits outside
                // #log-pane on purpose: the pane replaces itself every poll
                // and would take a focused or mid-click checkbox with it.
                @if run.status == "running" {
                    label class="log-toggle" for="autoscroll" {
                        input id="autoscroll" type="checkbox" checked {}
                        "Auto-scroll"
                    }
                }
            }
            (log_block(&run))
        }
        // The only page with bespoke behaviour; log.js keeps the output pane
        // pinned to the newest line. Loaded here rather than in every page
        // head, and versioned like the other assets so upgrades bust caches.
        script src={ "/static/log.js?v=" (env!("CARGO_PKG_VERSION")) } defer {}
    };
    Ok(views::page("Run", body))
}

/// The self-terminating log tail.
///
/// The element replaces itself on every poll. Once the run is no longer
/// `running` the returned markup simply omits `hx-get`/`hx-trigger`, so polling
/// stops on its own — no timer to cancel and no JavaScript of our own.
fn log_block(run: &RunRecord) -> Markup {
    let finished = run.status != "running";
    let text = util::strip_ansi(&state::read_run_log_tail(&run.id, TAIL_BYTES));
    html! {
        div id="log-pane"
            hx-get=[(!finished).then(|| format!("/f/runs/{}/log", run.id))]
            hx-trigger=[(!finished).then_some("load delay:2s")]
            hx-swap="outerHTML" {
            @if text.trim().is_empty() {
                p class="muted" { "No output yet." }
            } @else {
                pre class="log" { (text) }
            }
            @if !finished {
                p class="muted small" { "Streaming… this refreshes every 2 seconds." }
            }
        }
    }
}

pub async fn log_fragment(Path(run_id): Path<String>) -> Result<Markup, WebError> {
    let run = find_run(&run_id)?;
    Ok(log_block(&run))
}

pub async fn raw(Path(run_id): Path<String>) -> Result<Response, WebError> {
    let run = find_run(&run_id)?;
    let text = state::read_run_log(&run.id);
    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text,
    )
        .into_response())
}
