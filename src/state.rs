//! Run history: a JSON index of recent runs plus per-run log files.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::config;

/// One recorded execution of a project (git pull + command).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    /// Unique run id (also the log file basename).
    pub id: String,
    /// Project this run belongs to.
    pub project_id: String,
    /// RFC3339 start time.
    pub started_at: String,
    /// RFC3339 finish time (set when the run completes).
    pub finished_at: Option<String>,
    /// `running`, `success`, or `failed`.
    pub status: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Short summary (e.g. last line of output or an error message).
    pub message: String,
}

/// Path to the runs index JSON file.
pub fn runs_path() -> PathBuf {
    config::state_dir().join("runs.json")
}

/// Directory where per-run log files live.
pub fn runs_log_dir() -> PathBuf {
    config::log_dir().join("runs")
}

/// Path of the log file for a specific run.
pub fn run_log_path(run_id: &str) -> PathBuf {
    runs_log_dir().join(format!("{run_id}.log"))
}

/// Load the run index (newest first). Returns empty vec if missing/corrupt.
pub fn load_runs() -> Vec<RunRecord> {
    let path = runs_path();
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut runs: Vec<RunRecord> = serde_json::from_str(&text).unwrap_or_default();
    runs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    runs
}

/// Persist the run index (newest first, capped).
pub fn save_runs(runs: &[RunRecord]) -> Result<()> {
    let path = runs_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut sorted = runs.to_vec();
    sorted.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    sorted.truncate(500);
    let text = serde_json::to_string_pretty(&sorted)?;
    fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Append (or replace) a run record in the index.
pub fn append_run(record: RunRecord) -> Result<()> {
    let mut runs = load_runs();
    runs.retain(|r| r.id != record.id);
    runs.push(record);
    save_runs(&runs)
}

/// Latest run for a project (if any).
pub fn latest_run(project_id: &str) -> Option<RunRecord> {
    load_runs().into_iter().find(|r| r.project_id == project_id)
}

/// Read a run's log file into a string (empty if missing).
pub fn read_run_log(run_id: &str) -> String {
    fs::read_to_string(run_log_path(run_id)).unwrap_or_default()
}
