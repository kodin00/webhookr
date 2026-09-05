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
use crate::update;
use crate::web::views;
use crate::web::WebError;

pub async fn index() -> Result<Markup, WebError> {
    let cfg = config::load_config()?;
    Ok(views::page(
        "Settings",
        settings_body(&cfg, None, None, None, None),
    ))
}

#[derive(Deserialize)]
pub struct SettingsForm {
    pub listen_addr: String,
    pub web_listen_addr: String,
    #[serde(default)]
    pub require_access_header: Option<String>,
    #[serde(default)]
    pub telegram_enabled: Option<String>,
    #[serde(default)]
    pub telegram_chat_id: String,
    #[serde(default)]
    pub telegram_bot_token: String,
    #[serde(default)]
    pub telegram_clear_bot_token: Option<String>,
}

/// Apply the Telegram fields of a submitted settings form.
///
/// The bot token follows the house secret rule: the form never round-trips it
/// through the browser, so a blank field means "keep what is stored" and a
/// separate checkbox is the only way to remove it.
fn apply_telegram(form: &SettingsForm, telegram: &mut config::TelegramConfig) {
    telegram.enabled = form.telegram_enabled.is_some();
    telegram.chat_id = form.telegram_chat_id.trim().to_string();
    let submitted = form.telegram_bot_token.trim();
    if form.telegram_clear_bot_token.is_some() {
        telegram.bot_token.clear();
    } else if !submitted.is_empty() {
        telegram.bot_token = submitted.to_string();
    }
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

        let mut telegram = cfg.telegram.clone();
        apply_telegram(&form, &mut telegram);
        // Settings-save validation only: this never runs in the deploy path,
        // so a broken block can bounce the form but never a deploy.
        if let Some(problem) = telegram.problem() {
            anyhow::bail!("{problem}");
        }

        cfg.listen_addr = listen_addr.clone();
        cfg.web = web;
        cfg.telegram = telegram;
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
                    settings_body(&cfg, None, Some(&format!("{error:#}")), None, None),
                ),
            )
                .into_response())
        }
    }
}

fn settings_body(
    cfg: &config::AppConfig,
    notice: Option<&str>,
    error: Option<&str>,
    checked: Option<&Result<update::Build, String>>,
    tested: Option<&Result<String, String>>,
) -> Markup {
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
            fieldset {
                legend { "Telegram notifications" }
                label class="checkbox" {
                    input type="checkbox" name="telegram_enabled" value="1"
                          checked[cfg.telegram.enabled];
                    span {
                        strong { "Notify a Telegram chat about deploys" }
                        span class="hint" {
                            "One bot and one chat for every project. Messages are posted \
                             when a run starts and when it ends; a failed run quotes the \
                             tail of its log."
                        }
                    }
                }
                label {
                    span { "Chat id" }
                    input type="text" name="telegram_chat_id" value=(cfg.telegram.chat_id)
                          placeholder="-1001234567890";
                    span class="hint" {
                        "Group chats have negative ids. Add the bot to the group, send any \
                         message, then read the id from \
                         api.telegram.org/bot&lt;token&gt;/getUpdates. @channelname also works."
                    }
                }
                label {
                    span { "Bot token" }
                    input type="password" name="telegram_bot_token" autocomplete="off"
                          placeholder=(if cfg.telegram.bot_token.is_empty() {
                              "123456789:AAF…"
                          } else {
                              "stored — leave blank to keep it"
                          });
                    span class="hint" { "Create a bot with @BotFather on Telegram." }
                }
                @if !cfg.telegram.bot_token.is_empty() {
                    label class="checkbox" {
                        input type="checkbox" name="telegram_clear_bot_token" value="1";
                        span { "Remove the stored bot token" }
                    }
                }
                @if let Some(problem) = cfg.telegram.problem() {
                    span class="warn-inline" { (problem) }
                }
                p {
                    // `form=` points at a form outside this one: a nested
                    // `<form>` is dropped by the HTML parser, which would
                    // silently retarget this button at "save settings".
                    button type="submit" class="button" form="telegram-test" {
                        "Send test message"
                    }
                    span class="hint" {
                        "Uses the saved settings, so save first — the test \
                         cannot see anything typed but unsaved above it."
                    }
                }
                @match tested {
                    Some(Ok(msg)) => { (views::alert("ok", msg)) }
                    Some(Err(err)) => { (views::alert("error", err)) }
                    None => {}
                }
            }
            div class="form-actions" {
                button type="submit" class="button primary" { "Save settings" }
            }
        }
        // The test button's target. Sibling of the settings form, like the
        // version-check form below, because nested forms are dropped by the
        // HTML parser — see the button inside the Telegram fieldset.
        form id="telegram-test" method="post" action="/settings/telegram-test" {}

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

        (version_card(checked))

        section class="card" {
            h2 { "Paths" }
            (views::code_field("Config", &config::config_path().display().to_string()))
            (views::code_field("State", &config::state_dir().display().to_string()))
            (views::code_field("Logs", &config::log_dir().display().to_string()))
        }
    }
}

