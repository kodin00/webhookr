//! Runs a project's deploy: `git fetch`/`checkout`/`pull`, then a shell command.

use anyhow::{Context, Result};
use std::io::Write;
use std::time::Instant;
use tokio::process::Command;

use crate::config::ProjectConfig;
use crate::state::RunRecord;

/// Rough cap for the summary line derived from command output.
const MAX_MESSAGE_CHARS: usize = 200;

/// Run `git fetch/checkout/pull` then the project command, logging output and
/// recording a [`RunRecord`]. Returns the final record on success; returns an
/// `Err` only for infrastructure errors (bad path, log write failure, ...).
pub async fn run_project(p: &ProjectConfig) -> Result<RunRecord> {
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
    writeln!(log, "# webhookr run {id} for {} started {started_at}", p.name)?;

    // Everything printed by any command, so the summary can pick the last line.
    let mut combined = String::new();

    // Git steps, in order. `--ff-only` turns a dirty tree into a clean failure
    // instead of a merge.
    let git_steps: [(&str, Vec<&str>); 3] = [
        ("fetch", vec!["fetch", "origin"]),
        ("checkout", vec!["checkout", p.branch.as_str()]),
        ("pull", vec!["pull", "--ff-only", "origin", p.branch.as_str()]),
    ];
    for (step, args) in git_steps.iter() {
        let output = Command::new("git")
            .args(args.as_slice())
            .current_dir(&p.path)
            .output()
            .await
            .with_context(|| format!("failed to spawn git {step}"))?;
        record_output(&mut log, &mut combined, &format!("git {step}"), &output)?;

        if !output.status.success() {
            let message = match last_line(&combined) {
                Some(line) => format!("git pull failed: {line}"),
                None => "git pull failed".to_string(),
            };
            return finalize(&mut log, &id, &p.id, &started_at, started, "failed", message);
        }
    }

    // Deploy command through a shell so pipes/redirects behave as written.
    let output = Command::new("sh")
        .arg("-c")
        .arg(&p.command)
        .current_dir(&p.path)
        .output()
        .await
        .context("failed to spawn deploy command")?;
    record_output(&mut log, &mut combined, "command", &output)?;

    let status = if output.status.success() { "success" } else { "failed" };
    let message = last_line(&combined).unwrap_or_else(|| "no output".to_string());
    finalize(&mut log, &id, &p.id, &started_at, started, status, message)
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

    writeln!(log, "# webhookr run {id} finished ({status}) in {duration_ms}ms")?;
    crate::state::append_run(record.clone())?;
    Ok(record)
}

/// Last non-empty, whitespace-trimmed line of `text`, truncated for the summary.
fn last_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .next_back()
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
