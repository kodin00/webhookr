//! Synchronizes project sources and runs deployment presets.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;
use tokio::process::Command;

use crate::config::ProjectConfig;
use crate::github;
use crate::state::RunRecord;

/// Rough cap for the summary line derived from command output.
const MAX_MESSAGE_CHARS: usize = 200;

/// One lock per project id, so concurrent triggers for the *same* project
/// serialize while different projects still deploy in parallel.
///
/// The outer `std::sync::Mutex` only guards map lookup and is never held across
/// an `.await`; the inner lock is a `tokio::sync::Mutex` because it is held for
/// the entire run.
static PROJECT_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn project_lock(project_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    PROJECT_LOCKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(project_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// What kicked a run off.
///
/// A webhook delivery carries the push payload — already HMAC-verified by
/// [`crate::server`] — which is where the commit to report a status against
/// comes from. The CLI, TUI and admin UI use [`Default`] and fall back to
/// whatever the checkout has at HEAD.
#[derive(Debug, Default)]
pub struct Trigger {
    pub payload: Option<github::PushPayload>,
}

/// Clone or fast-forward the source, then deploy it.
pub async fn run_project(p: &ProjectConfig) -> Result<RunRecord> {
    run(p, true, Trigger::default()).await
}

/// [`run_project`] for a webhook delivery, carrying the verified push payload.
pub async fn run_project_with(p: &ProjectConfig, trigger: Trigger) -> Result<RunRecord> {
    run(p, true, trigger).await
}

/// Re-run the configured deployment without touching the Git checkout.
/// Used only as an explicit escape hatch (`run --no-pull`, TUI "Run deployment").
pub async fn deploy_project(p: &ProjectConfig) -> Result<RunRecord> {
    run(p, false, Trigger::default()).await
}

async fn run(p: &ProjectConfig, sync_source: bool, trigger: Trigger) -> Result<RunRecord> {
    p.validate()?;
    crate::config::ensure_dirs()?;

    // Built before the lock: the refusal path below reports too, and it has
    // neither a run id nor a log file to hang the report on.
    let mut reporter = github::Reporter::for_project(p, trigger.payload.as_ref());

    // The pushed sha is only usable when the push was to the branch this
    // project deploys. webhookr does not filter deliveries by branch, so a push
    // to `dev` still deploys `main` — reporting against the `dev` commit would
    // be a lie. When the ref does not match we fall back to post-sync HEAD.
    let pushed_sha = trigger
        .payload
        .as_ref()
        .filter(|payload| github::ref_matches(payload, &p.branch))
        .and_then(github::payload_sha)
        .map(str::to_string);

    // Single-flight per project: a webhook and a manual trigger must not race
    // `git pull` and `docker compose up` on the same checkout. `try_lock`
    // rather than `lock().await`, so a run that hangs (there is no timeout yet)
    // cannot wedge the project behind an invisible queue.
    let lock = project_lock(&p.id);
    let Ok(_guard) = lock.try_lock() else {
        // Terminal, not `pending`: nothing would ever resolve a pending here.
        // Silence would read on GitHub as "the webhook is not wired up", hiding
        // a push that really was received and not deployed. In the common case
        // the in-flight run's own `git pull` picks this commit up anyway and its
        // final status — same sha, same context — supersedes this one.
        if let (Some(reporter), Some(sha)) = (reporter.as_mut(), pushed_sha.as_deref()) {
            reporter.refused(sha).await;
        }
        bail!("a run for {} is already in progress", p.id);
    };

    let id = crate::util::new_run_id();
    let started_at = crate::util::now_iso();
    let started = Instant::now();

    // Open (or create) the per-run log and write the header line.
    let log_path = crate::state::run_log_path(&id);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log dir {}", parent.display()))?;
    }
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open log {}", log_path.display()))?;
    let action = if sync_source { "update" } else { "deploy" };
    writeln!(
        log,
        "# webhookr {action} {id} for {} started {started_at}",
        p.name
    )?;

    // Publish a `running` record up front so the daemon, CLI and web UI can all
    // see an in-flight deploy. `finalize` replaces it by id when the run ends.
    // Best-effort: failing to write history must not abort the deployment.
    if let Err(error) = crate::state::append_run(RunRecord {
        id: id.clone(),
        project_id: p.id.clone(),
        started_at: started_at.clone(),
        finished_at: None,
        status: "running".to_string(),
        duration_ms: 0,
        message: format!("{action} in progress"),
        // Best guess while the run is in flight: the pushed sha when the
        // webhook carried one, otherwise unknown until the deploy finishes.
        commit: pushed_sha.clone(),
    }) {
        eprintln!("webhookr: could not record run start: {error:#}");
    }

    // The sha to announce against at the start: the pushed one when we have
    // it, otherwise the checkout as it stands. A manual run carries no payload,
    // so it announces against the current HEAD, which is also what lets a
    // redeploy clear a stale red X on the commit that is actually live.
    let start_sha = match pushed_sha.clone() {
        Some(sha) => Some(sha),
        None => head_sha(&p.path).await,
    };
    if let Some(reporter) = reporter.as_mut() {
        reporter.set_run_url(&id);
        if let Some(sha) = &start_sha {
            reporter.pending(sha, Some(&mut log)).await;
        }
    }

    // Telegram notifications are app-wide rather than per project, so the
    // config is read here instead of being threaded through every caller. A
    // load failure just means no notifications: it must never fail the deploy.
    let telegram = crate::config::load_config()
        .ok()
        .and_then(|app| crate::telegram::Notifier::for_app(&app));
    if let Some(telegram) = telegram.as_ref() {
        telegram
            .started(&p.name, &id, start_sha.as_deref(), &mut log)
            .await;
    }

    // The commit this run ends up deploying, kept for the run history. A
    // synced run re-reads HEAD after the pull, which may have moved past the
    // sha announced above; a no-sync run deploys exactly that HEAD.
    let mut commit = if sync_source { pushed_sha.clone() } else { start_sha };

    let (status, state, message) = if sync_source {
        match sync_project(p, &mut log, &log_path).await {
            // The source could never be fetched, so the deployment did not run
            // at all. That is what GitHub's `error` means, as against a deploy
            // that ran and failed.
            Err(message) => ("failed", github::State::Error, message),
            Ok(()) => {
                // The pull may have moved HEAD past what we announced: a newer
                // push, or a run that had already fetched. The commit actually
                // deploying is the post-pull HEAD — record it for the history,
                // and announce it so both shas get the final state and neither
                // is left pending forever.
                if let Some(head) = head_sha(&p.path).await {
                    commit = Some(head.clone());
                    if let Some(reporter) = reporter.as_mut() {
                        reporter.pending(&head, Some(&mut log)).await;
                    }
                }
                deploy_phase(p, &mut log, &log_path).await
            }
        }
    } else {
        deploy_phase(p, &mut log, &log_path).await
    };

    // History before GitHub: the admin UI polls `finished_at` to stop tailing
    // the log, so a slow API call must not hold a finished run visibly open.
    let record = finalize(
        &mut log,
        &id,
        &p.id,
        &started_at,
        started,
        status,
        message.clone(),
        commit,
    )?;
    if let Some(reporter) = reporter.as_mut() {
        let text = if state == github::State::Success {
            format!("deployed in {}", human_duration(record.duration_ms))
        } else {
            message
        };
        reporter.finish(state, &text, &mut log).await;
    }
    if let Some(telegram) = telegram.as_ref() {
        // `read_tail` rather than `state::read_run_log_tail`: the state helper
        // prefixes a truncation marker aimed at the polling web view, and the
        // message adds its own. Strip ANSI before the tail is cut so colour
        // codes cannot eat the character budget.
        let tail = crate::util::strip_ansi(&read_tail(&log_path, 16 * 1024));
        telegram.finished(&p.name, &record, &tail, &mut log).await;
    }
    Ok(record)
}

/// Run the configured deployment and classify the result.
///
/// Extracted so both the "source synced" and "no sync requested" paths reach it
/// without duplicating the error handling. Infallible: a log write that fails is
/// not a reason to lose the run's outcome.
async fn deploy_phase(
    p: &ProjectConfig,
    log: &mut std::fs::File,
    log_path: &Path,
) -> (&'static str, github::State, String) {
    let ok = match deploy(p, log).await {
        Ok(ok) => ok,
        Err(error) => {
            let _ = writeln!(log, "--- deployment error ---\n{error:#}");
            false
        }
    };
    let message = summary_line(log_path).unwrap_or_else(|| "no output".to_string());
    if ok {
        ("success", github::State::Success, message)
    } else {
        ("failed", github::State::Failure, message)
    }
}

/// The commit currently checked out, or `None` if there isn't one.
///
/// A plain [`Command`] rather than [`git_command`]: `rev-parse` reads only local
/// refs, so it is never handed the project's token. And `.output()` rather than
/// [`exec`], because this is the one git call whose stdout we need to read
/// instead of stream into the run log.
async fn head_sha(path: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

async fn sync_project(
    p: &ProjectConfig,
    log: &mut std::fs::File,
    log_path: &Path,
) -> std::result::Result<(), String> {
    let path = Path::new(&p.path);
    if !path.exists() {
        if p.repository.trim().is_empty() {
            return Err(format!("project path does not exist: {}", p.path));
        }
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                return Err(format!("could not create {}: {error}", parent.display()));
            }
        }
        // Clone runs from the parent: the target directory does not exist yet.
        let cwd = path
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_string());
        let mut command = git_command(p);
        command.args([
            "clone",
            "--branch",
            p.branch.as_str(),
            "--single-branch",
            p.repository.as_str(),
            p.path.as_str(),
        ]);
        let ok = exec(command, &cwd, "git clone", log)
            .await
            .map_err(|error| format!("failed to run git clone: {error:#}"))?;
        return ok
            .then_some(())
            .ok_or_else(|| last_error("git clone failed", log_path));
    }

    if !path.join(".git").exists() {
        return Err(format!("project path is not a Git checkout: {}", p.path));
    }

    let git_steps: [(&str, Vec<&str>); 3] = [
        ("fetch", vec!["fetch", "origin", p.branch.as_str()]),
        ("checkout", vec!["checkout", p.branch.as_str()]),
        (
            "pull",
            vec!["pull", "--ff-only", "origin", p.branch.as_str()],
        ),
    ];
    for (step, args) in git_steps {
        let label = format!("git {step}");
        let mut command = git_command(p);
        command.args(&args);
        let ok = exec(command, &p.path, &label, log)
            .await
            .map_err(|error| format!("failed to run {label}: {error:#}"))?;
        if !ok {
            return Err(last_error(&format!("{label} failed"), log_path));
        }
    }
    Ok(())
}

