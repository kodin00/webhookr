//! GitHub commit status reporting.
//!
//! After a deploy, webhookr posts a commit status so the pushed commit shows the
//! pending / green-check / red-X indicator on GitHub's repository page, with a
//! "Details" link back into the run log.
//!
//! Everything here is best-effort. A status that cannot be posted is written
//! into the run log and forgotten: a deploy must never fail, or even be delayed,
//! because GitHub was unreachable or a token was wrong.

use std::fs::File;
use std::io::Write;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{self, ProjectConfig};

/// Pinned, so a future default version cannot change what the API expects.
const API_VERSION: &str = "2022-11-28";

/// Rough cap for a status description. GitHub calls this field "a short
/// description" and truncates long ones in its UI.
const MAX_DESCRIPTION: usize = 140;

/// Longest snippet quoted back from an API error body.
const MAX_ERROR_SNIPPET: usize = 200;

/// Shared HTTP client.
///
/// `Option` because `build()` can fail on TLS initialisation, and a commit
/// status is never worth a panic inside a deploy.
static CLIENT: LazyLock<Option<reqwest::Client>> = LazyLock::new(|| {
    reqwest::Client::builder()
        // Not optional: GitHub answers 403 to a request with no User-Agent, and
        // reqwest sends none by default. (Cloudflare's API does not care, which
        // is why `cloudflare.rs` gets away without one.)
        .user_agent(concat!("webhookr/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| eprintln!("webhookr: no HTTP client for commit statuses: {error}"))
        .ok()
});

// ----- repository identity -------------------------------------------------

/// A repository on a GitHub-compatible host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSlug {
    /// Host as written in the remote, including a port when one is given.
    pub host: String,
    pub owner: String,
    pub repo: String,
}

/// Parse a git remote into host/owner/repo.
///
/// `None` for anything that is not a web-hosted git URL — a filesystem remote
/// such as `/srv/site.git` has nowhere to report to.
///
/// Any userinfo is discarded without being read: a remote may carry an access
/// token (`https://x-access-token:TOKEN@github.com/o/r.git`), and it must not
/// survive into a struct that ends up in a log line.
pub fn parse_repo(url: &str) -> Option<RepoSlug> {
    let raw = url.trim().trim_end_matches('/');
    if raw.is_empty() {
        return None;
    }

    let (host, path) = match raw.split_once("://") {
        // scheme://[userinfo@]host[:port]/owner/repo
        Some((_, rest)) => strip_userinfo(rest).split_once('/')?,
        // userinfo@host:owner/repo — the scp-like form git accepts. Without an
        // '@' this is a local path, and a local path cannot be reported to.
        None if raw.contains('@') => strip_userinfo(raw).split_once(':')?,
        None => return None,
    };

    if host.is_empty() || host.starts_with('-') {
        return None;
    }
    let mut segments = path.split('/');
    let owner = clean_segment(segments.next()?)?;
    let name = segments.next()?;
    let repo = clean_segment(name.strip_suffix(".git").unwrap_or(name))?;

    Some(RepoSlug {
        host: host.to_ascii_lowercase(),
        owner,
        repo,
    })
}

/// Drop everything up to and including the last `@` of the authority.
///
/// That is the userinfo, which may be a credential. It is never inspected and
/// never returned.
fn strip_userinfo(authority: &str) -> &str {
    let end = authority.find('/').unwrap_or(authority.len());
    match authority[..end].rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    }
}

fn clean_segment(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('-') || value.contains("..") {
        return None;
    }
    Some(value.to_string())
}

/// REST base URL for a host.
///
/// `github.com` has a dedicated API host; anything else is treated as GitHub
/// Enterprise Server, which serves the same API under `/api/v3`.
pub fn api_base(host: &str) -> String {
    if host.eq_ignore_ascii_case("github.com") {
        "https://api.github.com".to_string()
    } else {
        format!("https://{host}/api/v3")
    }
}

