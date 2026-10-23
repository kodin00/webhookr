//! Server-side directory picker, replacing the TUI's `DirBrowser`.
//!
//! Lists directories only — never file contents — but it does expose the shape
//! of the server's filesystem to anyone who reaches this port. That is the same
//! trust boundary as the rest of the admin UI (which can run arbitrary shell),
//! and it is why the UI is off by default and expected to sit behind Access.

use axum::extract::Query;
use maud::{html, Markup};
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

#[derive(Deserialize)]
pub struct PathQuery {
    pub path: Option<String>,
}

/// Query for the compose-file picker: `root` is the project path the browsing
/// is anchored to, `path` the directory currently listed (defaults to root).
#[derive(Deserialize)]
pub struct RootedQuery {
    pub root: Option<String>,
    pub path: Option<String>,
}

/// Resolve a rooted browse request: refuse `..` in either parameter (the same
/// stance as [`resolve`]) and clamp the current directory to `root` or below,
/// so the picker can never walk out of the project checkout. Absolute roots
/// only — a relative one would silently browse the server's working directory.
fn resolve_within(root: Option<&str>, requested: Option<&str>) -> Option<(PathBuf, PathBuf)> {
    let has_dotdot = |path: &Path| {
        path.components()
            .any(|part| matches!(part, Component::ParentDir))
    };
    let root = root
        .map(str::trim)
        .filter(|candidate| !candidate.is_empty())
        .map(PathBuf::from)?;
    if !root.is_absolute() || has_dotdot(&root) {
        return None;
    }
    let current = match requested
        .map(str::trim)
        .filter(|candidate| !candidate.is_empty())
    {
        Some(path) => PathBuf::from(path),
        None => root.clone(),
    };
    // `Path::starts_with` compares whole components, so "/srv/app-api" does
    // not count as inside "/srv/app".
    if has_dotdot(&current) || !current.starts_with(&root) {
        return None;
    }
    Some((root, current))
}

/// Resolve a requested directory, refusing `..` traversal outright.
fn resolve(requested: Option<&str>) -> PathBuf {
    let raw = requested.map(str::trim).filter(|p| !p.is_empty());
    let candidate = match raw {
        Some("~") | None => dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
        Some(path) => PathBuf::from(path),
    };
    if candidate
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        // Rather than normalising, refuse: the UI never needs to send "..".
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    }
    candidate
}

/// Fragment: the directory picker.
pub async fn list(Query(query): Query<PathQuery>) -> Markup {
    let current = resolve(query.path.as_deref());
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut error: Option<String> = None;

    match std::fs::read_dir(&current) {
        Ok(reader) => {
            for entry in reader.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                // Skip dotfiles: they are noise when picking a checkout.
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                entries.push((name, path.to_string_lossy().into_owned()));
            }
            entries.sort_by_key(|(name, _)| name.to_lowercase());
        }
        Err(err) => error = Some(format!("Cannot read this directory: {err}")),
    }

    let current_str = current.to_string_lossy().into_owned();
    let parent = current
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| !p.is_empty());

    html! {
        div class="card browser" {
            header class="browser-head" {
                strong class="mono" { (current_str) }
                button type="button" class="button small"
                        hx-get="/f/browse/select" hx-vals=(select_vals(&current_str))
                        hx-target="#browser" hx-swap="innerHTML" { "Use this directory" }
            }
            div class="browser-shortcuts" {
                button type="button" class="button small"
                        hx-get="/f/browse?path=~" hx-target="#browser" hx-swap="innerHTML" { "~ Home" }
                button type="button" class="button small"
                        hx-get="/f/browse?path=/" hx-target="#browser" hx-swap="innerHTML" { "/ Root" }
                @if let Some(parent) = &parent {
                    button type="button" class="button small"
                            hx-get=(format!("/f/browse?path={}", urlencode(parent)))
                            hx-target="#browser" hx-swap="innerHTML" { ".. Up" }
                }
                button type="button" class="button small"
                        hx-get="/f/browse/select" hx-vals=(select_vals(""))
                        hx-target="#browser" hx-swap="innerHTML" { "Close" }
            }
            @if let Some(error) = &error {
                p class="alert alert-error" { (error) }
            } @else if entries.is_empty() {
                p class="muted" { "No subdirectories here." }
            } @else {
                ul class="browser-list" {
                    @for (name, full) in &entries {
                        li {
                            button type="button" class="link"
                                    hx-get=(format!("/f/browse?path={}", urlencode(full)))
                                    hx-target="#browser" hx-swap="innerHTML" { (name) "/" }
                        }
                    }
                }
            }
        }
    }
}