async fn deploy(p: &ProjectConfig, log: &mut std::fs::File) -> Result<bool> {
    if !Path::new(&p.path).exists() {
        bail!("project path does not exist: {}", p.path);
    }
    if p.uses_compose() {
        let compose_path = Path::new(&p.path).join(&p.compose_file);
        if !compose_path.is_file() {
            bail!("compose file does not exist: {}", compose_path.display());
        }
        let mut base = vec!["compose", "-f", p.compose_file.as_str()];
        for profile in &p.compose_profiles {
            base.extend(["--profile", profile.as_str()]);
        }
        if p.deploy_preset == "compose_pull" {
            let mut pull = base.clone();
            pull.push("pull");
            if !run_command("docker", &pull, &p.path, "docker compose pull", log).await? {
                return Ok(false);
            }
        }
        let mut up = base;
        up.extend(["up", "-d"]);
        if p.deploy_preset == "compose_build" {
            up.push("--build");
        }
        up.push("--remove-orphans");
        run_command("docker", &up, &p.path, "docker compose up", log).await
    } else {
        run_command(
            "sh",
            &["-c", p.command.as_str()],
            &p.path,
            "custom command",
            log,
        )
        .await
    }
}

/// A `git` command carrying the project's credentials, if it has any.
///
/// The token is handed over through a credential helper that reads an
/// environment variable. That keeps it out of the process list (where a URL
/// like `https://token@github.com/...` would expose it to every user on the
/// box) and out of the checkout's `.git/config`, so it is not persisted to disk
/// by git and does not survive in a repository someone later copies.
fn git_command(project: &ProjectConfig) -> Command {
    let mut command = Command::new("git");
    let token = project.git_token.trim();
    if !token.is_empty() {
        command
            // Clear inherited helpers first, so a system-wide helper cannot
            // answer before ours and silently use the wrong account.
            .arg("-c")
            .arg("credential.helper=")
            .arg("-c")
            .arg(
                "credential.helper=!f() { echo username=x-access-token; \
                 echo \"password=$WEBHOOKR_GIT_TOKEN\"; }; f",
            )
            .env("WEBHOOKR_GIT_TOKEN", token);
    }
    command
}