// ----- webhook payload -----------------------------------------------------

/// The parts of a GitHub `push` payload a status report needs.
///
/// Every field is optional so that any other event — `ping`, `workflow_run`, or
/// an entirely non-GitHub sender in `token` verify mode — deserializes into an
/// empty payload rather than failing.
#[derive(Debug, Default, Deserialize)]
pub struct PushPayload {
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub r#ref: Option<String>,
    #[serde(default)]
    pub deleted: Option<bool>,
    #[serde(default)]
    pub head_commit: Option<PushCommit>,
    #[serde(default)]
    pub repository: Option<PushRepo>,
}

#[derive(Debug, Deserialize)]
pub struct PushCommit {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct PushRepo {
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub clone_url: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
}

/// Read a webhook body as a push payload. Anything unparseable yields `None`,
/// which simply means this run reports against the checkout instead.
pub fn parse_push(body: &[u8]) -> Option<PushPayload> {
    serde_json::from_slice(body).ok()
}

/// The commit this push landed, or `None` when there is nothing to report on.
///
/// Both sources are needed. A push of more than a couple of thousand commits
/// arrives with `head_commit: null` but a valid `after`, while a branch deletion
/// has a null `head_commit` *and* an all-zero `after`.
pub fn payload_sha(payload: &PushPayload) -> Option<&str> {
    if payload.deleted == Some(true) {
        return None;
    }
    payload
        .head_commit
        .as_ref()
        .map(|commit| commit.id.as_str())
        .filter(|sha| is_sha(sha))
        .or_else(|| payload.after.as_deref().filter(|sha| is_sha(sha)))
}

/// A usable commit id: hex, a plausible length, and not the all-zero sha git
/// uses for "no such object".
fn is_sha(value: &str) -> bool {
    (7..=64).contains(&value.len())
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && value.bytes().any(|byte| byte != b'0')
}

/// Whether this push was to the branch the project actually deploys.
///
/// webhookr does not filter deliveries by branch, so a push to `dev` still
/// triggers a `main` deploy. Without this check we would post a status against a
/// commit that is not on the branch we checked out. An empty branch never
/// matches: it is permitted in config, but only for projects that never pull.
pub fn ref_matches(payload: &PushPayload, branch: &str) -> bool {
    let branch = branch.trim();
    !branch.is_empty()
        && payload
            .r#ref
            .as_deref()
            .and_then(|reference| reference.strip_prefix("refs/heads/"))
            == Some(branch)
}

/// The repository a push came from.
///
/// Preferred over the project's configured remote: the payload arrived under a
/// verified HMAC signature, and it names the repository GitHub will accept a
/// status for even when the local config points at a mirror.
pub fn payload_slug(payload: &PushPayload) -> Option<RepoSlug> {
    let repository = payload.repository.as_ref()?;
    let (owner, repo) = repository.full_name.as_deref()?.split_once('/')?;
    let host = repository
        .clone_url
        .as_deref()
        .or(repository.html_url.as_deref())
        .and_then(parse_repo)
        .map(|slug| slug.host)
        .unwrap_or_else(|| "github.com".to_string());

    Some(RepoSlug {
        host,
        owner: clean_segment(owner)?,
        repo: clean_segment(repo)?,
    })
}

// ----- the status itself ---------------------------------------------------

/// The four states GitHub accepts for a commit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Pending,
    Success,
    /// The deploy ran and failed.
    Failure,
    /// The deploy could not be run at all.
    Error,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Success => "success",
            State::Failure => "failure",
            State::Error => "error",
        }
    }
}

#[derive(Debug, Serialize)]
struct StatusBody<'a> {
    state: &'static str,
    /// Omitted rather than sent empty: GitHub renders it as the status's
    /// "Details" link, and a link to nowhere is worse than no link.
    #[serde(skip_serializing_if = "Option::is_none")]
    target_url: Option<&'a str>,
    description: &'a str,
    context: &'a str,
}

