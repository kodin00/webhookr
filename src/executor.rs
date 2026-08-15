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

/// Clone or fast-forward the source, then deploy it.
pub async fn run_project(p: &ProjectConfig) -> Result<RunRecord> {
    run(p, true).await
}

/// Re-run the configured deployment without touching the Git checkout.
/// Used only as an explicit escape hatch (`run --no-pull`, TUI "Run deployment").
pub async fn deploy_project(p: &ProjectConfig) -> Result<RunRecord> {
    run(p, false).await
}

async fn run(p: &ProjectConfig, sync_source: bool) -> Result<RunRecord> {
    p.validate()?;
    crate::config::ensure_dirs()?;

    // Single-flight per project: a webhook and a manual trigger must not race
    // `git pull` and `docker compose up` on the same checkout. `try_lock`
    // rather than `lock().await`, so a run that hangs (there is no timeout yet)
    // cannot wedge the project behind an invisible queue.
    let lock = project_lock(&p.id);
    let Ok(_guard) = lock.try_lock() else {
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
    }) {
        eprintln!("webhookr: could not record run start: {error:#}");
    }

    if sync_source {
        if let Err(message) = sync_project(p, &mut log, &log_path).await {
            return finalize(
                &mut log,
                &id,
                &p.id,
                &started_at,
                started,
                "failed",
                message,
            );
        }
    }

    let deploy_ok = match deploy(p, &mut log).await {
        Ok(ok) => ok,
        Err(error) => {
            writeln!(log, "--- deployment error ---\n{error:#}")?;
            false
        }
    };
    let status = if deploy_ok { "success" } else { "failed" };
    let message = summary_line(&log_path).unwrap_or_else(|| "no output".to_string());
    finalize(&mut log, &id, &p.id, &started_at, started, status, message)
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
        let ok = run_command(
            "git",
            &[
                "clone",
                "--branch",
                p.branch.as_str(),
                "--single-branch",
                p.repository.as_str(),
                p.path.as_str(),
            ],
            &cwd,
            "git clone",
            log,
        )
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
        let ok = run_command("git", &args, &p.path, &label, log)
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
    writeln!(log, "--- {label} ---")?;
    let stdout = log
        .try_clone()
        .with_context(|| format!("failed to attach log to {label}"))?;
    let stderr = log
        .try_clone()
        .with_context(|| format!("failed to attach log to {label}"))?;

    let status = Command::new(program)
        .args(args)
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
    };

    writeln!(
        log,
        "# webhookr run {id} finished ({status}) in {duration_ms}ms"
    )?;
    crate::state::append_run(record.clone())?;
    Ok(record)
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