/// Run one command, streaming its output straight into the run log.
///
/// The child inherits duplicated descriptors of the log file, which was opened
/// with `O_APPEND`. Every write from the child therefore appends atomically and
/// lands in the file *as it is produced*, which is what makes live tailing work
/// — `.output()` would buffer a ten-minute `docker compose up --build` until it
/// exited. Sharing one append-mode description also means stdout and stderr
/// interleave chronologically without a drain task or a pipe-buffer deadlock.
async fn run_command(
    program: &str,
    args: &[&str],
    cwd: &str,
    label: &str,
    log: &mut std::fs::File,
) -> Result<bool> {
    let mut command = Command::new(program);
    command.args(args);
    exec(command, cwd, label, log).await
}

/// Run a pre-built command, streaming its output into the run log.
async fn exec(
    mut command: Command,
    cwd: &str,
    label: &str,
    log: &mut std::fs::File,
) -> Result<bool> {
    writeln!(log, "--- {label} ---")?;
    let stdout = log
        .try_clone()
        .with_context(|| format!("failed to attach log to {label}"))?;
    let stderr = log
        .try_clone()
        .with_context(|| format!("failed to attach log to {label}"))?;

    let status = command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        // Turn credential/host-key prompts into fast failures instead of hangs.
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true)
        .status()
        .await
        .with_context(|| format!("failed to spawn {label}"))?;
    Ok(status.success())
}