/// Shape arbitrary command output into a one-line status description.
///
/// The text comes from [`crate::executor`]'s summary line, which is raw `git`
/// and `docker` output — colour codes and all. `summary_line` does not strip
/// escape sequences (only the web view does), so without this the commit page
/// shows `[0m` litter.
pub fn describe(raw: &str) -> String {
    let cleaned = crate::util::strip_ansi(raw);
    let mut out = String::new();
    let mut width = 0usize;
    let mut gap = false;

    for ch in cleaned.chars() {
        if ch.is_whitespace() || ch.is_control() {
            gap = width > 0;
            continue;
        }
        let step = 1 + usize::from(gap);
        if width + step > MAX_DESCRIPTION {
            out.push('…');
            return out;
        }
        if gap {
            out.push(' ');
            gap = false;
        }
        out.push(ch);
        width += step;
    }
    out
}

/// `POST /repos/{owner}/{repo}/statuses/{sha}`.
///
/// `api_base` is a parameter rather than a constant so a test can point it at a
/// local listener. Deliberately *not* an environment variable: a switch that
/// redirects token-bearing requests to an arbitrary host is a credential
/// exfiltration primitive nobody asked for.
async fn post_status(
    api_base: &str,
    token: &str,
    slug: &RepoSlug,
    sha: &str,
    body: &StatusBody<'_>,
) -> Result<()> {
    let client = CLIENT.as_ref().context("no HTTP client available")?;
    let url = format!(
        "{api_base}/repos/{}/{}/statuses/{sha}",
        slug.owner, slug.repo
    );

    let response = client
        .post(&url)
        .bearer_auth(token)
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", API_VERSION)
        .json(body)
        .send()
        .await
        .context("request to the GitHub Statuses API failed")?;

    let status = response.status();
    if status.is_success() {
        return Ok(());
    }

    // These three are the ones that waste an afternoon otherwise.
    let hint = match status.as_u16() {
        401 => " (the token was rejected)",
        // GitHub answers 404, not 403, when a token cannot see the repository.
        404 => " (no such repository, or the token cannot write commit statuses)",
        422 => " (that commit is not in this repository)",
        _ => "",
    };
    let detail = response.text().await.unwrap_or_default();
    bail!("HTTP {status}{hint}: {}", api_message(&detail));
}

/// The `message` field of a GitHub error body, truncated. Falls back to a
/// snippet of whatever came back when it is not the expected shape.
fn api_message(body: &str) -> String {
    #[derive(Deserialize)]
    struct ApiError {
        message: String,
    }

    let text = match serde_json::from_str::<ApiError>(body) {
        Ok(error) => error.message,
        Err(_) => body.to_string(),
    };
    let text = text.trim();
    if text.chars().count() <= MAX_ERROR_SNIPPET {
        return text.to_string();
    }
    text.chars().take(MAX_ERROR_SNIPPET).chain(['…']).collect()
}

// ----- the reporter --------------------------------------------------------

/// Posts the commit statuses for one run.
///
/// Every method returns `()`. A status that cannot be posted is noted in the run
/// log and dropped — nothing here may fail or delay a deploy.
pub struct Reporter {
    api_base: String,
    /// Never logged. See the hand-written [`std::fmt::Debug`] impl below.
    token: String,
    slug: RepoSlug,
    context: String,
    target_url: Option<String>,
    branch: String,
    preset_label: &'static str,
    /// Every sha this run has announced, in post order. More than one when the
    /// pull moved HEAD past the commit we posted `pending` on; the final state
    /// goes to all of them, so no commit is left pending forever.
    reported: Vec<String>,
}

impl std::fmt::Debug for Reporter {
    /// Hand-written so `{:?}` cannot carry the token into a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reporter")
            .field("slug", &self.slug)
            .field("context", &self.context)
            .field("reported", &self.reported)
            .finish_non_exhaustive()
    }
}

