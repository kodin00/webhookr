//! HTTP server: receives webhooks and dispatches project runs.

use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::Path,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::json;

use crate::cloudflare;
use crate::config::{self, ProjectConfig};
use crate::executor;
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
            project.id, project.name, project.branch
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

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/hooks/{id}", post(webhook));

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

    if !verify_secret(&project, &body, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid signature" })),
        );
    }

    std::mem::drop(tokio::spawn(async move {
        if let Err(error) = executor::run_project(&project).await {
            // Includes the single-flight rejection when a run is already going.
            eprintln!("webhookr: run for {} failed to start: {error:#}", project.id);
        }
    }));

    (
        StatusCode::ACCEPTED,
        Json(json!({ "ok": true, "project": id, "started": true })),
    )
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
