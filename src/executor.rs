//! Synchronizes project sources and runs deployment presets.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;
use std::time::Instant;
use tokio::process::Command;

use crate::config::ProjectConfig;
use crate::state::RunRecord;

/// Rough cap for the summary line derived from command output.
const MAX_MESSAGE_CHARS: usize = 200;

/// Clone or fast-forward the source, then deploy it.
pub async fn run_project(p: &ProjectConfig) -> Result<RunRecord> {
    run(p, true).await
}

/// Re-run the configured deployment without touching the Git checkout.
pub async fn deploy_project(p: &ProjectConfig) -> Result<RunRecord> {
    run(p, false).await
}

async fn run(p: &ProjectConfig, sync_source: bool) -> Result<RunRecord> {
    p.validate()?;
    crate::config::ensure_dirs()?;

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

    // Everything printed by any command, so the summary can pick the last line.
    let mut combined = String::new();

    if sync_source {
        if let Err(message) = sync_project(p, &mut log, &mut combined).await {
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

    let deploy_ok = match deploy(p, &mut log, &mut combined).await {
        Ok(ok) => ok,
        Err(error) => {
            writeln!(log, "--- deployment error ---\n{error:#}")?;
            combined.push_str(&format!("deployment failed: {error:#}\n"));
            false
        }
    };
    let status = if deploy_ok { "success" } else { "failed" };
    let message = last_line(&combined).unwrap_or_else(|| "no output".to_string());
    finalize(&mut log, &id, &p.id, &started_at, started, status, message)
}

async fn sync_project(
    p: &ProjectConfig,
    log: &mut std::fs::File,
    combined: &mut String,
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
        let output = Command::new("git")
            .args([
                "clone",
                "--branch",
                p.branch.as_str(),
                "--single-branch",
                p.repository.as_str(),
                p.path.as_str(),
            ])
            .output()
            .await
            .map_err(|error| format!("failed to spawn git clone: {error}"))?;
        record_output(log, combined, "git clone", &output)
            .map_err(|error| format!("failed to record git clone: {error}"))?;
        return output
            .status
            .success()
            .then_some(())
            .ok_or_else(|| last_error("git clone failed", combined));
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
        let output = Command::new("git")
            .args(&args)
            .current_dir(path)
            .output()
            .await
            .map_err(|error| format!("failed to spawn git {step}: {error}"))?;
        record_output(log, combined, &format!("git {step}"), &output)
            .map_err(|error| format!("failed to record git {step}: {error}"))?;
        if !output.status.success() {
            return Err(last_error(&format!("git {step} failed"), combined));
        }
    }
    Ok(())
}

async fn deploy(p: &ProjectConfig, log: &mut std::fs::File, combined: &mut String) -> Result<bool> {
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
            if !run_command(
                "docker",
                &pull,
                &p.path,
                "docker compose pull",
                log,
                combined,
            )
            .await?
            {
                return Ok(false);
            }
        }
        let mut up = base;
        up.extend(["up", "-d"]);
        if p.deploy_preset == "compose_build" {
            up.push("--build");
        }
        up.push("--remove-orphans");
        run_command("docker", &up, &p.path, "docker compose up", log, combined).await
    } else {
        run_command(
            "sh",
            &["-c", p.command.as_str()],
            &p.path,
            "custom command",
            log,
            combined,
        )
        .await
    }
}

async fn run_command(
    program: &str,
    args: &[&str],
    cwd: &str,
    label: &str,
    log: &mut std::fs::File,
    combined: &mut String,
) -> Result<bool> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("failed to spawn {label}"))?;
    record_output(log, combined, label, &output)?;
    Ok(output.status.success())
}

fn last_error(prefix: &str, output: &str) -> String {
    match last_line(output) {
        Some(line) => format!("{prefix}: {line}"),
        None => prefix.to_string(),
    }
}

/// Append one command's stdout and stderr (labeled) to the log and to the
/// shared buffer used to derive the summary line.
fn record_output(
    log: &mut std::fs::File,
    combined: &mut String,
    step: &str,
    output: &std::process::Output,
) -> Result<()> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    writeln!(log, "--- {step} stdout ---")?;
    write!(log, "{stdout}")?;
    if !stdout.is_empty() && !stdout.ends_with('\n') {
        writeln!(log)?;
    }

    writeln!(log, "--- {step} stderr ---")?;
    write!(log, "{stderr}")?;
    if !stderr.is_empty() && !stderr.ends_with('\n') {
        writeln!(log)?;
    }

    combined.push_str(&stdout);
    combined.push_str(&stderr);
    Ok(())
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

/// Last non-empty, whitespace-trimmed line of `text`, truncated for the summary.
fn last_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(truncate)
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
        let mut log = fs::File::create(&log_path).unwrap();
        let mut combined = String::new();
        sync_project(&project, &mut log, &mut combined)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(checkout.join("version.txt")).unwrap(),
            "one"
        );

        fs::write(seed.join("version.txt"), "two").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-m", "update"]);
        git(&seed, &["push"]);
        sync_project(&project, &mut log, &mut combined)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(checkout.join("version.txt")).unwrap(),
            "two"
        );
        fs::remove_dir_all(root).unwrap();
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