// ----- version & updates -------------------------------------------------

/// Running build, published build, and the button between them.
///
/// `checked` is the result of an on-demand comparison — the page does not check
/// on every load, because that would put a network call in front of a settings
/// page that has to work when GitHub does not.
fn version_card(checked: Option<&Result<update::Build, String>>) -> Markup {
    let current = update::Build::current();
    html! {
        section id="version" class="card" {
            h2 { "Version" }
            (views::code_field("Running", &current.label()))

            @match checked {
                None => {
                    p class="hint" {
                        "A release is a single rolling tag, so the commit in brackets is what \
                         actually identifies a build. Check it against what is published to \
                         see whether this server is up to date."
                    }
                }
                Some(Ok(latest)) => {
                    (views::code_field("Published", &latest.label()))
                    @if *latest == current {
                        p class="field-value" { "Up to date." }
                    } @else {
                        p class="warn-inline" { "An update is available." }
                        form method="post" action="/settings/update" {
                            button type="submit" class="button primary"
                                   hx-confirm="Replace the binary and restart the daemon?" {
                                "Update and restart"
                            }
                            span class="hint" {
                                "Downloads the published build, verifies its checksum, replaces \
                                 this binary and exits so the service manager starts it again. \
                                 If you are running webhookr in a terminal rather than under \
                                 systemd, it will not come back by itself."
                            }
                        }
                    }
                }
                Some(Err(error)) => (views::alert("error", error)),
            }

            p {
                button type="submit" class="button" form="version-check" { "Check for updates" }
            }
            form id="version-check" method="post" action="/settings/check-update" {}
        }
    }
}

pub async fn check_update() -> Result<Markup, WebError> {
    let cfg = config::load_config()?;
    let checked = update::latest().await.map_err(|error| format!("{error:#}"));
    Ok(views::page(
        "Settings",
        settings_body(&cfg, None, None, Some(&checked), None),
    ))
}

/// Fire one real message through the saved bot and chat, so a configuration
/// can be proven before a deploy depends on it.
///
/// POST like every state-changing route, though it changes no state: the CSRF
/// guard exempts GET, and a prefetched link must not post to a chat.
pub async fn telegram_test() -> Result<Markup, WebError> {
    let cfg = config::load_config()?;
    // Validation inside `send_test` requires a chat id, so the success path
    // always has one to name.
    let tested = crate::telegram::send_test(crate::telegram::TELEGRAM_API, &cfg.telegram)
        .await
        .map(|()| {
            format!(
                "Test message sent to chat {} — check Telegram.",
                cfg.telegram.chat_id.trim()
            )
        })
        .map_err(|error| format!("{error:#}"));
    Ok(views::page(
        "Settings",
        settings_body(&cfg, None, None, None, Some(&tested)),
    ))
}

