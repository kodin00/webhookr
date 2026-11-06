//! HTTP server: receives webhooks and dispatches project runs.

use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::{Path, Query},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::json;

use crate::cloudflare;
use crate::config::{self, ProjectConfig};
use crate::executor;
use crate::github;
use crate::util;
use crate::web;

/// Per-invocation override for the admin UI, from `serve` flags.
///
/// Applied to the in-memory config only: `--web` starts the UI for this run
/// without persisting anything, so it can never leave the UI switched on by
/// accident.
#[derive(Debug, Default, Clone, Copy)]
pub struct WebOverride {
    pub enable: bool,
    pub disable: bool,
    pub port: Option<u16>,
}

impl WebOverride {
    fn apply(&self, web: &mut config::WebConfig) {
        if self.enable {
            web.enabled = true;
        }
        if self.disable {
            web.enabled = false;
        }
        if let Some(port) = self.port {
            web.listen_addr = replace_port(&web.listen_addr, port);
            web.enabled = !self.disable;
        }
    }
}

/// Run the webhook daemon in the foreground. `port` overrides the configured
/// listen address's port when provided.
pub async fn serve(port: Option<u16>, web_override: WebOverride) -> Result<()> {
    let mut cfg = config::load_config()?;
    if let Some(p) = port {
        cfg.listen_addr = replace_port(&cfg.listen_addr, p);
    }
    web_override.apply(&mut cfg.web);
    if cfg.web.enabled {
        cfg.web.validate(&cfg.listen_addr)?;
    }
    config::ensure_dirs()?;

    // Runs left `running` by a crash or restart would otherwise sit in-flight
    // forever; close them out before we start accepting new ones.
    if let Err(error) = crate::state::mark_interrupted_runs() {
        eprintln!("webhookr: could not tidy interrupted runs: {error:#}");
    }

    let _tunnel = match cloudflare::spawn_connector(&cfg) {
        Ok(child) => child,
        Err(error) => {
            eprintln!("webhookr: Cloudflare Tunnel is not running: {error:#}");
            None
        }
    };

    println!("webhookr listening on http://{}", cfg.listen_addr);
    for project in &cfg.projects {
        println!(
            "  POST /hooks/{}  ->  {}  (branch {})",
            project.id,
            project.name,
            project.branch_names().join(", ")
        );
    }
    if let Some(tunnel) = &cfg.cloudflare {
        println!(
            "  PUBLIC https://{}  ->  http://127.0.0.1:{}  ({})",
            tunnel.hostname,
            config::listen_port(&cfg.listen_addr),
            cloudflare::connector_label()
        );
    }
    if cfg.web.enabled {
        println!("webhookr admin UI on http://{}", cfg.web.listen_addr);
        if let Some(host) = cfg
            .cloudflare
            .as_ref()
            .and_then(|tunnel| tunnel.admin_hostname.as_deref())
        {
            println!(
                "  PUBLIC https://{}  ->  http://127.0.0.1:{}",
                host,
                config::listen_port(&cfg.web.listen_addr)
            );
        }
        println!("  !! the admin UI has no login; protect it with Cloudflare Access");
    }

    let app = webhook_router();

    // Bind both up front so a port clash fails at startup, where systemd will
    // report it, rather than leaving the daemon half-started.
    let hooks_listener = tokio::net::TcpListener::bind(&cfg.listen_addr)
        .await
        .with_context(|| format!("failed to bind webhook listener on {}", cfg.listen_addr))?;

    let hooks = axum::serve(hooks_listener, app);

    if !cfg.web.enabled {
        hooks.await.context("webhook listener stopped")?;
        return Ok(());
    }

    let state = web::AppState {
        require_access_header: cfg.web.require_access_header,
    };
    let web_addr = cfg.web.listen_addr.clone();

    // try_join! rather than spawn: if either listener dies the process exits
    // non-zero and systemd restarts it, instead of silently serving only half
    // of what it should. `_tunnel` stays owned by this frame throughout.
    tokio::try_join!(
        async { hooks.await.context("webhook listener stopped") },
        web::serve(&web_addr, state),
    )?;
    Ok(())
}

