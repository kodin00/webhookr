//! Run history: a JSON index of recent runs plus per-run log files.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::config;

/// Serializes the read-modify-write of the run index within this process.
///
/// Deliberately a `std::sync::Mutex`: making this async would force
/// `append_run` async, which would ripple through `finalize` and
/// `executor::run` for the sake of a sub-millisecond critical section.
static RUNS_LOCK: Mutex<()> = Mutex::new(());

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
    /// Full sha of the commit this run deployed, when one could be determined.
    ///
    /// Absent on runs recorded before the field existed, and on runs that never
    /// reached a checkout (a failed clone, say) — hence an Option rather than an
    /// empty string.
    #[serde(default)]
    pub commit: Option<String>,
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

/// Load the run index (newest first). Returns an empty vec when missing.
///
/// A file that fails to parse is moved aside to `runs.json.corrupt` rather than
/// silently discarded, so the history is recoverable by hand.
pub fn load_runs() -> Vec<RunRecord> {
    let path = runs_path();
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<RunRecord>>(&text) {
        Ok(mut runs) => {
            runs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
            runs
        }
        Err(error) => {
            let salvage = path.with_extension("json.corrupt");
            eprintln!(
                "webhookr: run history at {} is unreadable ({error}); moving it to {}",
                path.display(),
                salvage.display()
            );
            let _ = fs::rename(&path, &salvage);
            Vec::new()
        }
    }
}

/// Persist the run index (newest first, capped).
///
/// Callers must already hold [`RUNS_LOCK`]: `std::sync::Mutex` is not
/// reentrant, so taking it here would deadlock the read-modify-write paths.
fn save_runs_inner(runs: &[RunRecord]) -> Result<()> {
    let mut sorted = runs.to_vec();
    sorted.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    sorted.truncate(500);
    let text = serde_json::to_string_pretty(&sorted)?;
    config::write_atomic(&runs_path(), &text, false)
}

/// Append (or replace) a run record in the index, keyed by run id.
///
/// The whole load-modify-save runs under [`RUNS_LOCK`], so concurrent runs in
/// this process cannot lose each other's records.
pub fn append_run(record: RunRecord) -> Result<()> {
    let _guard = RUNS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut runs = load_runs();
    runs.retain(|r| r.id != record.id);
    runs.push(record);
    save_runs_inner(&runs)
}

/// Close out runs left `running` by a crashed or restarted daemon.
///
/// Called once at startup. A CLI run that is genuinely still in flight will be
/// marked here too, but that self-heals: its own `finalize` overwrites the
/// record by id when it finishes.
pub fn mark_interrupted_runs() -> Result<()> {
    let _guard = RUNS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut runs = load_runs();
    let mut changed = false;
    for run in runs.iter_mut().filter(|r| r.status == "running") {
        run.status = "interrupted".to_string();
        run.finished_at = Some(crate::util::now_iso());
        run.message = "daemon restarted while this run was in flight".to_string();
        changed = true;
    }
    if changed {
        save_runs_inner(&runs)?;
    }
    Ok(())
}

/// Latest run for a project (if any).
pub fn latest_run(project_id: &str) -> Option<RunRecord> {
    load_runs().into_iter().find(|r| r.project_id == project_id)
}

/// Read a run's log file into a string (empty if missing).
pub fn read_run_log(run_id: &str) -> String {
    fs::read_to_string(run_log_path(run_id)).unwrap_or_default()
}

/// Read at most the last `max_bytes` of a run's log.
///
/// Polling clients call this every couple of seconds, so reading the whole file
/// each time is wasteful once a `docker build` has produced megabytes. When the
/// file is longer than `max_bytes` the first (probably partial) line is dropped
/// and a marker is prepended, which also guarantees the result starts on a
/// UTF-8 boundary.
pub fn read_run_log_tail(run_id: &str, max_bytes: u64) -> String {
    let path = run_log_path(run_id);
    let Ok(mut file) = fs::File::open(&path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len <= max_bytes {
        let mut text = String::new();
        let _ = file.read_to_string(&mut text);
        return text;
    }

    if file.seek(SeekFrom::Start(len - max_bytes)).is_err() {
        return String::new();
    }
    let mut buf = Vec::with_capacity(max_bytes as usize);
    if file.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    // Drop through the first newline: that line is truncated, and doing so
    // also lands us on a character boundary.
    let start = buf.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
    let tail = String::from_utf8_lossy(&buf[start..]);
    format!("[... earlier output truncated ...]\n{tail}")
}