impl Reporter {
    /// `None` when this project does not report commit statuses, or cannot.
    ///
    /// The reason is printed once per run when reporting is switched on but
    /// unusable, since that is a configuration mistake worth surfacing.
    pub fn for_project(project: &ProjectConfig, payload: Option<&PushPayload>) -> Option<Self> {
        if !project.status_reports {
            return None;
        }
        if let Some(problem) = project.status_report_problem() {
            eprintln!(
                "webhookr: not reporting commit status for {}: {problem}",
                project.id
            );
            return None;
        }
        let slug = payload
            .and_then(payload_slug)
            .or_else(|| parse_repo(&project.repository))?;
        let api_base = api_base(&slug.host);
        Some(Self::with_api_base(project, slug, api_base))
    }

    /// [`Self::for_project`] with the API base supplied, so a test can aim it at
    /// a local listener.
    pub(crate) fn with_api_base(
        project: &ProjectConfig,
        slug: RepoSlug,
        api_base: String,
    ) -> Self {
        Self {
            api_base,
            token: project.effective_status_token().to_string(),
            slug,
            context: project.effective_status_context(),
            target_url: None,
            branch: project.branch.clone(),
            preset_label: project.preset_label(),
            reported: Vec::new(),
        }
    }

    /// Attach the run's log page, once the run has an id.
    ///
    /// GitHub shows this as the status's "Details" link, so it is only worth
    /// sending when the admin UI is actually reachable from the internet.
    pub fn set_run_url(&mut self, run_id: &str) {
        let Ok(app) = config::load_config() else {
            return;
        };
        self.target_url =
            config::admin_base_url(&app).map(|base| format!("{base}/runs/{run_id}"));
    }

    /// Announce that a deploy of `sha` has started. A sha already announced is
    /// skipped, which is what makes calling this again after the pull cheap.
    pub async fn pending(&mut self, sha: &str, log: Option<&mut File>) {
        if self.reported.iter().any(|seen| seen == sha) {
            return;
        }
        self.reported.push(sha.to_string());
        let description = format!("deploying {} ({})", self.branch, self.preset_label);
        self.send(sha, State::Pending, &description, log).await;
    }

    /// Post the run's outcome to every commit it announced.
    pub async fn finish(&mut self, state: State, text: &str, log: &mut File) {
        let mut description = describe(text);
        if description.is_empty() {
            description = state.as_str().to_string();
        }
        for sha in self.reported.clone() {
            self.send(&sha, state, &description, Some(log)).await;
        }
    }

    /// Report that a trigger was dropped because a deploy was already running.
    ///
    /// Terminal on purpose: a `pending` here would never resolve. This runs
    /// before the run has an id or a log file, so like every other pre-run
    /// diagnostic it goes to stderr.
    pub async fn refused(&mut self, sha: &str) {
        self.reported.push(sha.to_string());
        self.send(
            sha,
            State::Error,
            "another deploy is in progress; it may already include this commit",
            None,
        )
        .await;
    }

    /// Post one status, swallowing every failure.
    async fn send(&self, sha: &str, state: State, description: &str, log: Option<&mut File>) {
        let body = StatusBody {
            state: state.as_str(),
            target_url: self.target_url.as_deref(),
            description,
            context: &self.context,
        };

        let mut outcome = post_status(&self.api_base, &self.token, &self.slug, sha, &body).await;
        // One retry for a terminal state: a transient 5xx would otherwise leave
        // the commit showing `pending` until somebody pushes again. `pending`
        // itself is not worth retrying — the final state supersedes it anyway.
        if outcome.is_err() && state != State::Pending {
            tokio::time::sleep(Duration::from_secs(1)).await;
            outcome = post_status(&self.api_base, &self.token, &self.slug, sha, &body).await;
        }

        let note = match &outcome {
            Ok(()) => format!("{} {} {}", state.as_str(), short(sha), self.context),
            Err(error) => format!(
                "could not post {} for {}: {error:#}",
                state.as_str(),
                short(sha)
            ),
        };
        match log {
            // The '#' is load-bearing, not decoration: `executor::summary_line`
            // skips lines starting with '#', so a status note appended after the
            // deploy cannot become the run's history message in place of the
            // real output.
            Some(log) => {
                let _ = writeln!(log, "# github status: {note}");
            }
            None => eprintln!("webhookr: github status: {note}"),
        }
    }
}