/// Routes that receive webhooks.
///
/// `/webhook/{id}` and `/hooks/{id}` are the same handler: `/hooks` is what
/// earlier versions published and is kept so already-configured senders keep
/// working, `/webhook` is the name used when the admin port serves everything
/// from a single hostname.
pub fn webhook_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/healthz", get(healthz))
        .route("/webhook/{id}", post(webhook))
        .route("/hooks/{id}", post(webhook))
}

/// Keep the host portion of `addr` but swap in `port` (split at the last `:`).
fn replace_port(addr: &str, port: u16) -> String {
    match addr.rfind(':') {
        Some(idx) => format!("{}:{}", &addr[..idx], port),
        None => format!("{}:{}", addr, port),
    }
}

/// Liveness/readiness probe plus a project count.
async fn healthz() -> Json<serde_json::Value> {
    let projects = config::load_config().map(|c| c.projects.len()).unwrap_or(0);
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "projects": projects,
    }))
}

/// Webhook entrypoint: verify the payload, then spawn a project run and reply.
async fn webhook(
    Path(id): Path<String>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let cfg = match config::load_config() {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "failed to load config" })),
            )
        }
    };

    let Some(project) = cfg.get(&id).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown project" })),
        );
    };

    eprintln!("webhookr: webhook received for {} ({id})", project.name);

    if !verify_secret(&project, &body, &headers) {
        eprintln!(
            "webhookr: signature verification failed for {} (mode {})",
            project.name, project.verify_mode
        );
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid signature" })),
        );
    }

    // Classified only now, never before `verify_secret`: it is the signature
    // that makes the repository and commit named in the body trustworthy enough
    // to post a status against. `classify` both narrows the delivery to a
    // [`github::TriggerKind`] the project's `trigger_events` filter matches
    // against, and yields the [`github::PushPayload`] the executor and reporter
    // consume — a *normalized* view for a merged-PR delivery, or whatever
    // `parse_push` made of anything else (empty for `ping`/non-push bodies).
    let event = headers
        .get("x-github-event")
        .and_then(|value| value.to_str().ok());
    let (kind, payload) = github::classify(event, &body);

    // One line per delivery saying what came in, parsed — never the raw body,
    // which could carry anything a sender chose to put in it. `branch` is where
    // the change landed (the base branch for a merge); `merge_source`, set only
    // on a merge, is the branch it came from.
    let payload_ref = payload.as_ref();
    let branch = payload_ref
        .and_then(|p| p.r#ref.as_deref())
        .map(|r| r.strip_prefix("refs/heads/").unwrap_or(r));
    eprintln!(
        "webhookr: {}: event '{}', kind {}, branch {}, merge from {}, commit {}, by {}, repo {}",
        project.name,
        event.unwrap_or("none"),
        kind.as_str(),
        branch.unwrap_or("-"),
        payload_ref
            .and_then(|p| p.merge_source.as_deref())
            .unwrap_or("-"),
        payload_ref.and_then(github::payload_sha).unwrap_or("-"),
        payload_ref.and_then(github::payload_actor).unwrap_or("-"),
        payload_ref.and_then(github::payload_slug).map_or_else(
            || "-".to_string(),
            |slug| format!("{}/{}", slug.owner, slug.repo)
        ),
    );

    // A `ping` never deploys; with a non-empty filter, only the selected kinds
    // (and a token-mode sender) deploy. Either way GitHub gets a 2xx and no run.
    if !kind.triggers_on(&project.trigger_events) {
        eprintln!(
            "webhookr: '{}' is not in trigger_events for {}; skipping",
            kind.as_str(),
            project.name
        );
        return (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "project": id,
                "started": false,
                "reason": format!("event '{}' is not in the project's triggers", kind.as_str()),
            })),
        );
    }

    // Resolve which of the project's branch targets this delivery is for: the
    // push ref (the base branch for a merge), or `?branch=` for a token-mode
    // sender. Everything below gates against the *resolved target*.
    let target = match resolve_target(&project, payload.as_ref(), query.get("branch")) {
        Ok(target) => target,
        Err(TargetError::UnknownQueryBranch) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "unknown branch", "project": id })),
            );
        }
        Err(TargetError::NotOnBranch) => {
            let reason = "delivery is not on one of the project's branches";
            eprintln!("webhookr: {reason} for {}; skipping", project.name);
            return (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "project": id,
                    "started": false,
                    "reason": reason,
                })),
            );
        }
    };

    // The branch gates. With a branch configured, only deliveries on that
    // branch start a run — a push must be to it, a merge must land on it —
    // and with `merge_sources` listed, a merge must additionally come from one
    // of them. A payload-less token-mode sender passes through; a GitHub
    // delivery whose body could not be read is refused rather than deployed
    // unverified. Skipped deliveries still get a 2xx: the delivery was fine,
    // it just was not a trigger.
    if let Some(reason) = github::trigger_allowed(
        &kind,
        payload.as_ref(),
        &target.branch,
        &target.merge_sources,
    ) {
        eprintln!("webhookr: {reason} for {}; skipping", project.name);
        return (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "project": id,
                "started": false,
                "branch": target.branch,
                "reason": reason,
            })),
        );
    }

    let trigger = executor::Trigger {
        // The kind, with the payload's actor, becomes the run's "triggered by".
        source: Some(github::trigger_label(&kind, payload.as_ref())),
        payload,
    };

    eprintln!(
        "webhookr: dispatching run for {} (branch {})",
        project.name,
        if target.branch.is_empty() {
            "-"
        } else {
            &target.branch
        }
    );
    let branch_label = target.branch.clone();
    std::mem::drop(tokio::spawn(async move {
        if let Err(error) = executor::run_project_with(&target, trigger).await {
            // Includes the single-flight rejection when a run is already going.
            eprintln!("webhookr: run for {} failed to start: {error:#}", target.id);
        }
    }));

    (
        StatusCode::ACCEPTED,
        Json(json!({ "ok": true, "project": id, "started": true, "branch": branch_label })),
    )
}

