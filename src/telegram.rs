//! Telegram notifications for deploy runs.
//!
//! When a deploy starts, fails, or succeeds, webhookr posts a message to one
//! Telegram chat through one bot: `🚀` when the run begins, `✅` or `❌` when it
//! ends, quoting the tail of the run log on a failure.
//!
//! Everything here is best-effort, like the GitHub reporter it mirrors. A
//! message that cannot be sent is written into the run log and forgotten: a
//! deploy must never fail, or even be delayed, because Telegram was
//! unreachable or a token was wrong.

use std::fs::File;
use std::io::Write;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{self, AppConfig};
use crate::state::RunRecord;

const TELEGRAM_API: &str = "https://api.telegram.org";

/// Hard per-message limit Telegram enforces and rejects beyond.
const MAX_MESSAGE: usize = 4096;

/// Characters of run log quoted in a failure message, chosen so the header
/// and the admin link always fit alongside it.
const MAX_TAIL_CHARS: usize = 3000;

/// Longest snippet quoted back from an API error body.
const MAX_ERROR_SNIPPET: usize = 200;

/// Shared HTTP client.
///
/// `Option` because `build()` can fail on TLS initialisation, and a
/// notification is never worth a panic inside a deploy.
static CLIENT: LazyLock<Option<reqwest::Client>> = LazyLock::new(|| {
    reqwest::Client::builder()
        // Telegram does not require a User-Agent, but github.rs sends one and
        // consistency costs nothing.
        .user_agent(concat!("webhookr/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| eprintln!("webhookr: no HTTP client for Telegram: {error}"))
        .ok()
});

// ----- the request ---------------------------------------------------------

/// `chat_id` as Telegram accepts it: a JSON number when it is one (group and
/// supergroup ids are negative), a string for `@channelname`.
#[derive(Serialize)]
#[serde(untagged)]
enum ChatTarget<'a> {
    Id(i64),
    Name(&'a str),
}

fn chat_target(raw: &str) -> ChatTarget<'_> {
    let raw = raw.trim();
    match raw.parse::<i64>() {
        Ok(id) => ChatTarget::Id(id),
        Err(_) => ChatTarget::Name(raw),
    }
}

#[derive(Serialize)]
struct SendMessageBody<'a> {
    chat_id: ChatTarget<'a>,
    text: &'a str,
    // No `parse_mode`: plain text needs no escaping, so a commit message or a
    // log line full of `_` and `*` cannot scramble the message.
}

/// `POST {api_base}/bot{token}/sendMessage`.
///
/// `api_base` is a parameter rather than a constant so a test can point it at
/// a local listener. Deliberately *not* an environment variable, for the same
/// reason as `github::post_status`: a switch that redirects token-bearing
/// requests to an arbitrary host is a credential exfiltration primitive
/// nobody asked for.
async fn post_message(api_base: &str, token: &str, chat_id: &str, text: &str) -> Result<()> {
    let client = CLIENT.as_ref().context("no HTTP client available")?;
    let url = format!("{api_base}/bot{token}/sendMessage");

    let response = client
        .post(&url)
        .json(&SendMessageBody {
            chat_id: chat_target(chat_id),
            text,
        })
        .send()
        .await
        // reqwest's Display includes the request URL, and unlike GitHub's
        // header token this one rides in the URL path — so the token must be
        // scrubbed before the error is allowed anywhere near a log.
        .map_err(|error| {
            anyhow!(
                "request to Telegram failed: {}",
                redact(&error.to_string(), token)
            )
        })?;

    let status = response.status();
    if status.is_success() {
        return Ok(());
    }

    // These are the ones that waste an afternoon otherwise.
    let hint = match status.as_u16() {
        400 => " (the bot cannot post to that chat — has it been added to the group?)",
        401 | 404 => " (the token was rejected)",
        429 => " (rate limited)",
        _ => "",
    };
    let detail = response.text().await.unwrap_or_default();
    bail!("HTTP {status}{hint}: {}", api_message(&detail));
}