/// First seven characters of a sha, as git and GitHub display it.
fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slug(host: &str, owner: &str, repo: &str) -> Option<RepoSlug> {
        Some(RepoSlug {
            host: host.into(),
            owner: owner.into(),
            repo: repo.into(),
        })
    }

    #[test]
    fn parses_every_remote_shape_git_accepts() {
        assert_eq!(
            parse_repo("https://github.com/me/site.git"),
            slug("github.com", "me", "site")
        );
        assert_eq!(
            parse_repo("https://github.com/me/site"),
            slug("github.com", "me", "site")
        );
        assert_eq!(
            parse_repo("https://github.com/me/site/"),
            slug("github.com", "me", "site")
        );
        assert_eq!(
            parse_repo("git@github.com:me/site.git"),
            slug("github.com", "me", "site")
        );
        assert_eq!(
            parse_repo("ssh://git@github.com/me/site.git"),
            slug("github.com", "me", "site")
        );
        // Extra path segments (a browser URL pasted into the field) are ignored.
        assert_eq!(
            parse_repo("https://github.com/me/site/tree/main"),
            slug("github.com", "me", "site")
        );
        // A host is case-insensitive; owner and repo are not.
        assert_eq!(
            parse_repo("https://GitHub.com/Me/Site.git"),
            slug("github.com", "Me", "Site")
        );
        // Enterprise, with the port kept so `api_base` can rebuild the URL.
        assert_eq!(
            parse_repo("https://ghe.example.com:8443/me/site.git"),
            slug("ghe.example.com:8443", "me", "site")
        );
    }

    #[test]
    fn a_token_in_the_remote_is_discarded_not_carried() {
        let parsed = parse_repo("https://x-access-token:ghp_SECRETVALUE@github.com/me/site.git");
        assert_eq!(parsed, slug("github.com", "me", "site"));

        let rendered = format!("{parsed:?}");
        assert!(!rendered.contains("ghp_SECRETVALUE"), "token leaked: {rendered}");
        assert!(!rendered.contains('@'), "userinfo leaked: {rendered}");

        assert_eq!(
            parse_repo("https://ghp_SECRETVALUE@github.com/me/site"),
            slug("github.com", "me", "site")
        );
    }

    #[test]
    fn remotes_with_nowhere_to_report_to_are_rejected() {
        // The executor's own test suite clones from a local bare repo; it must
        // never try to post a status for one.
        assert_eq!(parse_repo("/srv/remote.git"), None);
        assert_eq!(parse_repo("../sibling.git"), None);
        assert_eq!(parse_repo(""), None);
        assert_eq!(parse_repo("   "), None);
        assert_eq!(parse_repo("https://github.com/me"), None);
        assert_eq!(parse_repo("https://github.com/"), None);
    }

    #[test]
    fn api_base_splits_dotcom_from_enterprise() {
        assert_eq!(api_base("github.com"), "https://api.github.com");
        assert_eq!(api_base("GitHub.com"), "https://api.github.com");
        assert_eq!(
            api_base("ghe.example.com"),
            "https://ghe.example.com/api/v3"
        );
        assert_eq!(
            api_base("ghe.example.com:8443"),
            "https://ghe.example.com:8443/api/v3"
        );
    }

    #[test]
    fn finds_the_pushed_commit() {
        let push = parse_push(
            br#"{"ref":"refs/heads/main","after":"1111111111111111111111111111111111111111",
                 "head_commit":{"id":"2222222222222222222222222222222222222222"}}"#,
        )
        .unwrap();
        // `head_commit` is the tip of what was pushed; prefer it.
        assert_eq!(
            payload_sha(&push),
            Some("2222222222222222222222222222222222222222")
        );

        // A push of thousands of commits omits `head_commit` but still has
        // `after`.
        let huge = parse_push(
            br#"{"ref":"refs/heads/main","after":"3333333333333333333333333333333333333333",
                 "head_commit":null}"#,
        )
        .unwrap();
        assert_eq!(
            payload_sha(&huge),
            Some("3333333333333333333333333333333333333333")
        );
    }

    #[test]
    fn nothing_is_reported_for_a_deleted_branch_or_a_non_push() {
        // A branch deletion: zero `after`, no head commit, `deleted: true`.
        let deleted = parse_push(
            br#"{"ref":"refs/heads/gone","deleted":true,
                 "after":"0000000000000000000000000000000000000000","head_commit":null}"#,
        )
        .unwrap();
        assert_eq!(payload_sha(&deleted), None);

        // The zero sha is rejected on its own merits too.
        let zeroes = parse_push(
            br#"{"after":"0000000000000000000000000000000000000000"}"#,
        )
        .unwrap();
        assert_eq!(payload_sha(&zeroes), None);

        // A `ping` parses into an empty payload rather than failing.
        let ping = parse_push(br#"{"zen":"Keep it logically awesome.","hook_id":1}"#).unwrap();
        assert_eq!(payload_sha(&ping), None);
        assert!(payload_slug(&ping).is_none());

        // Anything unparseable is simply not a push.
        assert!(parse_push(b"not json at all").is_none());
        assert!(parse_push(b"").is_none());
    }

    #[test]
    fn only_a_push_to_the_deployed_branch_names_its_commit() {
        let on_main = parse_push(br#"{"ref":"refs/heads/main"}"#).unwrap();
        assert!(ref_matches(&on_main, "main"));
        assert!(!ref_matches(&on_main, "dev"));
        // An empty branch is valid config for a project that never pulls, but it
        // can never match a ref.
        assert!(!ref_matches(&on_main, ""));

        let tag = parse_push(br#"{"ref":"refs/tags/v1.0.0"}"#).unwrap();
        assert!(!ref_matches(&tag, "v1.0.0"));

        let nothing = PushPayload::default();
        assert!(!ref_matches(&nothing, "main"));
    }

    #[test]
    fn the_payload_names_the_repository_it_came_from() {
        let push = parse_push(
            br#"{"repository":{"full_name":"me/site",
                 "clone_url":"https://github.com/me/site.git"}}"#,
        )
        .unwrap();
        assert_eq!(payload_slug(&push), slug("github.com", "me", "site"));

        // An Enterprise payload carries its own host.
        let ghe = parse_push(
            br#"{"repository":{"full_name":"me/site",
                 "html_url":"https://ghe.example.com/me/site"}}"#,
        )
        .unwrap();
        assert_eq!(payload_slug(&ghe), slug("ghe.example.com", "me", "site"));

        // With no URL to read a host from, assume the public host.
        let bare = parse_push(br#"{"repository":{"full_name":"me/site"}}"#).unwrap();
        assert_eq!(payload_slug(&bare), slug("github.com", "me", "site"));
    }

    #[test]
    fn descriptions_are_one_short_clean_line() {
        assert_eq!(describe("  deployed  fine  "), "deployed fine");
        assert_eq!(describe("first\nsecond\r\nthird"), "first second third");

        // Docker and git colour their output; none of it belongs on a commit.
        let coloured = "\x1b[31mError\x1b[0m response from daemon";
        assert_eq!(describe(coloured), "Error response from daemon");

        let long = "x".repeat(500);
        let shaped = describe(&long);
        assert_eq!(shaped.chars().count(), MAX_DESCRIPTION + 1, "{shaped}");
        assert!(shaped.ends_with('…'));

        // Truncating multi-byte text must not panic or split a character.
        let wide = "é".repeat(500);
        assert!(describe(&wide).ends_with('…'));

        assert_eq!(describe(""), "");
        assert_eq!(describe("   \n  "), "");
    }

    #[test]
    fn api_errors_quote_only_the_message() {
        assert_eq!(api_message(r#"{"message":"Not Found"}"#), "Not Found");
        // A non-JSON body (an HTML error page from a proxy) still says something.
        assert_eq!(api_message("  gateway timeout  "), "gateway timeout");

        let long = format!(r#"{{"message":"{}"}}"#, "z".repeat(400));
        let quoted = api_message(&long);
        assert_eq!(quoted.chars().count(), MAX_ERROR_SNIPPET + 1);
    }

    /// Captured request, so the HTTP tests can assert on what actually went out.
    struct Seen {
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    /// Serve `status` for every status POST, recording each request.
    async fn fake_github(
        status: axum::http::StatusCode,
        response_body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Seen>>>, tokio::task::JoinHandle<()>) {
        use axum::{extract::State as AxumState, routing::post, Router};
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route(
                "/repos/{owner}/{repo}/statuses/{sha}",
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

    fn test_slug() -> RepoSlug {
        RepoSlug {
            host: "github.com".into(),
            owner: "me".into(),
            repo: "site".into(),
        }
    }

    #[tokio::test]
    async fn sends_the_request_the_statuses_api_expects() {
        let (base, seen, server) = fake_github(axum::http::StatusCode::CREATED, "{}").await;

        let body = StatusBody {
            state: "success",
            target_url: None,
            description: "deployed in 4s",
            context: "webhookr/site",
        };
        post_status(&base, "ghp_TESTTOKEN", &test_slug(), "abc1234def", &body)
            .await
            .unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let request = &seen[0];
        assert_eq!(request.path, "/repos/me/site/statuses/abc1234def");

        let header = |name: &str| {
            request
                .headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .unwrap_or_default()
        };
        // Without a User-Agent GitHub answers 403 for every request.
        assert!(header("user-agent").starts_with("webhookr/"), "{:?}", header("user-agent"));
        assert_eq!(header("accept"), "application/vnd.github+json");
        assert_eq!(header("x-github-api-version"), API_VERSION);
        // Assert the shape, never the value.
        assert!(header("authorization").starts_with("Bearer "));

        assert!(request.body.contains(r#""state":"success""#), "{}", request.body);
        assert!(request.body.contains("webhookr/site"), "{}", request.body);
        // An absent Details link is omitted, not sent as null.
        assert!(
            !request.body.contains("target_url"),
            "target_url should be omitted: {}",
            request.body
        );

        server.abort();
    }

    #[tokio::test]
    async fn a_rejected_status_explains_itself_without_the_token() {
        let (base, _seen, server) = fake_github(
            axum::http::StatusCode::NOT_FOUND,
            r#"{"message":"Not Found","documentation_url":"https://docs.github.com/"}"#,
        )
        .await;

        let body = StatusBody {
            state: "failure",
            target_url: Some("https://deploy.example.com/runs/abc"),
            description: "it broke",
            context: "webhookr/site",
        };
        let error = post_status(&base, "ghp_TESTTOKEN", &test_slug(), "abc1234def", &body)
            .await
            .expect_err("a 404 must be an error");

        // A Details link, when there is one, is sent.
        {
            let seen = _seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert!(
                seen[0]
                    .body
                    .contains(r#""target_url":"https://deploy.example.com/runs/abc""#),
                "{}",
                seen[0].body
            );
        }

        let rendered = format!("{error:#}");
        assert!(rendered.contains("404"), "{rendered}");
        assert!(rendered.contains("Not Found"), "{rendered}");
        assert!(
            rendered.contains("cannot write commit statuses"),
            "the 404 hint is the whole point: {rendered}"
        );
        assert!(!rendered.contains("ghp_TESTTOKEN"), "token leaked: {rendered}");

        server.abort();
    }
}