/// Why a delivery could not be matched to one of the project's branch targets.
#[derive(Debug, PartialEq, Eq)]
enum TargetError {
    /// The delivery's branch is not one the project deploys (2xx skip).
    NotOnBranch,
    /// A token-mode sender named a `?branch=` the project does not deploy (400).
    UnknownQueryBranch,
}

/// Pick the branch target a delivery is for.
///
/// * A GitHub payload with a `ref` (a push ref, or the base branch of a
///   merged PR — `classify` already normalizes it) resolves to the target for
///   that branch; no such target is [`TargetError::NotOnBranch`].
/// * A payload without a `ref` (ping, odd bodies) resolves to the primary
///   target, so `trigger_allowed` keeps producing its existing skip reasons.
/// * A payload-less token-mode sender may pass `?branch=`; an unknown name is
///   [`TargetError::UnknownQueryBranch`], none means the primary target.
/// * A project with no branch and no extra branches keeps its old behavior:
///   nothing gates, the project itself is the target.
fn resolve_target(
    project: &ProjectConfig,
    payload: Option<&github::PushPayload>,
    query_branch: Option<&String>,
) -> Result<ProjectConfig, TargetError> {
    let primary = || {
        project
            .target(&project.branch)
            .unwrap_or_else(|| project.clone())
    };
    if project.branch.trim().is_empty() && project.branches.is_empty() {
        return Ok(primary());
    }
    match payload {
        Some(payload) => match payload.r#ref.as_deref() {
            Some(reference) => project
                .target(github::branch_name(reference))
                .ok_or(TargetError::NotOnBranch),
            None => Ok(primary()),
        },
        None => match query_branch.map(|b| b.trim()).filter(|b| !b.is_empty()) {
            Some(branch) => project
                .target(branch)
                .ok_or(TargetError::UnknownQueryBranch),
            None => Ok(primary()),
        },
    }
}