/// The `description` field of a Telegram error body, truncated. Falls back to
/// a snippet of whatever came back when it is not the expected shape.
fn api_message(body: &str) -> String {
    #[derive(Deserialize)]
    struct ApiError {
        description: String,
    }

    let text = match serde_json::from_str::<ApiError>(body) {
        Ok(error) => error.description,
        Err(_) => body.to_string(),
    };
    let text = text.trim();
    if text.chars().count() <= MAX_ERROR_SNIPPET {
        return text.to_string();
    }
    text.chars().take(MAX_ERROR_SNIPPET).chain(['…']).collect()
}

/// Replace every occurrence of the token with `***`. The token sits in the
/// request URL, and reqwest error messages carry the URL.
fn redact(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_string()
    } else {
        text.replace(token, "***")
    }
}

// ----- the notifier --------------------------------------------------------

/// Sends the Telegram messages for one run.
///
/// Every method returns `()`. A message that cannot be sent is noted in the
/// run log and dropped — nothing here may fail or delay a deploy.
pub struct Notifier {
    api_base: String,
    /// Never logged. See the hand-written [`std::fmt::Debug`] impl below.
    token: String,
    chat_id: String,
    /// Public admin base URL, when there is one worth handing out. Each
    /// message links to `/runs/{id}` under it.
    run_base: Option<String>,
}

impl std::fmt::Debug for Notifier {
    /// Hand-written so `{:?}` cannot carry the token into a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifier")
            .field("chat_id", &self.chat_id)
            .field("run_base", &self.run_base)
            .finish_non_exhaustive()
    }
}

impl Notifier {
    /// `None` when notifications are off, or on and unusable.
    ///
    /// The reason is printed once per run when notifications are switched on
    /// but unusable, since that is a configuration mistake worth surfacing.
    pub fn for_app(config: &AppConfig) -> Option<Self> {
        if !config.telegram.enabled {
            return None;
        }
        if let Some(problem) = config.telegram.problem() {
            eprintln!("webhookr: not sending Telegram notifications: {problem}");
            return None;
        }
        let mut notifier = Self::with_api_base(
            config.telegram.bot_token.trim(),
            config.telegram.chat_id.trim(),
            TELEGRAM_API.to_string(),
        );
        notifier.run_base = config::admin_base_url(config);
        Some(notifier)
    }

    /// A notifier aimed at `api_base`, so a test can use a local listener.
    pub(crate) fn with_api_base(token: &str, chat_id: &str, api_base: String) -> Self {
        Self {
            api_base,
            token: token.to_string(),
            chat_id: chat_id.to_string(),
            run_base: None,
        }
    }

    /// The run has begun: post the `🚀` message.
    pub async fn started(
        &self,
        project_name: &str,
        run_id: &str,
        commit: Option<&str>,
        log: &mut File,
    ) {
        let text = message_started(
            project_name,
            run_id,
            commit,
            self.run_url(run_id).as_deref(),
        );
        self.send("started", &text, false, log).await;
    }

    /// The run has ended: post the `✅` or `❌` message, quoting `log_tail` on
    /// a failure.
    pub async fn finished(
        &self,
        project_name: &str,
        record: &RunRecord,
        log_tail: &str,
        log: &mut File,
    ) {
        let text = message_finished(
            project_name,
            record,
            log_tail,
            self.run_url(&record.id).as_deref(),
        );
        let label = if record.status == "success" {
            "succeeded"
        } else {
            "failed"
        };
        self.send(label, &text, true, log).await;
    }

    fn run_url(&self, run_id: &str) -> Option<String> {
        self.run_base
            .as_ref()
            .map(|base| format!("{base}/runs/{run_id}"))
    }

