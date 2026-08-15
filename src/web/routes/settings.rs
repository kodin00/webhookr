//! Server settings and Cloudflare Tunnel provisioning.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Form,
};
use maud::{html, Markup};
use serde::Deserialize;

use crate::cloudflare;
use crate::config;
use crate::web::views;
use crate::web::WebError;

pub async fn index() -> Result<Markup, WebError> {
    let cfg = config::load_config()?;
    Ok(views::page("Settings", settings_body(&cfg, None, None)))
}

#[derive(Deserialize)]
pub struct SettingsForm {
    pub listen_addr: String,
    pub web_listen_addr: String,
    #[serde(default)]
    pub require_access_header: Option<String>,
}

pub async fn save(Form(form): Form<SettingsForm>) -> Result<Response, WebError> {
    let listen_addr = form.listen_addr.trim().to_string();
    let web_listen_addr = form.web_listen_addr.trim().to_string();
    let require_access_header = form.require_access_header.is_some();

    let outcome = config::update_config(|cfg| {
        if listen_addr.is_empty() || listen_addr.rsplit_once(':').is_none() {
            anyhow::bail!("webhook listen address must be host:port");
        }
        let mut web = cfg.web.clone();
        web.listen_addr = web_listen_addr.clone();
        web.require_access_header = require_access_header;
        web.validate(&listen_addr)?;

        cfg.listen_addr = listen_addr.clone();
        cfg.web = web;
        Ok(())
    });

    match outcome {
        Ok(()) => Ok(Redirect::to("/settings?saved=1").into_response()),
        Err(error) => {
            let cfg = config::load_config()?;
            Ok((
                StatusCode::UNPROCESSABLE_ENTITY,
                views::page(
                    "Settings",
                    settings_body(&cfg, None, Some(&format!("{error:#}"))),
                ),
            )
                .into_response())
        }
    }
}

fn settings_body(cfg: &config::AppConfig, notice: Option<&str>, error: Option<&str>) -> Markup {
    html! {
        section class="page-head" { h1 { "Settings" } }
        @if let Some(notice) = notice { (views::alert("ok", notice)) }
        @if let Some(error) = error { (views::alert("error", error)) }

        form method="post" action="/settings" class="card form" {
            fieldset {
                legend { "Listeners" }
                label {
                    span { "Webhook listen address" }
                    input type="text" name="listen_addr" value=(cfg.listen_addr) required;
                    span class="hint" { "Where GitHub delivers webhooks. Restart required after a change." }
                }
                label {
                    span { "Admin UI listen address" }
                    input type="text" name="web_listen_addr" value=(cfg.web.listen_addr) required;
                    span class="hint" {
                        "Keep this on 127.0.0.1 unless you know otherwise: cloudflared connects \
                         from this same host, so loopback still works through the tunnel while \
                         keeping the port off the public interface."
                    }
                }
                label class="checkbox" {
                    input type="checkbox" name="require_access_header" value="1"
                          checked[cfg.web.require_access_header];
                    span {
                        strong { "Require a Cloudflare Access header" }
                        span class="hint" {
                            "Rejects requests without Cf-Access-Jwt-Assertion. A presence check \
                             only — it does not validate the token — so it is a safety net, not \
                             a replacement for an Access policy."
                        }
                    }
                }
            }
            div class="form-actions" {
                button type="submit" class="button primary" { "Save settings" }
            }
        }

        section class="card" {
            h2 { "Cloudflare Tunnel" }
            @match &cfg.cloudflare {
                Some(tunnel) => {
                    (views::code_field("Webhook hostname", &tunnel.hostname))
                    @match &tunnel.admin_hostname {
                        Some(host) => (views::code_field("Admin hostname", host)),
                        None => p class="muted" { "No admin hostname routed yet." }
                    }
                    (views::code_field("Tunnel", &tunnel.tunnel_name))
                }
                None => p class="muted" { "Not configured." }
            }
            p { a class="button" href="/settings/cloudflare" { "Configure tunnel" } }
        }

        section class="card" {
            h2 { "Paths" }
            (views::code_field("Config", &config::config_path().display().to_string()))
            (views::code_field("State", &config::state_dir().display().to_string()))
            (views::code_field("Logs", &config::log_dir().display().to_string()))
            (views::field("Version", env!("CARGO_PKG_VERSION")))
        }
    }
}