/// Authenticate a webhook payload against the project's `verify_mode`.
fn verify_secret(p: &ProjectConfig, body: &[u8], headers: &HeaderMap) -> bool {
    match p.verify_mode.as_str() {
        "github" => {
            use hmac::{Hmac, Mac};
            use sha2::Sha256;
            type HmacSha256 = Hmac<Sha256>;

            let Some(sig) = headers
                .get("x-hub-signature-256")
                .and_then(|v| v.to_str().ok())
            else {
                return false;
            };
            let Some(hexsig) = sig.strip_prefix("sha256=") else {
                return false;
            };
            let Ok(expected) = hex::decode(hexsig) else {
                return false;
            };
            let Ok(mut mac) = HmacSha256::new_from_slice(p.secret.as_bytes()) else {
                return false;
            };
            mac.update(body);
            mac.verify_slice(&expected).is_ok()
        }
        "token" => match headers.get("x-webhookr-key") {
            Some(v) => util::constant_time_eq(v.as_bytes(), p.secret.as_bytes()),
            None => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(reference: Option<&str>) -> github::PushPayload {
        github::PushPayload {
            r#ref: reference.map(str::to_string),
            ..Default::default()
        }
    }

    fn bare_project() -> ProjectConfig {
        ProjectConfig::new(
            "app".into(),
            "App".into(),
            "/srv/app".into(),
            String::new(),
            "true".into(),
            "secret".into(),
            "token".into(),
        )
    }

    fn project() -> ProjectConfig {
        let mut p = bare_project();
        p.branch = "develop".into();
        let mut extra = config::BranchTarget::new("sandbox".into());
        extra.compose_file = "docker-compose.sandbox.yml".into();
        extra.merge_sources = vec!["feature".into()];
        p.branches.push(extra);
        p
    }

    #[test]
    fn pushes_resolve_to_their_branch_target() {
        let p = project();
        let t = resolve_target(&p, Some(&payload(Some("refs/heads/sandbox"))), None).unwrap();
        assert_eq!(t.branch, "sandbox");
        assert_eq!(t.compose_file, "docker-compose.sandbox.yml");
        assert_eq!(t.compose_project, "app-sandbox");
        let t = resolve_target(&p, Some(&payload(Some("refs/heads/develop"))), None).unwrap();
        assert_eq!(t.branch, "develop");
        assert_eq!(
            resolve_target(&p, Some(&payload(Some("refs/heads/other"))), None).unwrap_err(),
            TargetError::NotOnBranch
        );
        // Gating then applies the resolved target's merge_sources.
        let t = resolve_target(&p, Some(&payload(Some("refs/heads/sandbox"))), None).unwrap();
        assert_eq!(t.merge_sources, vec!["feature".to_string()]);
    }

    #[test]
    fn token_senders_use_the_query_branch_or_the_primary() {
        let p = project();
        assert_eq!(resolve_target(&p, None, None).unwrap().branch, "develop");
        let q = "sandbox".to_string();
        assert_eq!(
            resolve_target(&p, None, Some(&q)).unwrap().branch,
            "sandbox"
        );
        let bad = "nope".to_string();
        assert_eq!(
            resolve_target(&p, None, Some(&bad)).unwrap_err(),
            TargetError::UnknownQueryBranch
        );
    }

    #[test]
    fn a_project_without_branches_keeps_the_old_behavior() {
        let p = bare_project();
        let t = resolve_target(&p, Some(&payload(Some("refs/heads/anything"))), None).unwrap();
        assert!(t.branch.is_empty());
    }

    #[test]
    fn a_refless_payload_falls_back_to_the_primary() {
        let p = project();
        let t = resolve_target(&p, Some(&payload(None)), None).unwrap();
        assert_eq!(t.branch, "develop");
    }
}