/// Fragment: close the picker and push the chosen path into the form field.
///
/// The out-of-band swap updates `#path` without any inline script, which
/// matters because the CSP forbids inline JavaScript.
pub async fn select(Query(query): Query<PathQuery>) -> Markup {
    let chosen = query.path.unwrap_or_default();
    html! {
        @if !chosen.is_empty() {
            input type="text" id="path" name="path" value=(chosen) hx-swap-oob="true";
        }
    }
}

/// Fragment: the compose-file picker, swapped into `#db-file-browser`.
///
/// [`list`]'s card/list look, but rooted at the project path: the header shows
/// the current directory relative to that root, ".. Up" stops at the root, and
/// the only files offered are compose-looking YAML ones. Directories navigate;
/// files carry the "Use this file" action.
pub async fn files(Query(query): Query<RootedQuery>) -> Markup {
    let Some((root, current)) = resolve_within(query.root.as_deref(), query.path.as_deref()) else {
        return html! {
            div class="card browser" {
                p class="alert alert-error" {
                    "Pick a project path first — this browser stays inside it."
                }
            }
        };
    };
    if !root.exists() {
        return html! {
            div class="card browser" {
                p class="hint warn" {
                    "That path does not exist yet — set a repository URL and webhookr "
                    "will clone it there."
                }
            }
        };
    }

    let mut dirs: Vec<(String, String)> = Vec::new();
    let mut matches: Vec<(String, String)> = Vec::new();
    let mut error: Option<String> = None;

    match std::fs::read_dir(&current) {
        Ok(reader) => {
            for entry in reader.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                // Skip dotfiles: same noise rule as the directory picker.
                if name.starts_with('.') {
                    continue;
                }
                // Case-insensitive on purpose: compose files wander between
                // docker-compose.yml, Compose.yaml and every casing between.
                let lower = name.to_lowercase();
                let path = entry.path();
                if path.is_dir() {
                    dirs.push((name, path.to_string_lossy().into_owned()));
                } else if lower.contains("compose")
                    && (lower.ends_with(".yml") || lower.ends_with(".yaml"))
                {
                    matches.push((name, path.to_string_lossy().into_owned()));
                }
            }
            dirs.sort_by_key(|(name, _)| name.to_lowercase());
            matches.sort_by_key(|(name, _)| name.to_lowercase());
        }
        Err(err) => error = Some(format!("Cannot read this directory: {err}")),
    }

    let root_str = root.to_string_lossy().into_owned();
    // The header is the position within the project, not another absolute
    // path — the root is already on the page in the path field.
    let rel = current
        .strip_prefix(&root)
        .expect("resolve_within already checked the prefix")
        .to_string_lossy()
        .into_owned();
    let heading = if rel.is_empty() { ".".to_string() } else { rel };
    // ".. Up" only while there is somewhere inside the root to go up to.
    let up = if current != root {
        current
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|p| !p.is_empty())
    } else {
        None
    };

    html! {
        div class="card browser" {
            header class="browser-head" {
                strong class="mono" { (heading) }
            }
            div class="browser-shortcuts" {
                @if let Some(up) = &up {
                    button type="button" class="button small"
                            hx-get=(format!(
                                "/f/browse-files?root={}&path={}",
                                urlencode(&root_str),
                                urlencode(up)
                            ))
                            hx-target="#db-file-browser" hx-swap="innerHTML" { ".. Up" }
                }
                // No params: file_select answers empty, which is what closes
                // the picker — the same trick the directory picker's Close
                // button plays on /f/browse/select.
                button type="button" class="button small"
                        hx-get="/f/browse-file-select"
                        hx-target="#db-file-browser" hx-swap="innerHTML" { "Close" }
            }
            @if let Some(error) = &error {
                p class="alert alert-error" { (error) }
            } @else if dirs.is_empty() && matches.is_empty() {
                p class="muted" { "No subdirectories or compose files here." }
            } @else {
                ul class="browser-list" {
                    @for (name, full) in &dirs {
                        li {
                            button type="button" class="link"
                                    hx-get=(format!(
                                        "/f/browse-files?root={}&path={}",
                                        urlencode(&root_str),
                                        urlencode(full)
                                    ))
                                    hx-target="#db-file-browser" hx-swap="innerHTML" {
                                (name) "/"
                            }
                        }
                    }
                    @for (name, full) in &matches {
                        li class="browser-file" {
                            span class="mono" { (name) }
                            button type="button" class="button small"
                                    hx-get="/f/browse-file-select"
                                    hx-vals=(rooted_vals(&root_str, full))
                                    hx-target="#db-file-browser" hx-swap="innerHTML" {
                                "Use this file"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Fragment: close the compose picker and push the chosen file into the form.
///
/// Out-of-band swaps only (the CSP forbids inline script): the file's path
/// relative to the project root lands in `#db_compose_file`, and the picker
/// is replaced by an empty `#db-file-browser`. Anything the picker cannot
/// resolve — missing parameters, `..`, a path outside the root, a directory —
/// returns an empty response, which simply clears whatever the caller
/// targeted (how the picker's own Close button dismisses it).
pub async fn file_select(Query(query): Query<RootedQuery>) -> Markup {
    let Some((root, file)) = resolve_within(query.root.as_deref(), query.path.as_deref()) else {
        return html! {};
    };
    if !file.is_file() {
        return html! {};
    }
    let Ok(relative) = file.strip_prefix(&root) else {
        return html! {};
    };
    let relative = relative.to_string_lossy().into_owned();
    if relative.is_empty() {
        return html! {};
    }
    html! {
        // Same classes as the static field it replaces, so a pick does not
        // visibly restyle the input mid-form.
        input type="text" id="db_compose_file" name="db_compose_file" class="mono"
              value=(relative) hx-swap-oob="true";
        div id="db-file-browser" hx-swap-oob="true" {};
    }
}

/// Fragment: does this path look usable?
pub async fn check(Query(query): Query<PathQuery>) -> Markup {
    let raw = query.path.unwrap_or_default();
    let path = Path::new(raw.trim());

    if raw.trim().is_empty() {
        return html! { span class="hint" { "Enter a path to check it." } };
    }
    if !path.exists() {
        return html! {
            span class="hint warn" {
                "Does not exist yet — set a repository URL above and webhookr will clone it here."
            }
        };
    }
    if !path.is_dir() {
        return html! { span class="hint bad" { "That is a file, not a directory." } };
    }
    if path.join(".git").exists() {
        html! { span class="hint good" { "Exists and is a Git checkout." } }
    } else {
        html! {
            span class="hint warn" {
                "Directory exists but is not a Git checkout — deploys that pull will fail."
            }
        }
    }
}

/// Minimal percent-encoding for a path inside a query string.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// JSON object for htmx's `hx-vals`, so the path survives quoting intact.
fn select_vals(path: &str) -> String {
    serde_json::json!({ "path": path }).to_string()
}

/// JSON object for htmx's `hx-vals` carrying both rooted-picker parameters.
fn rooted_vals(root: &str, path: &str) -> String {
    serde_json::json!({ "root": root, "path": path }).to_string()
}