// ----- Cloudflare --------------------------------------------------------

#[derive(Deserialize)]
pub struct CloudflareForm {
    pub api_token: String,
    pub hostname: String,
    #[serde(default)]
    pub admin_hostname: String,
}

pub async fn cloudflare_form() -> Result<Markup, WebError> {
    let cfg = config::load_config()?;
    Ok(views::page("Cloudflare Tunnel", cloudflare_body(&cfg, None)))
}

pub async fn cloudflare_save(Form(form): Form<CloudflareForm>) -> Result<Response, WebError> {
    let cfg = config::load_config()?;
    let token = form.api_token.trim().to_string();
    let hostname = form.hostname.trim().to_string();
    let admin = form.admin_hostname.trim().to_string();
    let admin_opt = (!admin.is_empty()).then(|| admin.clone());

    if token.is_empty() {
        return Ok(render_cloudflare_error(&cfg, "An API token is required."));
    }

    // `cloudflare::provision` uses the blocking reqwest client and makes
    // several 30s-timeout calls. Running it inline would pin a runtime worker
    // for minutes, stalling the webhook listener sharing this process.
    let probe = cfg.clone();
    let provisioned = tokio::task::spawn_blocking(move || {
        cloudflare::provision(&token, &hostname, admin_opt.as_deref(), &probe)
    })
    .await
    .map_err(|error| WebError::new(StatusCode::INTERNAL_SERVER_ERROR, error.into()))?;

    match provisioned {
        Ok(provisioned) => {
            // Re-read under the lock so we don't clobber a concurrent edit.
            config::update_config(|cfg| {
                cfg.web.hostname = provisioned.config.admin_hostname.clone();
                cloudflare::apply(provisioned, cfg)
            })?;
            Ok(Redirect::to("/settings").into_response())
        }
        Err(error) => Ok(render_cloudflare_error(&cfg, &format!("{error:#}"))),
    }
}

fn render_cloudflare_error(cfg: &config::AppConfig, message: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        views::page("Cloudflare Tunnel", cloudflare_body(cfg, Some(message))),
    )
        .into_response()
}

fn cloudflare_body(cfg: &config::AppConfig, error: Option<&str>) -> Markup {
    let hostname = cfg
        .cloudflare
        .as_ref()
        .map(|c| c.hostname.clone())
        .unwrap_or_default();
    let admin_hostname = cfg
        .cloudflare
        .as_ref()
        .and_then(|c| c.admin_hostname.clone())
        .or_else(|| cfg.web.hostname.clone())
        .unwrap_or_default();

    html! {
        section class="page-head" { h1 { "Cloudflare Tunnel" } }
        @if let Some(error) = error { (views::alert("error", error)) }

        section class="card" {
            p {
                "Publishes this server at real HTTPS hostnames without opening a port. \
                 The API token is used once and never stored — only the narrower runtime \
                 tunnel token is saved."
            }
            p class="hint" {
                "The token needs Zone Read, DNS Write, and Cloudflare Tunnel Write for the \
                 target account and zone."
            }
        }

        form method="post" action="/settings/cloudflare" class="card form" {
            fieldset {
                legend { "Hostnames" }
                label {
                    span { "Webhook hostname" }
                    input type="text" name="hostname" value=(hostname) required
                          placeholder="hooks.example.com";
                    span class="hint" { "Where GitHub sends webhooks." }
                }
                label {
                    span { "Admin hostname" span class="optional" { "optional" } }
                    input type="text" name="admin_hostname" value=(admin_hostname)
                          placeholder="deploy.example.com";
                    span class="hint" {
                        "Separate hostname for this UI. It must be separate: putting Access on \
                         the webhook hostname would break GitHub, which cannot log in."
                    }
                }
                label {
                    span { "API token" }
                    input type="password" name="api_token" required autocomplete="off"
                          placeholder="scoped Cloudflare API token";
                }
            }
            div class="form-actions" {
                button type="submit" class="button primary" { "Provision tunnel" }
                a class="button" href="/settings" { "Cancel" }
            }
        }

        section class="card warn-card" {
            h2 { "Before you expose the admin hostname" }
            p {
                "This UI has no login of its own. Anyone who can reach it can set a project's \
                 deploy command and run it. Add a Cloudflare Access policy on the admin \
                 hostname before using it."
            }
        }
    }
}