fn last_error(prefix: &str, log_path: &Path) -> String {
    match summary_line(log_path) {
        Some(line) => format!("{prefix}: {line}"),
        None => prefix.to_string(),
    }
}

/// Last meaningful line of the run log, used for the history summary.
///
/// Reads only the tail: a build log can be megabytes and we want one line.
fn summary_line(log_path: &Path) -> Option<String> {
    let text = read_tail(log_path, 8 * 1024);
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty() && !line.starts_with("--- ") && !line.starts_with('#'))
        .map(truncate)
}

/// Read at most the last `max_bytes` of a file, starting at a line boundary.
fn read_tail(path: &Path, max_bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let from = if start == 0 {
        0
    } else {
        buf.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1)
    };
    String::from_utf8_lossy(&buf[from..]).into_owned()
}

/// Build the [`RunRecord`], write the tail line, persist it, and return it.
fn finalize(
    log: &mut std::fs::File,
    id: &str,
    project_id: &str,
    started_at: &str,
    started: Instant,
    status: &str,
    message: String,
    commit: Option<String>,
) -> Result<RunRecord> {
    let finished_at = crate::util::now_iso();
    let duration_ms = started.elapsed().as_millis() as u64;

    let record = RunRecord {
        id: id.to_string(),
        project_id: project_id.to_string(),
        started_at: started_at.to_string(),
        finished_at: Some(finished_at.clone()),
        status: status.to_string(),
        duration_ms,
        message,
        commit,
    };

    writeln!(
        log,
        "# webhookr run {id} finished ({status}) in {duration_ms}ms"
    )?;
    crate::state::append_run(record.clone())?;
    Ok(record)
}

