//! HTTP server: receives webhooks and dispatches project runs.

use anyhow::Result;
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

/// Run the webhook daemon in the foreground. `port` overrides the configured
/// listen address's port when provided.
pub async fn serve(port: Option<u16>) -> Result<()> {
    let mut cfg = config::load_config()?;
    if let Some(p) = port {
        cfg.listen_addr = replace_port(&cfg.listen_addr, p);
    }
    config::ensure_dirs()?;

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
            cfg.listen_addr
                .rsplit_once(':')
                .map(|(_, port)| port)
                .unwrap_or("9000"),
            cloudflare::connector_label()
        );
    }

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/hooks/{id}", post(webhook));

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await?;
    axum::serve(listener, app).await?;
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
        let _ = executor::run_project(&project).await;
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