    /// Send one message, swallowing every failure.
    async fn send(&self, label: &str, text: &str, retry: bool, log: &mut File) {
        let mut outcome = post_message(&self.api_base, &self.token, &self.chat_id, text).await;
        // One retry for a final message: a transient 5xx would otherwise leave
        // the chat silent about a finished deploy. `started` is not worth a
        // retry — the final message supersedes it within minutes anyway.
        if outcome.is_err() && retry {
            tokio::time::sleep(Duration::from_secs(1)).await;
            outcome = post_message(&self.api_base, &self.token, &self.chat_id, text).await;
        }

        let note = match &outcome {
            Ok(()) => format!("{label} message sent"),
            Err(error) => format!(
                "could not send the {label} message: {}",
                redact(&format!("{error:#}"), &self.token)
            ),
        };
        // The '#' is load-bearing, not decoration: `executor::summary_line`
        // skips lines starting with '#', so a note appended after the deploy
        // cannot become the run's history message in place of the real output.
        let _ = writeln!(log, "# telegram: {note}");
    }
}

// ----- the messages --------------------------------------------------------

/// First eight characters of an id, the short form runs are known by.
fn short(value: &str) -> &str {
    value.get(..8).unwrap_or(value)
}

pub fn message_started(
    project_name: &str,
    run_id: &str,
    commit: Option<&str>,
    run_url: Option<&str>,
) -> String {
    let commit = commit.map(short).unwrap_or("unknown");
    let mut text = format!(
        "🚀 deploy started — {project_name}\nrun {} · commit {commit}",
        short(run_id)
    );
    if let Some(url) = run_url {
        text.push_str("\n\n");
        text.push_str(url);
    }
    truncate_chars(&text, MAX_MESSAGE)
}

pub fn message_finished(
    project_name: &str,
    record: &RunRecord,
    log_tail: &str,
    run_url: Option<&str>,
) -> String {
    let duration = crate::executor::human_duration(record.duration_ms);
    let commit = record.commit.as_deref().map(short).unwrap_or("unknown");
    let succeeded = record.status == "success";
    let verb = if succeeded { "succeeded" } else { "failed" };
    let timing = if succeeded {
        format!("deployed in {duration}")
    } else {
        format!("failed after {duration}")
    };
    let mut text = format!(
        "{} deploy {verb} — {project_name}\nrun {} · commit {commit} · {timing}\n{}",
        if succeeded { "✅" } else { "❌" },
        short(&record.id),
        record.message
    );
    if !succeeded {
        text.push_str("\n\n--- last part of the run log ---\n");
        text.push_str(&truncate_chars(log_tail, MAX_TAIL_CHARS));
    }
    if let Some(url) = run_url {
        text.push_str("\n\n");
        text.push_str(url);
    }
    truncate_chars(&text, MAX_MESSAGE)
}

