//! Shared page chrome and the components used by more than one page.
//!
//! Everything here goes through maud, which escapes interpolated values by
//! default. That matters more than it might look: deploy logs contain whatever
//! `git` and `docker` printed, including any filename in the repository, so raw
//! interpolation would be a stored-XSS hole on a page that displays webhook
//! secrets. `PreEscaped` must not appear anywhere in this module tree.

use maud::{html, Markup, DOCTYPE};

use crate::state::RunRecord;

/// Wrap page content in the site shell.
pub fn page(title: &str, body: Markup) -> Markup {
    let version = env!("CARGO_PKG_VERSION");
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "webhookr — " (title) }
                // htmx otherwise injects a <style> block for its request
                // indicators, which `style-src 'self'` blocks. Turning it off
                // keeps the CSP strict instead of relaxing it to
                // 'unsafe-inline'; the indicator styling lives in app.css.
                meta name="htmx-config" content=r#"{"includeIndicatorStyles":false}"#;
                link rel="stylesheet" href={ "/static/app.css?v=" (version) };
                script src={ "/static/htmx.min.js?v=" (version) } defer {}
            }
            body {
                header class="topbar" {
                    a class="brand" href="/" { "webhookr" }
                    nav {
                        a href="/" { "Dashboard" }
                        a href="/projects" { "Projects" }
                        a href="/runs" { "Runs" }
                        a href="/settings" { "Settings" }
                    }
                }
                main { (body) }
                footer class="footer" {
                    span { "webhookr " (version) }
                    span class="warn-inline" {
                        "No login — protect this page with Cloudflare Access."
                    }
                }
            }
        }
    }
}

/// A one-off notice at the top of a page.
pub fn alert(kind: &str, message: &str) -> Markup {
    html! { p class={ "alert alert-" (kind) } { (message) } }
}

/// Status pill for a run, with elapsed time while it is still going.
pub fn status_badge(record: Option<&RunRecord>) -> Markup {
    let Some(record) = record else {
        return html! { span class="badge badge-none" { "never run" } };
    };

    let (class, text) = match record.status.as_str() {
        "success" => ("ok", "success".to_string()),
        "failed" => ("fail", "failed".to_string()),
        "interrupted" => ("warn", "interrupted".to_string()),
        "running" => {
            let mins = minutes_since(&record.started_at);
            // No deploy timeout exists yet, so a very old `running` record is
            // more likely a wedged command than genuine progress. Say so rather
            // than pretending it is fine.
            match mins {
                Some(m) if m >= 60 => ("warn", format!("running? ({}h)", m / 60)),
                Some(m) if m >= 1 => ("run", format!("running ({m}m)")),
                _ => ("run", "running".to_string()),
            }
        }
        other => ("none", other.to_string()),
    };
    html! { span class={ "badge badge-" (class) } { (text) } }
}

/// Whole-minutes elapsed since an RFC3339 timestamp.
fn minutes_since(started_at: &str) -> Option<i64> {
    let started = chrono::DateTime::parse_from_rfc3339(started_at).ok()?;
    let elapsed = chrono::Utc::now().signed_duration_since(started.to_utc());
    Some(elapsed.num_minutes().max(0))
}

/// A stored UTC timestamp rendered as Asia/Jakarta wall clock.
///
/// History keeps UTC — sortable, unambiguous — but every human-facing page
/// shows WIB. Jakarta has never observed DST, so a fixed +07:00 offset is
/// correct indefinitely and spares the binary a tz-database dependency.
/// A malformed timestamp (hand-edited history file) passes through untouched
/// rather than blanking the cell.
pub fn jakarta_time(iso: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(dt) => {
            let wib = chrono::FixedOffset::east_opt(7 * 3600).expect("7h is a valid offset");
            dt.with_timezone(&wib).format("%Y-%m-%d %H:%M:%S WIB").to_string()
        }
        Err(_) => iso.to_string(),
    }
}

/// Human-readable run duration.
pub fn duration(record: &RunRecord) -> String {
    if record.status == "running" {
        return "—".to_string();
    }
    let ms = record.duration_ms;
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Labelled read-only value.
pub fn field(label: &str, value: &str) -> Markup {
    html! {
        div class="field" {
            span class="field-label" { (label) }
            span class="field-value" { (value) }
        }
    }
}

/// Copy-friendly monospace value.
pub fn code_field(label: &str, value: &str) -> Markup {
    html! {
        div class="field" {
            span class="field-label" { (label) }
            code class="field-value mono" { (value) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::jakarta_time;

    #[test]
    fn utc_timestamps_render_as_wib() {
        assert_eq!(
            jakarta_time("2026-09-01T00:00:42Z"),
            "2026-09-01 07:00:42 WIB"
        );
    }

    #[test]
    fn conversion_crosses_day_and_year_boundaries() {
        // 17:30 UTC on New Year's Eve is already 1 January in Jakarta.
        assert_eq!(
            jakarta_time("2026-12-31T17:30:00Z"),
            "2027-01-01 00:30:00 WIB"
        );
    }

    #[test]
    fn unparseable_timestamps_pass_through() {
        assert_eq!(jakarta_time("whenever"), "whenever");
        assert_eq!(jakarta_time(""), "");
    }
}
