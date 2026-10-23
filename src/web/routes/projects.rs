//! Project CRUD, secrets, and deploy triggers.

use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Form,
};
use maud::{html, Markup};
use serde::Deserialize;

use crate::backup;
use crate::config::{self, ProjectConfig, PRESETS};
use crate::executor;
use crate::state;
use crate::util;
use crate::web::forms::{self, ProjectForm};
use crate::web::views;
use crate::web::WebError;

fn load_project(id: &str) -> Result<(config::AppConfig, ProjectConfig), WebError> {
    let cfg = config::load_config()?;
    let project = cfg
        .get(id)
        .cloned()
        .ok_or_else(|| WebError::not_found("project"))?;
    Ok((cfg, project))
}

// ----- list & detail -----------------------------------------------------

pub async fn list() -> Result<Markup, WebError> {
    let cfg = config::load_config()?;
    let body = html! {
        section class="page-head" {
            h1 { "Projects" }
            a class="button primary" href="/projects/new" { "Add project" }
        }
        section class="card" {
            @if cfg.projects.is_empty() {
                p class="muted" { "No projects configured yet." }
            } @else {
                div class="table-scroll" {
                    table {
                        thead { tr {
                            th { "Name" } th { "Branch" } th { "Deployment" }
                            th { "Verify" } th { "Webhook URL" }
                        } }
                        tbody {
                            @for project in &cfg.projects {
                                tr {
                                    td { a href={ "/projects/" (project.id) } { (project.name) } }
                                    td class="mono" { (project.branch) }
                                    td { (project.preset_label()) }
                                    td class="mono" { (project.verify_mode) }
                                    td class="mono small" {
                                        (config::webhook_url(&cfg, &project.id))
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    Ok(views::page("Projects", body))
}

pub async fn detail(
    Path(id): Path<String>,
    Query(started): Query<DetailQuery>,
) -> Result<Markup, WebError> {
    // Present only right after the "Back up now" button redirects back, so
    // the operator knows why the list below has not changed yet.
    let backup_started = started.backed_up.is_some();
    let (cfg, project) = load_project(&id)?;
    let runs: Vec<_> = state::load_runs()
        .into_iter()
        .filter(|r| r.project_id == id)
        .take(10)
        .collect();
    // The in-flight run, if any — so the Deploy card can offer to stop it
    // without a separate lookup, and the lock state on the page is the truth.
    let running = runs
        .iter()
        .find(|r| r.status == "running")
        .map(|r| r.id.clone());
    let hook_url = config::webhook_url(&cfg, &project.id);
    // Tokens now come from the linked github key (set in Settings). Show the
    // key's label when one is linked; fall back to "stored (legacy)" for a
    // pre-existing per-project token, or "none" for a public repo.
    let credentials = match cfg.github_key(&project) {
        Some(key) => key.label.as_str(),
        None => {
            if !project.git_token.is_empty() {
                "stored (legacy)"
            } else {
                "none (public repo)"
            }
        }
    };
    // Probed per view rather than cached: one `docker compose config` keeps
    // this card honest about what a rollback here would actually restore.
    let database = backup::database_for(&project).await;
    let backups = backup::load_index(&project.id);
    let database_label = {
        let label = match &database {
            Some(db) => format!("{} — service “{}”", db.kind.as_str(), db.service),
            None if project.database.mode == "off" => {
                "off — rollbacks restore code only".to_string()
            }
            None => "none detected — rollbacks restore code only".to_string(),
        };
        // Name the compose file detection is pinned to, when one is set: it
        // is the difference between "nothing there" and "wrong file", and
        // this card is what an operator reads before trusting a rollback.
        if project.database.compose_file.is_empty() {
            label
        } else {
            format!("{label} · {}", project.database.compose_file)
        }
    };
    let backups_label = if project.database.backups {
        format!("on, keeping the newest {}", project.database.keep)
    } else {
        "off".to_string()
    };

    let body = html! {
        section class="page-head" {
            h1 { (project.name) }
            div class="head-actions" {
                a class="button" href={ "/projects/" (project.id) "/edit" } { "Edit" }
                a class="button danger" href={ "/projects/" (project.id) "/delete" } { "Delete" }
            }
        }

        section class="card" {
            h2 { "Deploy" }
            div class="actions" {
                form method="post" action={ "/projects/" (project.id) "/update-app" } {
                    button type="submit" class="button primary" { "Pull & deploy" }
                }
                form method="post" action={ "/projects/" (project.id) "/deploy" } {
                    button type="submit" class="button" { "Redeploy without pulling" }
                }
                @if let Some(run_id) = &running {
                    form method="post" action={ "/projects/" (project.id) "/cancel" }
                         hx-confirm="Cancel the running deploy? The process will be killed." {
                        button type="submit" class="button danger" { "Cancel running" }
                    }
                    // A direct link so the operator can watch the run they are
                    // about to stop, without navigating through the run list.
                    a class="button" href={ "/runs/" (run_id) } { "View live run" }
                }
            }
        }

        section class="card" {
            h2 { "Configuration" }
            (views::code_field("ID", &project.id))
            (views::code_field("Path", &project.path))
            @if !project.repository.is_empty() {
                (views::code_field("Repository", &project.repository))
                (views::field("GitHub key", credentials))
            }
            (views::code_field("Branch", &project.branch))
            (views::field("Deployment", project.preset_label()))
            @if project.uses_compose() {
                (views::code_field("Compose file", &project.compose_file))
                @if !project.compose_profiles.is_empty() {
                    (views::code_field("Profiles", &project.compose_profiles.join(", ")))
                }
            }
            div class="field" {
                span class="field-label" { "Runs" }
                span class="field-value" {
                    pre class="log cmd-preview" { (config::deploy_command_preview(&project)) }
                }
            }
        }

        section class="card" {
            h2 { "Database & backups" }
            (views::field("Database", &database_label))
            (views::field("Pre-deploy backups", &backups_label))
            @if backup_started {
                // The dump runs in the background, so the list below cannot
                // have changed yet — say so instead of looking like a no-op.
                p class="warn-inline" {
                    "Manual backup started — it appears in the list below when the dump "
                    "finishes (failures are narrated in manual.log under the project's "
                    "backups directory)."
                }
            }
            @if database.is_some() {
                form method="post" action={ "/projects/" (project.id) "/backup" } {
                    button type="submit" class="button small" { "Back up now" }
                }
            }
            @if backups.is_empty() {
                p class="muted" {
                    "No backups yet — the next deploy with backups on takes one"
                    @if database.is_none() {
                        " (none will be taken until a database is detected or configured)"
                    }
                    "."
                }
            } @else {
                div class="table-scroll" {
                    table {
                        thead { tr {
                            th { "Saved" } th { "Engine" } th { "Taken" } th { "File" } th { "Size" }
                        } }
                        tbody {
                            @for backup in &backups {
                                tr {
                                    td class="mono small" title=(backup.created_at) {
                                        (views::jakarta_time(&backup.created_at))
                                    }
                                    td class="small" { (backup.kind) }
                                    td class="small" {
                                        (backup.label)
                                        @if !backup.before_run.is_empty() {
                                            " — "
                                            // The run whose undo restores this
                                            // dump; one hop to see what it was.
                                            a href={ "/runs/" (backup.before_run) } { "view run" }
                                        }
                                    }
                                    td class="mono small" { (backup.file) }
                                    td class="mono small" { (human_size(backup.size)) }
                                }
                            }
                        }
                    }
                }
            }
        }

        section class="card" {
            h2 { "Webhook" }
            (views::code_field("URL", &hook_url))
            (views::field("Verification", verify_label(&project.verify_mode)))
            (views::field("Triggers", &project.triggers_label()))

            @if project.status_reports {
                (views::code_field("Commit status", &project.effective_status_context()))
                @if let Some(problem) = cfg.status_report_problem(&project) {
                    div class="field" {
                        span class="field-label" {}
                        span class="field-value warn-inline" { (problem) }
                    }
                }
            } @else {
                (views::field("Commit status", "not reported"))
            }

            div class="field" {
                span class="field-label" { "Secret" }
                span class="field-value" {
                    // Kept out of the initial HTML so it is not sitting in a
                    // page left open on a second screen.
                    span id="secret" class="mono" { "••••••••••••" }
                    button class="button small"
                        hx-get={ "/f/projects/" (project.id) "/secret" }
                        hx-target="#secret" hx-swap="outerHTML" { "Reveal" }
                }
            }

            details {
                summary { "Example request" }
                pre class="log" { (curl_example(&project, &hook_url)) }
            }

            form method="post" action={ "/projects/" (project.id) "/secret/rotate" }
                 hx-confirm="Rotate this secret? The old one stops working immediately." {
                button type="submit" class="button danger small" { "Rotate secret" }
            }
        }

        section class="card" {
            h2 { "Recent runs" }
            @if runs.is_empty() {
                p class="muted" { "No runs yet." }
            } @else {
                ul class="run-list" {
                    @for run in &runs {
                        li {
                            (views::status_badge(Some(run)))
                            a href={ "/runs/" (run.id) } title=(run.started_at) {
                                (views::jakarta_time(&run.started_at))
                            }
                            span class="muted" { (views::duration(run)) }
                            @if let Some(author) = run.author.as_deref() {
                                span class="muted small" { (author) }
                            }
                            @if let Some(trigger) = run.triggered_by.as_deref() {
                                span class="muted small" { (trigger) }
                            }
                            span class="summary" { (run.message) }
                        }
                    }
                }
            }
        }
    };
    Ok(views::page(&project.name, body))
}

fn verify_label(mode: &str) -> &'static str {
    match mode {
        "token" => "Shared token (X-Webhookr-Key header)",
        _ => "GitHub signature (X-Hub-Signature-256)",
    }
}

fn curl_example(project: &ProjectConfig, url: &str) -> String {
    match project.verify_mode.as_str() {
        "token" => format!("curl -X POST -H 'X-Webhookr-Key: <secret>' \\\n  {url}"),
        _ => format!(
            "# GitHub sends this automatically once the secret is set.\n\
             # Settings -> Webhooks -> Add webhook\n\
             #   Payload URL:  {url}\n\
             #   Content type: application/json\n\
             #   Secret:       <secret>"
        ),
    }
}

/// Fragment: the plaintext secret, swapped in on demand.
pub async fn reveal_secret(Path(id): Path<String>) -> Result<Markup, WebError> {
    let (_, project) = load_project(&id)?;
    Ok(html! { span id="secret" class="mono selectable" { (project.secret) } })
}

// ----- create & edit -----------------------------------------------------

pub async fn new_form() -> Result<Markup, WebError> {
    let blank = ProjectForm {
        name: String::new(),
        id: String::new(),
        repository: String::new(),
        repository_pick: String::new(),
        github_key_id: String::new(),
        path: String::new(),
        branch: "main".to_string(),
        command: String::new(),
        deploy_preset: "custom".to_string(),
        compose_file: "compose.yaml".to_string(),
        compose_profiles: String::new(),
        verify_mode: "github".to_string(),
        trigger_push: None,
        trigger_merge: None,
        merge_sources: String::new(),
        status_reports: None,
        status_context: String::new(),
        db_mode: String::new(),
        db_kind: String::new(),
        db_service: String::new(),
        db_compose_file: String::new(),
        db_backups: None,
        db_keep: String::new(),
    };
    Ok(views::page(
        "Add project",
        project_form("Add project", "/projects", &blank, None, None),
    ))
}

pub async fn edit_form(Path(id): Path<String>) -> Result<Markup, WebError> {
    let (_, project) = load_project(&id)?;
    let form = ProjectForm::from_project(&project);
    Ok(views::page(
        "Edit project",
        project_form(
            &format!("Edit {}", project.name),
            &format!("/projects/{}", project.id),
            &form,
            Some(&project.id),
            None,
        ),
    ))
}

pub async fn create(Form(form): Form<ProjectForm>) -> Result<Response, WebError> {
    let project = match form.to_project(None) {
        Ok(project) => project,
        // Re-render with what they typed rather than throwing it away.
        Err(error) => {
            return Ok((
                StatusCode::UNPROCESSABLE_ENTITY,
                views::page(
                    "Add project",
                    project_form(
                        "Add project",
                        "/projects",
                        &form,
                        None,
                        Some(&forms::explain(&error)),
                    ),
                ),
            )
                .into_response())
        }
    };

    let id = project.id.clone();
    let taken = config::update_config(|cfg| {
        if cfg.get(&id).is_some() {
            anyhow::bail!("a project with id '{id}' already exists");
        }
        cfg.upsert(project);
        Ok(())
    });

    match taken {
        Ok(()) => Ok(Redirect::to(&format!("/projects/{id}")).into_response()),
        Err(error) => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            views::page(
                "Add project",
                project_form(
                    "Add project",
                    "/projects",
                    &form,
                    None,
                    Some(&forms::explain(&error)),
                ),
            ),
        )
            .into_response()),
    }
}

pub async fn update(
    Path(id): Path<String>,
    Form(form): Form<ProjectForm>,
) -> Result<Response, WebError> {
    let (_, existing) = load_project(&id)?;

    let project = match form.to_project(Some(&existing)) {
        Ok(project) => project,
        Err(error) => {
            return Ok((
                StatusCode::UNPROCESSABLE_ENTITY,
                views::page(
                    "Edit project",
                    project_form(
                        &format!("Edit {}", existing.name),
                        &format!("/projects/{id}"),
                        &form,
                        Some(&id),
                        Some(&forms::explain(&error)),
                    ),
                ),
            )
                .into_response())
        }
    };

    config::update_config(|cfg| {
        cfg.upsert(project);
        Ok(())
    })?;
    Ok(Redirect::to(&format!("/projects/{id}")).into_response())
}

/// The add/edit form: every field the TUI's nine-step wizard collects, at once.
fn project_form(
    title: &str,
    action: &str,
    form: &ProjectForm,
    existing_id: Option<&str>,
    error: Option<&str>,
) -> Markup {
    // The context the project will actually report under, so the placeholder
    // shows the real default rather than a generic example.
    let context_placeholder = match existing_id {
        Some(id) => format!("webhookr/{id}"),
        None => "webhookr/<slug>".to_string(),
    };
    // The database section's display defaults: a blank mode/keep (a fresh
    // form, or a validation re-render that never saw a value) shows as
    // auto/5 — what a save would actually store — rather than an empty
    // control that quietly means the same thing.
    let db_mode = if form.db_mode.trim().is_empty() {
        "auto"
    } else {
        form.db_mode.trim()
    };
    let db_keep = if form.db_keep.trim().is_empty() {
        "5"
    } else {
        form.db_keep.trim()
    };
    html! {
        section class="page-head" { h1 { (title) } }
        @if let Some(error) = error { (views::alert("error", error)) }

        form method="post" action=(action) class="card form" {
            fieldset {
                legend { "Source" }

                label {
                    span { "Name" }
                    input type="text" name="name" value=(form.name) required
                          placeholder="My Site"
                          hx-get="/f/slug" hx-target="#slug" hx-swap="innerHTML"
                          hx-trigger="keyup changed delay:300ms";
                }
                @match existing_id {
                    Some(id) => {
                        p class="hint" { "URL slug: " code class="mono" { (id) } " (cannot be changed)" }
                        input type="hidden" name="id" value=(id);
                    }
                    None => p class="hint" {
                        "URL slug: " code id="slug" class="mono" {
                            @if form.name.is_empty() { "—" } @else { (crate::cli::slugify(&form.name)) }
                        }
                    }
                }

                // The repository picker: a list of the user's GitHub repos
                // (fetched from the shared github keys) loads here on page
                // load, with a manual-URL fallback. `hx-include` sends the
                // current form values so the fragment can pre-fill the URL
                // and key on edit or after a validation re-render.
                div id="repo-picker"
                     hx-get="/f/github-repos" hx-trigger="load"
                     hx-include="closest form" hx-swap="innerHTML" {}

                label {
                    span { "Path on this server" }
                    span class="input-row" {
                        input type="text" id="path" name="path" value=(form.path) required
                              placeholder="/srv/my-site";
                        button type="button" class="button small"
                               hx-get="/f/browse" hx-target="#browser" hx-swap="innerHTML" { "Browse…" }
                        button type="button" class="button small"
                               hx-get="/f/path-check" hx-include="#path"
                               hx-target="#path-check" hx-swap="innerHTML" { "Check" }
                    }
                    span id="path-check" class="hint" {}
                }
                div id="browser" {}

                label {
                    span { "Branch" }
                    input type="text" name="branch" value=(form.branch) required placeholder="main";
                }
            }

            fieldset {
                legend { "Deployment" }
                div class="radio-group" {
                    @for (id, label, description) in PRESETS {
                        label class="radio" {
                            input type="radio" name="deploy_preset" value=(id)
                                  checked[form.deploy_preset == id]
                                  hx-get="/f/deploy-fields" hx-include="closest form"
                                  hx-target="#deploy-fields" hx-swap="innerHTML";
                            span class="radio-text" {
                                strong { (label) }
                                span class="hint" { (description) }
                            }
                        }
                    }
                }
                div id="deploy-fields" { (deploy_fields_markup(form)) }
            }

            fieldset {
                legend { "Database" }
                span class="hint" {
                    "\"auto\" detects the Postgres/MySQL/MariaDB service in the compose file, "
                    "and powers the pre-deploy backups a rollback restores from."
                }
                label {
                    span { "Mode" }
                    (views::dropdown(
                        "db_mode",
                        db_mode,
                        &[
                            ("auto", "Auto-detect from the compose file"),
                            ("manual", "Manual — override what's detected"),
                            ("off", "Off — no backups, code-only rollbacks"),
                        ],
                    ))
                }
                label {
                    span { "Engine" span class="optional" { "manual mode only" } }
                    (views::dropdown(
                        "db_kind",
                        &form.db_kind,
                        &[
                            ("", "Detect from the compose file"),
                            ("postgres", "Postgres"),
                            ("mysql", "MySQL"),
                            ("mariadb", "MariaDB"),
                        ],
                    ))
                    span class="hint" {
                        "Ignored in auto mode (detection decides at backup time) and when off. \
                         In manual mode, the detect option reads the engine from the file."
                    }
                }
                label {
                    span { "Database service" span class="optional" { "manual mode only" } }
                    input type="text" name="db_service" value=(form.db_service)
                          placeholder="db";
                    span class="hint" {
                        "The compose service running the database, e.g. "
                        code class="mono" { "db" } ". In manual mode, leaving it blank reads \
                         the service from the database compose file."
                    }
                }
                label {
                    span { "Database compose file" span class="optional" { "optional" } }
                    span class="input-row" {
                        input type="text" id="db_compose_file" name="db_compose_file"
                              class="mono" value=(form.db_compose_file)
                              placeholder="docker-compose.yml";
                        // The picker browses inside the checkout, so without
                        // a path there is nothing to list — the button waits
                        // and the hint names what to fill in first.
                        button type="button" class="button small"
                               disabled[form.path.trim().is_empty()]
                               hx-get="/f/browse-files"
                               hx-vals=(browse_files_vals(form.path.trim()))
                               hx-target="#db-file-browser" hx-swap="innerHTML" { "Browse…" }
                    }
                    span class="hint" {
                        "The compose file auto-detect reads, relative to the checkout. "
                        "Leave empty to use the preset's compose file; a custom-command "
                        "project must name the file that defines the database service."
                        @if form.path.trim().is_empty() {
                            " Fill in the path on this server above to enable Browse."
                        }
                    }
                }
                div id="db-file-browser" {}
                (views::checkbox(
                    "db_backups",
                    form.db_backups.is_some(),
                    "Back up the database before every deploy",
                    Some(
                        "This is what makes a rollback able to restore data as well as \
                         code. A failed dump fails the deploy before any code changes.",
                    ),
                ))
                label {
                    span { "Backups to keep" }
                    input type="number" name="db_keep" value=(db_keep) min="1" step="1";
                    span class="hint" {
                        "Dumps kept per project; the oldest are pruned after each new one."
                    }
                }
            }

            fieldset {
                legend { "Webhook security" }
                div class="radio-group" {
                    label class="radio" {
                        input type="radio" name="verify_mode" value="github"
                              checked[form.verify_mode != "token"];
                        span class="radio-text" {
                            strong { "GitHub signature" }
                            span class="hint" {
                                "Verifies X-Hub-Signature-256. Use this for GitHub webhooks."
                            }
                        }
                    }
                    label class="radio" {
                        input type="radio" name="verify_mode" value="token"
                              checked[form.verify_mode == "token"];
                        span class="radio-text" {
                            strong { "Shared token" }
                            span class="hint" {
                                "Sender must pass the secret in an X-Webhookr-Key header."
                            }
                        }
                    }
                }
            }

            fieldset {
                legend { "Deployment triggers" }
                span class="hint" {
                    "Which GitHub deliveries start a deploy. Leave both unticked to deploy on "
                    "every delivery (the default)."
                }
                (views::checkbox(
                    "trigger_push",
                    form.trigger_push.is_some(),
                    "Push",
                    Some(
                        "A push to the configured branch — including the merge commit \
                         pushed after a PR merge.",
                    ),
                ))
                (views::checkbox(
                    "trigger_merge",
                    form.trigger_merge.is_some(),
                    "Merge",
                    Some(
                        "A pull request closed and merged. Needs the pull_request event \
                         enabled in your GitHub webhook settings; a ping never triggers \
                         a deploy.",
                    ),
                ))

                label {
                    span { "Merge from branches" span class="optional" { "optional" } }
                    input type="text" name="merge_sources" value=(form.merge_sources)
                          placeholder="develop, staging";
                    span class="hint" {
                        "Only merges whose pull request came from these branches trigger a "
                        "deploy. Comma-separated; empty means any merge."
                    }
                }
            }

            fieldset {
                legend { "GitHub commit status" }

                (views::checkbox(
                    "status_reports",
                    form.status_reports.is_some(),
                    "Report deploy results on the commit",
                    Some(
                        "GitHub shows the pending dot, green check or red X beside the \
                         deployed commit on the repository page. Statuses are posted \
                         with the linked GitHub key (set in Settings).",
                    ),
                ))

                label {
                    span { "Context" span class="optional" { "optional" } }
                    input type="text" name="status_context" value=(form.status_context)
                          placeholder=(context_placeholder);
                    span class="hint" {
                        "The label GitHub shows beside the status. Defaults to one per "
                        "project, so two projects deploying from one repository do not "
                        "overwrite each other."
                    }
                }
            }

            div class="form-actions" {
                button type="submit" class="button primary" { "Save project" }
                a class="button" href="/projects" { "Cancel" }
            }
        }
    }
}

/// JSON object for htmx's `hx-vals`, so the checkout path survives quoting
/// intact — same trick as the directory picker's select fragment.
fn browse_files_vals(root: &str) -> String {
    serde_json::json!({ "root": root }).to_string()
}

/// Fragment swapped in when the preset radio changes.
///
/// Rendered server-side so the page still works without JavaScript — the form
/// simply shows the fields for whichever preset is currently selected.
pub async fn deploy_fields(Query(query): Query<PresetQuery>) -> Markup {
    let form = ProjectForm {
        name: String::new(),
        id: String::new(),
        repository: String::new(),
        repository_pick: String::new(),
        github_key_id: String::new(),
        path: String::new(),
        branch: String::new(),
        command: query.command.unwrap_or_default(),
        deploy_preset: query.deploy_preset.unwrap_or_else(|| "custom".to_string()),
        compose_file: query
            .compose_file
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "docker-compose.yml".to_string()),
        compose_profiles: query.compose_profiles.unwrap_or_default(),
        verify_mode: String::new(),
        trigger_push: None,
        trigger_merge: None,
        // The deploy-fields preview never renders the trigger fields.
        merge_sources: String::new(),
        status_reports: None,
        status_context: String::new(),
        // Nor the database section: the preview only answers for the preset.
        db_mode: String::new(),
        db_kind: String::new(),
        db_service: String::new(),
        db_compose_file: String::new(),
        db_backups: None,
        db_keep: String::new(),
    };
    deploy_fields_markup(&form)
}

/// The subset of the form the preview needs. Sent by `hx-include="closest form"`,
/// so every other field arrives too and is simply ignored.
#[derive(Deserialize)]
pub struct PresetQuery {
    pub deploy_preset: Option<String>,
    pub compose_file: Option<String>,
    pub compose_profiles: Option<String>,
    pub command: Option<String>,
}

fn deploy_fields_markup(form: &ProjectForm) -> Markup {
    let compose = form.deploy_preset.starts_with("compose_");
    html! {
        @if compose {
            label {
                span { "Compose file" }
                input type="text" name="compose_file" value=(form.compose_file)
                      placeholder="docker-compose.yml"
                      hx-get="/f/deploy-fields" hx-include="closest form"
                      hx-target="#deploy-fields" hx-swap="innerHTML"
                      hx-trigger="keyup changed delay:400ms";
                span class="hint" {
                    "Path relative to the checkout — for example "
                    code class="mono" { "docker-compose.yml" } " or "
                    code class="mono" { "deploy/compose.prod.yml" } ". "
                    "It cannot use '..' to escape the project directory."
                }
            }
            label {
                span { "Compose profiles" span class="optional" { "optional" } }
                input type="text" name="compose_profiles" value=(form.compose_profiles)
                      placeholder="web, worker"
                      hx-get="/f/deploy-fields" hx-include="closest form"
                      hx-target="#deploy-fields" hx-swap="innerHTML"
                      hx-trigger="keyup changed delay:400ms";
                span class="hint" { "Comma-separated." }
            }
        } @else {
            label {
                span { "Command" }
                textarea name="command" rows="3" placeholder="./deploy.sh" { (form.command) }
                span class="hint" { "Runs with sh -c from the project directory." }
            }
        }
        (command_preview(form))
    }
}

/// Show the exact command a deploy will run, built from the same helper the
/// project page uses, so the form is never a guess about what happens.
fn command_preview(form: &ProjectForm) -> Markup {
    let preview = config::deploy_command_preview(&ProjectConfig {
        deploy_preset: form.deploy_preset.clone(),
        compose_file: if form.compose_file.trim().is_empty() {
            "docker-compose.yml".to_string()
        } else {
            form.compose_file.trim().to_string()
        },
        compose_profiles: form
            .compose_profiles
            .split([',', '\n'])
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
        command: form.command.clone(),
        ..ProjectConfig::new(
            "preview".into(),
            "preview".into(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            "github".into(),
        )
    });

    html! {
        div id="cmd-preview-wrap" {
            span class="field-label" { "Runs" }
            @if preview.trim().is_empty() {
                p class="hint" { "Fill in the command above." }
            } @else {
                pre id="cmd-preview" class="log cmd-preview" { (preview) }
            }
        }
    }
}

/// Fragment: live slug preview under the name field.
pub async fn slug_preview(Query(query): Query<SlugQuery>) -> Markup {
    let name = query.name.unwrap_or_default();
    html! {
        @if name.trim().is_empty() { "—" } @else { (crate::cli::slugify(&name)) }
    }
}

#[derive(Deserialize)]
pub struct SlugQuery {
    pub name: Option<String>,
}

// ----- repo picker fragment ----------------------------------------------

/// Fragment: the repository picker.
///
/// Loaded into `#repo-picker` on the project form via `hx-get` on page load.
/// `hx-include="closest form"` sends the current form values so the manual URL
/// and key dropdown can be pre-filled on edit or after a validation re-render.
///
/// Each repo renders as a radio whose value encodes both the clone URL and the
/// owning key id as `"clone_url\tkey_id"`; [`ProjectForm::to_project`] splits
/// that pair. A separate manual URL field (`name="repository"`) takes
/// precedence when filled, paired with a dropdown of all github keys for the
/// manual-URL case. No inline JavaScript is needed — the CSP forbids it.
pub async fn github_repos(Query(query): Query<RepoPickerQuery>) -> Result<maud::Markup, WebError> {
    let cfg = config::load_config()?;
    let current_repo = query.repository.as_deref().unwrap_or("");
    let current_key = query.github_key_id.as_deref().unwrap_or("");

    // The key dropdown's options: "no key" first, then one entry per
    // configured key — the same pairs the native select offered.
    let mut key_options: Vec<(&str, &str)> = vec![("", "no key / public repo")];
    for key in &cfg.github_keys {
        key_options.push((key.id.as_str(), key.label.as_str()));
    }

    // Fetch repos from every configured github key, best-effort: a key whose
    // token is bad or rate-limited should not blank out the others.
    let mut repos = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for key in &cfg.github_keys {
        match crate::github::list_repos(&key.token, &key.host, &key.id, &key.label).await {
            Ok(mut list) => repos.append(&mut list),
            Err(err) => errors.push(format!("{}: {err}", key.label)),
        }
    }

    Ok(html! {
        @if cfg.github_keys.is_empty() {
            p class="hint" {
                "No GitHub keys configured — add one in Settings, or type a repository URL below."
            }
        } @else {
            @if !repos.is_empty() {
                label {
                    span { "Your repositories" }
                    div class="radio-group" {
                        @for repo in &repos {
                            @let encoded = format!("{}\t{}", repo.clone_url, repo.key_id);
                            label class="radio" {
                                input type="radio" name="repository_pick"
                                      value=(encoded)
                                      checked[!current_repo.is_empty()
                                              && current_repo == repo.clone_url];
                                span class="radio-text" {
                                    (repo.full_name)
                                    @if repo.private {
                                        span class="muted small" { " (private)" }
                                    }
                                    span class="muted small" { " — " (repo.key_label) }
                                }
                            }
                        }
                    }
                    span class="hint" {
                        "Pick a repository to link its GitHub key for clone, pull and commit statuses."
                    }
                }
                p class="hint" { "— or type a URL —" }
            }
            @for err in &errors {
                p class="hint warn" { (err) }
            }
        }

        label {
            span { "Repository URL" span class="optional" { "optional" } }
            input type="url" name="repository" value=(current_repo)
                  placeholder="https://github.com/owner/repo.git";
            span class="hint" {
                "If set and the path below does not exist yet, webhookr clones it on the first run."
            }
        }

        @if !cfg.github_keys.is_empty() {
            label {
                span { "GitHub key" span class="optional" { "for a typed URL" } }
                (views::dropdown("github_key_id", current_key, &key_options))
                span class="hint" {
                    "Used for clone, pull and commit statuses when you type a URL above."
                }
            }
        }
    })
}

/// The subset of the form the repo picker needs to pre-fill its fields. Sent by
/// `hx-include="closest form"`, so every other field arrives too and is simply
/// ignored.
#[derive(Deserialize)]
pub struct RepoPickerQuery {
    pub repository: Option<String>,
    pub github_key_id: Option<String>,
}

// ----- destructive actions ----------------------------------------------

pub async fn delete_confirm(Path(id): Path<String>) -> Result<Markup, WebError> {
    let (_, project) = load_project(&id)?;
    let body = html! {
        section class="card" {
            h1 { "Delete " (project.name) "?" }
            p {
                "This removes the webhook route and its configuration. The checkout at "
                code class="mono" { (project.path) } " is left untouched."
            }
            div class="actions" {
                form method="post" action={ "/projects/" (project.id) "/delete" } {
                    button type="submit" class="button danger" { "Delete project" }
                }
                a class="button" href={ "/projects/" (project.id) } { "Cancel" }
            }
        }
    };
    Ok(views::page("Delete project", body))
}

pub async fn delete(Path(id): Path<String>) -> Result<Redirect, WebError> {
    config::update_config(|cfg| {
        cfg.remove(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown project '{id}'"))?;
        Ok(())
    })?;
    Ok(Redirect::to("/projects"))
}

pub async fn rotate_secret(Path(id): Path<String>) -> Result<Redirect, WebError> {
    config::update_config(|cfg| {
        let project = cfg
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown project '{id}'"))?;
        project.secret = util::generate_secret();
        Ok(())
    })?;
    Ok(Redirect::to(&format!("/projects/{id}")))
}

// ----- triggers ----------------------------------------------------------

pub async fn deploy(Path(id): Path<String>) -> Result<Response, WebError> {
    trigger(&id, false).await
}

pub async fn update_app(Path(id): Path<String>) -> Result<Response, WebError> {
    trigger(&id, true).await
}

/// Cancel the project's in-flight run, if there is one, and go watch it. A
/// run that is not actually `running` has no cancel signal to trip, so just
/// land the operator back on the project page.
pub async fn cancel(Path(id): Path<String>) -> Result<Response, WebError> {
    let running = state::load_runs()
        .into_iter()
        .find(|r| r.project_id == id && r.status == "running");
    match running {
        Some(run) => {
            executor::cancel_run(&run.id);
            Ok(Redirect::to(&format!("/runs/{}", run.id)).into_response())
        }
        None => Ok(Redirect::to(&format!("/projects/{id}")).into_response()),
    }
}

/// Start a run and send the browser to its log.
///
/// The executor's per-project lock means a second trigger while one is in
/// flight is refused rather than queued, so surface that as a message instead
/// of silently doing nothing.
async fn trigger(id: &str, sync_source: bool) -> Result<Response, WebError> {
    let (_, project) = load_project(id)?;

    let before: std::collections::HashSet<String> =
        state::load_runs().into_iter().map(|r| r.id).collect();

    let handle = tokio::spawn(async move {
        // Labelled `web UI` so the run's history says a person clicked a
        // button here, as against a push, a workflow, or the CLI.
        let trigger = executor::Trigger {
            payload: None,
            source: Some("web UI".to_string()),
        };
        let result = if sync_source {
            executor::run_project_with(&project, trigger).await
        } else {
            executor::deploy_project_with(&project, trigger).await
        };
        if let Err(error) = result {
            eprintln!("webhookr: run failed to start: {error:#}");
        }
    });

    // Give the executor a moment to register its `running` record so we can
    // redirect straight to the live log instead of a stale list.
    for _ in 0..20 {
        if handle.is_finished() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if let Some(run) = state::load_runs()
            .into_iter()
            .find(|r| r.project_id == id && !before.contains(&r.id))
        {
            return Ok(Redirect::to(&format!("/runs/{}", run.id)).into_response());
        }
    }

    // No new record: almost always the single-flight guard refusing a
    // concurrent run. Point at the project so the state is visible.
    Ok(Redirect::to(&format!("/projects/{id}")).into_response())
}

// ----- rollback ------------------------------------------------------------

/// The confirm page's query: which run to roll back to.
#[derive(Deserialize)]
/// The project page's query flags. `backed-up` is set by the "Back up now"
/// redirect so the page can say a dump is on its way before it lands.
pub struct DetailQuery {
    pub backed_up: Option<String>,
}

#[derive(Deserialize)]
pub struct RollbackQuery {
    pub to: Option<String>,
}

/// The confirm page's POST body: the chosen run, plus the restore checkbox.
/// `restore_db` follows the checkbox-presence pattern (see
/// [`ProjectForm::status_reports`]) — an unticked box sends no key at all.
#[derive(Deserialize)]
pub struct RollbackForm {
    pub run_id: String,
    pub restore_db: Option<String>,
}

/// Confirm rolling a project back to a recorded run.
///
/// A GET on purpose: the page itself changes nothing, and the destructive half
/// is the POST from it — so a prefetched or shared link can only ever show the
/// operator what is about to happen. That is also why the run page's rollback
/// button needs no `hx-confirm` of its own.
pub async fn rollback_form(
    Path(id): Path<String>,
    Query(query): Query<RollbackQuery>,
) -> Result<Markup, WebError> {
    let (_, project) = load_project(&id)?;

    // The target run. An unknown (or missing) id is a broken link rather than
    // a broken rule, so it 404s like any other unknown run.
    let run_id = query.to.unwrap_or_default();
    let run = state::load_runs()
        .into_iter()
        .find(|r| r.id == run_id.trim())
        .ok_or_else(|| WebError::not_found("run"))?;

    // A rollback replays the commit a run deployed, so the target must be a
    // successful run of this project that recorded one. Anything else (a
    // failed run, a run from before commits were recorded, another project's
    // run) gets an explanation rather than a bare error — the run plainly
    // exists, so the operator followed a real link to get here.
    if run.project_id != project.id || run.status != "success" || run.commit.is_none() {
        let body = html! {
            section class="card" {
                h1 { "Rollback " (project.name) }
                p {
                    "Rollback replays the commit a run deployed, so the target has to be "
                    "a successful run of this project with a recorded commit. That run "
                    "does not qualify, so there is nothing to roll back to."
                }
                div class="actions" {
                    a class="button" href={ "/runs/" (run.id) } { "Back to the run" }
                }
            }
        };
        return Ok(views::page("Rollback", body));
    }

    // The data half, decided live rather than from a stored flag, so the page
    // states what a rollback would do *now*: detection reads the current
    // compose file, and the manifest says whether the dump this restore would
    // replay still exists. Backups switched off in the project's database
    // block skip detection entirely — there would be nothing to offer anyway.
    let db = if project.database.backups {
        backup::database_for(&project).await
    } else {
        None
    };

    let sha = run.commit.clone().unwrap_or_default();
    // Char-boundary-safe, and the same seven characters GitHub prints.
    let short: String = sha.chars().take(7).collect();

    let body = html! {
        section class="card" {
            h1 { "Rollback " (project.name) }
            p {
                "The checkout returns to the commit this run deployed and the "
                "deployment runs again from it. Later runs stay in the history."
            }
            (views::field("Roll back to", &short))
            (views::code_field("Commit", &sha))
            (views::field("Author", run.author.as_deref().unwrap_or("—")))
            (views::code_field("Started (WIB)", &views::jakarta_time(&run.started_at)))
            @if !run.message.is_empty() {
                (views::field("Summary", &run.message))
            }
        }

        section class="card" {
            h2 { "Database" }
            // The form wraps the whole section, not just the button: the
            // restore checkbox below only posts if it sits inside the
            // <form> element.
            form method="post" action={ "/projects/" (project.id) "/rollback" } {
                input type="hidden" name="run_id" value=(run.id);
                @match db {
                    None => {
                        // Either detection found nothing or backups are off in
                        // the project's database block — both mean there is no
                        // data half to offer, so no checkbox either.
                        p class="muted" {
                            "No database backups configured — this rollback restores "
                            "code only."
                        }
                    }
                    Some(db) => {
                        (views::field("Engine", db.kind.as_str()))
                        (views::code_field("Service", &db.service))
                        @match backup::backup_before_run(&project.id, &run.id) {
                            Some(dump) => {
                                (views::code_field(
                                    "Backup taken (WIB)",
                                    &views::jakarta_time(&dump.created_at),
                                ))
                                (views::code_field("Backup file", &dump.file))
                                (views::field("Backup size", &human_size(dump.size)))
                                (views::checkbox(
                                    "restore_db",
                                    true,
                                    "Restore the database backup taken before that deploy",
                                    Some(
                                        "Untick to move only the code back; the database \
                                         keeps everything the deploys since then wrote.",
                                    ),
                                ))
                            }
                            None => {
                                // The run predates backups (or its dump was
                                // pruned). Nothing to restore to that point,
                                // so the checkbox is simply absent — an
                                // absent checkbox cannot post restore_db.
                                p class="warn-inline" {
                                    "No backup was recorded before that run — the "
                                    "database cannot be restored to that point. The "
                                    "rollback will restore code only."
                                }
                            }
                        }
                    }
                }
                div class="actions" {
                    button type="submit" class="button danger" { "Rollback now" }
                    a class="button" href={ "/runs/" (run.id) } { "Cancel" }
                }
            }
        }
    };
    Ok(views::page("Rollback", body))
}

/// Replay a recorded run: code to its commit, database to the dump taken
/// before it. Everything the confirm page checked is re-checked here, because
/// a POST can arrive stale (the history moved on) or hand-built.
pub async fn rollback(
    Path(id): Path<String>,
    Form(params): Form<RollbackForm>,
) -> Result<Response, WebError> {
    let (_, project) = load_project(&id)?;

    // The same rules the confirm page applied. A failure here is a bad
    // request, not a missing page: the project exists, the message is about
    // the run the form named.
    let run = state::load_runs()
        .into_iter()
        .find(|r| r.id == params.run_id.trim())
        .filter(|r| r.project_id == project.id && r.status == "success" && r.commit.is_some())
        .ok_or_else(|| {
            WebError::new(
                StatusCode::BAD_REQUEST,
                anyhow::anyhow!(
                    "rollback needs a successful run of this project with a recorded commit"
                ),
            )
        })?;

    // Only when the (untickable) box was ticked. The executor re-checks that
    // a usable dump exists, so a stale tick cannot restore a pruned file.
    let restore_db = params.restore_db.is_some();

    let before: std::collections::HashSet<String> =
        state::load_runs().into_iter().map(|r| r.id).collect();

    let handle = tokio::spawn(async move {
        // Labelled so the run's history says a person chose to replay this
        // deploy from the admin UI, as against a push, a manual run, or the CLI.
        let trigger = executor::Trigger {
            payload: None,
            source: Some("rollback (web UI)".to_string()),
        };
        let result = executor::rollback_project_with(&project, &run, restore_db, trigger).await;
        if let Err(error) = result {
            eprintln!("webhookr: rollback failed to start: {error:#}");
        }
    });

    // Give the executor a moment to register its `running` record so we can
    // redirect straight to the live log instead of a stale list.
    for _ in 0..20 {
        if handle.is_finished() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if let Some(run) = state::load_runs()
            .into_iter()
            .find(|r| r.project_id == id && !before.contains(&r.id))
        {
            return Ok(Redirect::to(&format!("/runs/{}", run.id)).into_response());
        }
    }

    // No new record: almost always the single-flight guard refusing a
    // concurrent run. Point at the project so the state is visible.
    Ok(Redirect::to(&format!("/projects/{id}")).into_response())
}

/// Start a manual database dump in the background and return to the project
/// page. Fire-and-forget on purpose: a big database's dump runs for minutes,
/// and holding the request open for it would only time out at a reverse
/// proxy. The dump lands in the backups card (and `manual.log` in the
/// project's backups directory narrates failures).
pub async fn backup_now(Path(id): Path<String>) -> Result<Response, WebError> {
    let (_, project) = load_project(&id)?;
    tokio::spawn(async move {
        if let Err(error) = executor::manual_backup(&project).await {
            eprintln!(
                "webhookr: manual backup of {} failed: {error:#}",
                project.id
            );
        }
    });
    Ok(Redirect::to(&format!("/projects/{id}?backed-up=1")).into_response())
}

/// Bytes as KiB/MiB for the confirm page: dump sizes span orders of
/// magnitude, and a raw byte count makes the operator do the math.
fn human_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let size = bytes as f64;
    if size >= KIB * KIB * KIB {
        format!("{:.1} GiB", size / (KIB * KIB * KIB))
    } else if size >= KIB * KIB {
        format!("{:.1} MiB", size / (KIB * KIB))
    } else if size >= KIB {
        format!("{:.1} KiB", size / KIB)
    } else {
        format!("{bytes} B")
    }
}
