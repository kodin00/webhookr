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