/// A run duration for display in a commit status or a Telegram message, where
/// there is room for a rounded figure and nothing more.
pub(crate) fn human_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}s", (ms + 500) / 1000)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Truncate to roughly [`MAX_MESSAGE_CHARS`] characters, marking cuts with `...`.
fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_MESSAGE_CHARS {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(MAX_MESSAGE_CHARS).collect();
        out.push_str("...");
        out
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::process::Command as StdCommand;

    use super::sync_project;
    use crate::config::ProjectConfig;

    #[tokio::test]
    async fn clones_missing_checkout_and_fast_forwards_existing_checkout() {
        let root =
            std::env::temp_dir().join(format!("webhookr-executor-{}", crate::util::new_run_id()));
        let remote = root.join("remote.git");
        let seed = root.join("seed");
        let checkout = root.join("checkout");
        fs::create_dir_all(&seed).unwrap();
        git(&root, &["init", "--bare", remote.to_str().unwrap()]);
        git(&seed, &["init", "-b", "main"]);
        git(&seed, &["config", "user.email", "test@example.com"]);
        git(&seed, &["config", "user.name", "Webhookr Test"]);
        fs::write(seed.join("version.txt"), "one").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-m", "initial"]);
        git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&seed, &["push", "-u", "origin", "main"]);

        let mut project = ProjectConfig::new(
            "site".into(),
            "Site".into(),
            checkout.to_string_lossy().into_owned(),
            "main".into(),
            "true".into(),
            "secret".into(),
            "github".into(),
        );
        project.repository = remote.to_string_lossy().into_owned();
        let log_path = root.join("test.log");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        sync_project(&project, &mut log, &log_path).await.unwrap();
        assert_eq!(
            fs::read_to_string(checkout.join("version.txt")).unwrap(),
            "one"
        );

        fs::write(seed.join("version.txt"), "two").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-m", "update"]);
        git(&seed, &["push"]);
        sync_project(&project, &mut log, &log_path).await.unwrap();
        assert_eq!(
            fs::read_to_string(checkout.join("version.txt")).unwrap(),
            "two"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// The guard on requirement one: GitHub being unreachable must not change
    /// the outcome of a deploy.
    #[tokio::test]
    async fn a_dead_github_does_not_fail_the_deploy() {
        let root =
            std::env::temp_dir().join(format!("webhookr-status-{}", crate::util::new_run_id()));
        fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEBHOOKR_STATE_DIR", root.join("state"));

        let mut project = ProjectConfig::new(
            "site".into(),
            "Site".into(),
            root.to_string_lossy().into_owned(),
            "main".into(),
            "true".into(),
            "secret".into(),
            "github".into(),
        );
        project.repository = "https://github.com/me/site.git".into();
        project.status_reports = true;
        project.status_token = "ghp_not-a-real-token".into();

        // Port 1 refuses instantly, so this is a fast, offline stand-in for
        // every way the Statuses API can be unavailable.
        let mut reporter = crate::github::Reporter::with_api_base(
            &project,
            crate::github::RepoSlug {
                host: "github.com".into(),
                owner: "me".into(),
                repo: "site".into(),
            },
            "http://127.0.0.1:1".into(),
        );

        let log_path = root.join("run.log");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        writeln!(log, "--- custom command ---").unwrap();
        writeln!(log, "Successfully tagged site:latest").unwrap();

        reporter
            .pending("1111111111111111111111111111111111111111", Some(&mut log))
            .await;
        reporter
            .finish(
                crate::github::State::Success,
                "Successfully tagged site:latest",
                &mut log,
            )
            .await;

        let written = fs::read_to_string(&log_path).unwrap();
        assert!(
            written.contains("# github status: could not post"),
            "the failure should be noted, not hidden: {written}"
        );
        assert!(
            !written.contains("ghp_not-a-real-token"),
            "the token must never reach the run log: {written}"
        );
        // The load-bearing part: those notes are the last lines in the log, and
        // the run's history message must still be the real command output.
        assert_eq!(
            super::summary_line(&log_path).as_deref(),
            Some("Successfully tagged site:latest")
        );

        std::env::remove_var("WEBHOOKR_STATE_DIR");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_dead_telegram_does_not_fail_the_deploy() {
        let root =
            std::env::temp_dir().join(format!("webhookr-telegram-{}", crate::util::new_run_id()));
        fs::create_dir_all(&root).unwrap();

        // Port 1 refuses instantly, so this is a fast, offline stand-in for
        // every way the Telegram API can be unavailable.
        let notifier = crate::telegram::Notifier::with_api_base(
            "123:not-a-real-token",
            "-1001",
            "http://127.0.0.1:1".into(),
        );

        let log_path = root.join("run.log");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        writeln!(log, "--- custom command ---").unwrap();
        writeln!(log, "Successfully tagged site:latest").unwrap();

        notifier
            .started("Site", "a1b2c3d4e5f6", None, &mut log)
            .await;
        let failed = crate::state::RunRecord {
            id: "a1b2c3d4e5f6".into(),
            project_id: "site".into(),
            started_at: String::new(),
            finished_at: None,
            status: "failed".into(),
            duration_ms: 100,
            message: "error: pull failed".into(),
            commit: None,
        };
        notifier
            .finished("Site", &failed, "docker: no such image", &mut log)
            .await;

        let written = fs::read_to_string(&log_path).unwrap();
        assert!(
            written.contains("# telegram: could not send"),
            "the failure should be noted, not hidden: {written}"
        );
        assert!(
            !written.contains("123:not-a-real-token"),
            "the token must never reach the run log: {written}"
        );
        // The load-bearing part: those notes are the last lines in the log, and
        // the run's history message must still be the real command output.
        assert_eq!(
            super::summary_line(&log_path).as_deref(),
            Some("Successfully tagged site:latest")
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn durations_read_as_durations() {
        assert_eq!(super::human_duration(0), "0ms");
        assert_eq!(super::human_duration(940), "940ms");
        assert_eq!(super::human_duration(1_400), "1s");
        assert_eq!(super::human_duration(1_600), "2s");
        assert_eq!(super::human_duration(59_000), "59s");
        assert_eq!(super::human_duration(150_000), "2m30s");
    }

    #[test]
    fn read_tail_starts_on_a_line_boundary() {
        let path = std::env::temp_dir().join(format!("webhookr-tail-{}", crate::util::new_run_id()));
        fs::write(&path, "alpha\nbravo\ncharlie\n").unwrap();

        // Whole file when it fits.
        assert_eq!(super::read_tail(&path, 1024), "alpha\nbravo\ncharlie\n");

        // Truncated reads drop the partial first line rather than splitting it.
        let tail = super::read_tail(&path, 12);
        assert!(!tail.contains("alpha"), "partial line leaked: {tail:?}");
        assert!(tail.ends_with("charlie\n"), "unexpected tail: {tail:?}");

        assert_eq!(super::summary_line(&path).as_deref(), Some("charlie"));
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn summary_line_skips_step_headers_and_run_banner() {
        let path = std::env::temp_dir().join(format!("webhookr-sum-{}", crate::util::new_run_id()));
        // A command that produced no output leaves only the banner and header.
        fs::write(&path, "# webhookr deploy abc for Site started now\n--- custom command ---\n")
            .unwrap();
        assert_eq!(super::summary_line(&path), None);
        fs::remove_file(&path).unwrap();
    }

    fn git(cwd: &std::path::Path, args: &[&str]) {
        let status = StdCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }
}