pub async fn self_update() -> Result<Response, WebError> {
    let cfg = config::load_config()?;
    match update::install().await {
        Ok(update::Outcome::UpToDate(build)) => {
            let notice = format!("Already running the published build: {}", build.label());
            Ok(
                views::page(
                    "Settings",
                    settings_body(&cfg, Some(&notice), None, None, None),
                )
                .into_response(),
            )
        }
        Ok(update::Outcome::Replaced { from, to, path }) => {
            println!(
                "webhookr: replaced {} ({} -> {}); exiting so the service restarts",
                path.display(),
                from.label(),
                to.label()
            );
            // Respond first, then go. The new binary only starts running when
            // this process ends, and the operator should see why it happened.
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                std::process::exit(update::RESTART_EXIT_CODE);
            });
            let notice = format!(
                "Updated to {}. Restarting — reload this page in a few seconds.",
                to.label()
            );
            Ok(
                views::page(
                    "Settings",
                    settings_body(&cfg, Some(&notice), None, None, None),
                )
                .into_response(),
            )
        }
        Err(error) => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            views::page(
                "Settings",
                settings_body(&cfg, None, Some(&format!("{error:#}")), None, None),
            ),
        )
            .into_response()),
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
    let hostname_opt = (!hostname.is_empty()).then(|| hostname.clone());
    let admin_opt = (!admin.is_empty()).then(|| admin.clone());

    if token.is_empty() {
        return Ok(render_cloudflare_error(&cfg, "An API token is required."));
    }
    if hostname_opt.is_none() && admin_opt.is_none() {
        return Ok(render_cloudflare_error(
            &cfg,
            "Enter at least one hostname. Using only the admin hostname serves \
             the dashboard and webhooks from that single host.",
        ));
    }

    // `cloudflare::provision` uses the blocking reqwest client and makes
    // several 30s-timeout calls. Running it inline would pin a runtime worker
    // for minutes, stalling the webhook listener sharing this process.
    let probe = cfg.clone();
    let provisioned = tokio::task::spawn_blocking(move || {
        cloudflare::provision(&token, hostname_opt.as_deref(), admin_opt.as_deref(), &probe)
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

#[cfg(test)]
mod tests {
    use super::{apply_telegram, SettingsForm};
    use crate::config;

    fn form(enabled: bool, chat_id: &str, bot_token: &str, clear: bool) -> SettingsForm {
        SettingsForm {
            listen_addr: "127.0.0.1:9000".into(),
            web_listen_addr: "127.0.0.1:9001".into(),
            require_access_header: enabled.then(|| "1".into()),
            telegram_enabled: enabled.then(|| "1".into()),
            telegram_chat_id: chat_id.into(),
            telegram_bot_token: bot_token.into(),
            telegram_clear_bot_token: clear.then(|| "1".into()),
        }
    }

    #[test]
    fn a_stored_bot_token_survives_a_save_unless_cleared() {
        let stored = config::TelegramConfig {
            enabled: true,
            bot_token: "111:OLD".into(),
            chat_id: "-100123".into(),
        };

        // Blank means "keep what is stored": the secret never round-trips
        // through the browser.
        let mut kept = stored.clone();
        apply_telegram(&form(true, " -100123 ", "", false), &mut kept);
        assert_eq!(kept.bot_token, "111:OLD");
        assert_eq!(kept.chat_id, "-100123", "chat id is trimmed on save");

        // A filled field replaces it; a checked box removes it entirely.
        let mut replaced = stored.clone();
        apply_telegram(&form(true, "-100123", " 222:NEW ", false), &mut replaced);
        assert_eq!(replaced.bot_token, "222:NEW");

        let mut cleared = stored.clone();
        apply_telegram(&form(false, "-100123", "", true), &mut cleared);
        assert!(cleared.bot_token.is_empty());

        // Unticking the switch keeps the token and the chat, so re-enabling
        // is one click rather than a trip back to @BotFather.
        let mut disabled = stored.clone();
        apply_telegram(&form(false, "-100123", "", false), &mut disabled);
        assert!(!disabled.enabled);
        assert_eq!(disabled.bot_token, "111:OLD");
        assert_eq!(disabled.chat_id, "-100123");
    }
}
