//! Run history and the live log view.

use axum::{
    extract::{Path, Query},
    response::{IntoResponse, Response},
};
use maud::{html, Markup};
use serde::Deserialize;

use crate::state::{self, RunRecord};
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
                            th { "Status" } th { "Project" } th { "Started" }
                            th { "Duration" } th { "Summary" }
                        } }
                        tbody {
                            @for run in &runs {
                                tr {
                                    td { (views::status_badge(Some(run))) }
                                    td { a href={ "/projects/" (run.project_id) } { (run.project_id) } }
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
            (views::code_field("Started", &run.started_at))
            @if let Some(finished) = &run.finished_at {
                (views::code_field("Finished", finished))
            }
            (views::field("Duration", &views::duration(&run)))
            @if !run.message.is_empty() { (views::field("Summary", &run.message)) }
        }
        section class="card" {
            h2 { "Output" }
            (log_block(&run))
        }
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