/// Truncate to at most `max` characters, marking a cut with `…`. Works on
/// chars, never bytes: Telegram counts characters, and splitting a multi-byte
/// one would panic.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars()
        .take(max.saturating_sub(1))
        .chain(['…'])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(status: &str, message: &str) -> RunRecord {
        RunRecord {
            id: "a1b2c3d4e5f6".into(),
            project_id: "site".into(),
            started_at: "2026-08-29T00:00:00Z".into(),
            finished_at: Some("2026-08-29T00:00:42Z".into()),
            status: status.into(),
            duration_ms: 42_000,
            message: message.into(),
            commit: Some("4f5e6d7c8b9a".repeat(4)),
        }
    }

    #[test]
    fn chat_ids_serialize_as_numbers_or_names() {
        let numeric = serde_json::to_string(&SendMessageBody {
            chat_id: chat_target(" -1001234567890 "),
            text: "hello group",
        })
        .unwrap();
        assert!(numeric.contains(r#""chat_id":-1001234567890"#), "{numeric}");
        assert!(numeric.contains(r#""text":"hello group""#), "{numeric}");
        assert!(!numeric.contains("parse_mode"), "{numeric}");

        let named = serde_json::to_string(&SendMessageBody {
            chat_id: chat_target("@mychannel"),
            text: "hello channel",
        })
        .unwrap();
        assert!(named.contains(r#""chat_id":"@mychannel""#), "{named}");

        // Not a number and not a name: still serialized, Telegram gets to say
        // so — config validation is advisory and must not panic here.
        let junk = serde_json::to_string(&SendMessageBody {
            chat_id: chat_target("abc"),
            text: "?",
        })
        .unwrap();
        assert!(junk.contains(r#""chat_id":"abc""#), "{junk}");
    }

    #[test]
    fn messages_name_the_run_project_and_commit() {
        let started = message_started(
            "My Site",
            "a1b2c3d4e5f6",
            Some("4f5e6d7c8b9a"),
            Some("https://panel.example.com/runs/a1b2c3d4e5f6"),
        );
        assert!(started.contains("🚀 deploy started — My Site"), "{started}");
        assert!(
            started.contains("run a1b2c3d4 · commit 4f5e6d7"),
            "{started}"
        );
        assert!(started.ends_with("https://panel.example.com/runs/a1b2c3d4e5f6"));

        // No commit to name yet (a manual run before HEAD is read).
        let blind = message_started("My Site", "a1b2c3d4e5f6", None, None);
        assert!(blind.contains("commit unknown"), "{blind}");

        let ok = message_finished(
            "My Site",
            &record("success", "Successfully tagged site:latest"),
            "leftover",
            None,
        );
        assert!(ok.contains("✅ deploy succeeded — My Site"), "{ok}");
        assert!(ok.contains("deployed in 42s"), "{ok}");
        assert!(ok.contains("Successfully tagged site:latest"), "{ok}");
        assert!(!ok.contains("run log"), "success quotes no log: {ok}");

        let failed = message_finished(
            "My Site",
            &record("failed", "error: pull failed: non-fast-forward"),
            "line one\nline two",
            Some("https://panel.example.com/runs/a1b2c3d4e5f6"),
        );
        assert!(failed.contains("❌ deploy failed — My Site"), "{failed}");
        assert!(failed.contains("failed after 42s"), "{failed}");
        assert!(
            failed.contains("error: pull failed: non-fast-forward"),
            "{failed}"
        );
        assert!(
            failed.contains("--- last part of the run log ---"),
            "{failed}"
        );
        assert!(failed.contains("line one\nline two"), "{failed}");
        assert!(failed.ends_with("https://panel.example.com/runs/a1b2c3d4e5f6"));
    }

    #[test]
    fn messages_fit_telegrams_limit_and_never_split_characters() {
        // A huge log tail is capped at MAX_TAIL_CHARS, long before the
        // whole-message clamp would matter.
        let huge_tail = message_finished(
            "My Site",
            &record("failed", "boom"),
            &"x".repeat(10_000),
            None,
        );
        assert!(huge_tail.chars().count() <= MAX_MESSAGE);
        assert!(huge_tail.contains('…'), "a cut must be marked: {huge_tail}");

        // A giant summary line alone can push past the limit; then the whole
        // message is clamped, with the ellipsis as the last character.
        let huge_message = message_finished(
            "My Site",
            &record("failed", &"y".repeat(5_000)),
            "tail",
            None,
        );
        assert!(huge_message.chars().count() <= MAX_MESSAGE);
        assert!(huge_message.ends_with('…'));

        // Multi-byte text must truncate on a character boundary.
        let wide = message_finished(
            "My Site",
            &record("failed", "boom"),
            &"é".repeat(4_000),
            None,
        );
        assert!(wide.chars().count() <= MAX_MESSAGE);
        assert!(wide.contains('…'));
    }

    #[test]
    fn the_debug_impl_never_carries_the_token() {
        let notifier = Notifier::with_api_base("123:SECRETVALUE", "-1001", "http://x".into());
        let rendered = format!("{notifier:?}");
        assert!(
            !rendered.contains("123:SECRETVALUE"),
            "token leaked: {rendered}"
        );
    }

    #[test]
    fn transport_errors_are_redacted() {
        assert_eq!(
            redact("GET http://x/bot/123:ABC/sendMessage failed", "123:ABC"),
            "GET http://x/bot/***/sendMessage failed"
        );
        assert_eq!(redact("no token in here", "123:ABC"), "no token in here");
        assert_eq!(redact("empty token", ""), "empty token");
    }

    /// Captured request, so the HTTP tests can assert on what actually went out.
    #[derive(Debug)]
    struct Seen {
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    /// Serve `status` for every sendMessage POST, recording each request.
    async fn fake_telegram(
        status: axum::http::StatusCode,
        response_body: &'static str,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<Seen>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{extract::State as AxumState, routing::post, Router};
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route(
                // The real API glues the literal `bot` onto the token with no
                // separator: `/bot123:ABC/sendMessage`.
                "/bot{token}/sendMessage",
                post(
                    move |AxumState(seen): AxumState<Arc<Mutex<Vec<Seen>>>>,
                          uri: axum::http::Uri,
                          headers: axum::http::HeaderMap,
                          body: String| async move {
                        seen.lock().unwrap().push(Seen {
                            path: uri.path().to_string(),
                            headers: headers
                                .iter()
                                .map(|(name, value)| {
                                    (
                                        name.as_str().to_string(),
                                        value.to_str().unwrap_or_default().to_string(),
                                    )
                                })
                                .collect(),
                            body,
                        });
                        (status, response_body)
                    },
                ),
            )
            .with_state(seen.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, seen, server)
    }

    #[tokio::test]
    async fn sends_the_request_the_sendmessage_api_expects() {
        let (base, seen, server) =
            fake_telegram(axum::http::StatusCode::OK, r#"{"ok":true}"#).await;

        let outcome = post_message(&base, "123:TESTTOKEN", "-1001234567890", "hello group").await;
        outcome.unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let request = &seen[0];
        assert_eq!(request.path, "/bot123:TESTTOKEN/sendMessage");

        let header = |name: &str| {
            request
                .headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .unwrap_or_default()
        };
        assert!(
            header("user-agent").starts_with("webhookr/"),
            "{:?}",
            header("user-agent")
        );
        // A negative id is a JSON number, not a string.
        assert!(
            request.body.contains(r#""chat_id":-1001234567890"#),
            "{}",
            request.body
        );
        assert!(request.body.contains("hello group"), "{}", request.body);
        // Assert the shape, never the value.
        assert!(!request.body.contains("parse_mode"), "{}", request.body);

        server.abort();
    }

    #[tokio::test]
    async fn a_rejected_message_explains_itself_without_the_token() {
        let cases = [
            (
                axum::http::StatusCode::BAD_REQUEST,
                r#"{"ok":false,"error_code":400,"description":"Bad Request: chat not found"}"#,
                "has it been added to the group",
            ),
            (
                axum::http::StatusCode::UNAUTHORIZED,
                r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#,
                "the token was rejected",
            ),
        ];
        for (status, body, hint) in cases {
            let (base, _seen, server) = fake_telegram(status, body).await;
            let error = post_message(&base, "123:TESTTOKEN", "-1001", "hello group")
                .await
                .expect_err("a non-2xx must be an error");
            let rendered = format!("{error:#}");
            assert!(
                rendered.contains("chat not found") || rendered.contains("Unauthorized"),
                "{rendered}"
            );
            assert!(
                rendered.contains(hint),
                "the hint is the whole point: {rendered}"
            );
            assert!(
                !rendered.contains("123:TESTTOKEN"),
                "token leaked: {rendered}"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn transport_errors_do_not_leak_the_token() {
        // Port 1 refuses connections immediately: a fast-offline stand-in.
        let error = post_message(
            "http://127.0.0.1:1",
            "123:TESTTOKEN",
            "-1001",
            "hello group",
        )
        .await
        .expect_err("a dead endpoint must be an error");
        let rendered = format!("{error:#}");
        assert!(
            !rendered.contains("123:TESTTOKEN"),
            "token leaked: {rendered}"
        );
    }
}
