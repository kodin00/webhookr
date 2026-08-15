//! Project CRUD, secrets, and deploy triggers.

use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Form,
};
use maud::{html, Markup};
use serde::Deserialize;

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

pub async fn detail(Path(id): Path<String>) -> Result<Markup, WebError> {
    let (cfg, project) = load_project(&id)?;
    let runs: Vec<_> = state::load_runs()
        .into_iter()
        .filter(|r| r.project_id == id)
        .take(10)
        .collect();
    let hook_url = config::webhook_url(&cfg, &project.id);

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
            }
        }

        section class="card" {
            h2 { "Configuration" }
            (views::code_field("ID", &project.id))
            (views::code_field("Path", &project.path))
            @if !project.repository.is_empty() {
                (views::code_field("Repository", &project.repository))
            }
            @if !project.repository.is_empty() {
                (views::field(
                    "Access token",
                    if project.git_token.is_empty() { "none (public repo)" } else { "stored" },
                ))
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
            h2 { "Webhook" }
            (views::code_field("URL", &hook_url))
            (views::field("Verification", verify_label(&project.verify_mode)))

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
                            a href={ "/runs/" (run.id) } { (run.started_at) }
                            span class="muted" { (views::duration(run)) }
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
        git_token: String::new(),
        clear_git_token: None,
        path: String::new(),
        branch: "main".to_string(),
        command: String::new(),
        deploy_preset: "custom".to_string(),
        compose_file: "compose.yaml".to_string(),
        compose_profiles: String::new(),
        verify_mode: "github".to_string(),
    };
    Ok(views::page(
        "Add project",
        project_form("Add project", "/projects", &blank, None, None, false),
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
            !project.git_token.is_empty(),
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
                        false,
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
                    false,
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
                        !existing.git_token.is_empty(),
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
    has_token: bool,
) -> Markup {
    let token_placeholder = if has_token {
        "stored — leave blank to keep it"
    } else {
        "ghp_… or github_pat_…"
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

                label {
                    span { "Repository URL" span class="optional" { "optional" } }
                    input type="text" name="repository" value=(form.repository)
                          placeholder="https://github.com/me/site.git";
                    span class="hint" {
                        "If set and the path below does not exist yet, webhookr clones it on the first run."
                    }
                }

                label {
                    span { "Access token" span class="optional" { "private repos only" } }
                    input type="password" name="git_token" autocomplete="off"
                          placeholder=(token_placeholder);
                    span class="hint" {
                        "A GitHub personal access token with repo read access. Used for clone "
                        "and pull over HTTPS, handed to git through a credential helper so it "
                        "never lands in the process list or in .git/config."
                    }
                }
                @if has_token {
                    label class="checkbox" {
                        input type="checkbox" name="clear_git_token" value="1";
                        span { "Remove the stored token" }
                    }
                }

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

            div class="form-actions" {
                button type="submit" class="button primary" { "Save project" }
                a class="button" href="/projects" { "Cancel" }
            }
        }
    }
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
        git_token: String::new(),
        clear_git_token: None,
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
        let result = if sync_source {
            executor::run_project(&project).await
        } else {
            executor::deploy_project(&project).await
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
