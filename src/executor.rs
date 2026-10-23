//! Synchronizes project sources and runs deployment presets.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;
use tokio::process::Command;
use tokio::sync::Notify;

use crate::backup;
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

/// One cancel signal per in-flight run id, so the web UI can stop a deployment
/// the same daemon process is executing. A run registers its signal on start
/// and drops it on finish (success, failure, cancel, or panic) via
/// [`CancelGuard`]; `cancel_run` trips it and `exec` honours it.
///
/// `Notify` stores one permit from `notify_one`, so a cancel that arrives
/// before `exec` first awaits is not lost — the next `exec` in the run
/// consumes it immediately. That bounds the gap between a cancel and the
/// child dying to "the rest of the current step", which is as good as it gets
/// without a process group kill.
static CANCEL: LazyLock<Mutex<HashMap<String, Arc<Notify>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Trip a run's cancel signal. Returns `false` when no such run is in flight
/// (already finished, or an id that never existed) — the caller treats that as
/// "nothing to cancel".
pub fn cancel_run(run_id: &str) -> bool {
    CANCEL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(run_id)
        .cloned()
        .inspect(|notify| notify.notify_one())
        .is_some()
}

/// Inserts a run's cancel signal on construction, removes it on drop. The
/// signal stays alive for the guard's whole lifetime so `run()` can hand
/// `notify()` to every step without re-cloning from the registry.
struct CancelGuard {
    id: String,
    notify: Arc<Notify>,
}

impl CancelGuard {
    fn register(id: &str) -> Self {
        let notify = Arc::new(Notify::new());
        CANCEL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), notify.clone());
        Self {
            id: id.to_string(),
            notify,
        }
    }

    fn notify(&self) -> &Notify {
        &self.notify
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        CANCEL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

/// Whether a step ran to completion or was cancelled mid-flight.
#[derive(Debug)]
enum ExecOutcome {
    Ran(bool),
    Cancelled,
}

/// Whether syncing the source finished or was cancelled.
#[derive(Debug)]
enum SyncOutcome {
    Done,
    Cancelled,
}

/// How [`run`] moves the source checkout before deploying it.
///
/// `Forward` and `AsIs` replace the old `sync_source: bool`; `Rollback` is the
/// third shape. `run_id` rides along on `Rollback` (not just the sha) because
/// the database half of a rollback needs the *target run's* id to find its
/// pre-deploy dump, and the history wants it for `rollback_to` — threading it
/// through the mode keeps [`run`]'s signature at one discriminator.
#[derive(Debug)]
enum SyncMode {
    /// Clone or fast-forward to the branch tip, then deploy (the webhook path).
    Forward,
    /// Deploy the checkout exactly as it stands, without touching Git.
    AsIs,
    /// Detach the checkout at a previously-deployed commit and deploy that.
    /// `sha` is guard-validated (see [`rollback_sha`]) to a full 40-hex object
    /// name before it ever reaches git positionally; `run_id` is the run being
    /// rolled back to; `restore_db` switches the database half on or off.
    Rollback {
        sha: String,
        run_id: String,
        restore_db: bool,
    },
}

/// What kicked a run off.
///
/// A webhook delivery carries the push payload — already HMAC-verified by
/// [`crate::server`] — which is where the commit to report a status against
/// comes from. The CLI, TUI and admin UI carry no payload and fall back to
/// whatever the checkout has at HEAD. `source` is the display label recorded
/// on the run and shown wherever runs are.
#[derive(Debug, Default)]
pub struct Trigger {
    pub payload: Option<github::PushPayload>,
    pub source: Option<String>,
}

impl Trigger {
    /// A trigger with no payload: `label` says who asked for the run
    /// (`web UI`, `cli`). Webhook deliveries build theirs in [`crate::server`].
    fn manual(label: &str) -> Self {
        Self {
            payload: None,
            source: Some(label.to_string()),
        }
    }
}

/// Clone or fast-forward the source, then deploy it. Manual entry point
/// (`webhookr run`, and the TUI through it).
pub async fn run_project(p: &ProjectConfig) -> Result<RunRecord> {
    run(p, SyncMode::Forward, Trigger::manual("cli")).await
}

/// [`run_project`] carrying a caller-built trigger: a webhook delivery with
/// its verified payload, or the admin UI's buttons labelled `web UI`.
pub async fn run_project_with(p: &ProjectConfig, trigger: Trigger) -> Result<RunRecord> {
    run(p, SyncMode::Forward, trigger).await
}

/// Re-run the configured deployment without touching the Git checkout.
/// Used only as an explicit escape hatch (`run --no-pull`, TUI "Run deployment").
pub async fn deploy_project(p: &ProjectConfig) -> Result<RunRecord> {
    run(p, SyncMode::AsIs, Trigger::manual("cli")).await
}

/// [`deploy_project`] carrying a caller-built trigger, for the admin UI's
/// "Redeploy" button.
pub async fn deploy_project_with(p: &ProjectConfig, trigger: Trigger) -> Result<RunRecord> {
    run(p, SyncMode::AsIs, trigger).await
}

/// Roll the checkout back to the commit a past run deployed, redeploy it, and
/// (when `restore_db`) restore the database dump taken immediately before that
/// run. Manual entry point (`webhookr rollback`); callers default the database
/// half on, because undoing a bad deploy usually means undoing its schema
/// changes too.
pub async fn rollback_project(
    p: &ProjectConfig,
    target: &RunRecord,
    restore_db: bool,
) -> Result<RunRecord> {
    rollback_project_with(p, target, restore_db, Trigger::manual("rollback (cli)")).await
}

/// [`rollback_project`] carrying a caller-built trigger, for the admin UI's
/// rollback button. The guard runs here, before a run id is allocated or the
/// per-project lock taken, so an unrollable target costs nothing.
pub async fn rollback_project_with(
    p: &ProjectConfig,
    target: &RunRecord,
    restore_db: bool,
    trigger: Trigger,
) -> Result<RunRecord> {
    let sha = rollback_sha(target)?;
    run(
        p,
        SyncMode::Rollback {
            sha,
            run_id: target.id.clone(),
            restore_db,
        },
        trigger,
    )
    .await
}

async fn run(p: &ProjectConfig, mode: SyncMode, trigger: Trigger) -> Result<RunRecord> {
    p.validate()?;
    crate::config::ensure_dirs()?;

    // Built before the lock: the refusal path below reports too, and it has
    // neither a run id nor a log file to hang the report on.
    // Tokens now live on the app (shared github keys) rather than the project,
    // so the reporter resolves the status token against the live config. The
    // git credential helper does the same in `sync_project`.
    let app = crate::config::load_config().unwrap_or_default();
    let mut reporter = github::Reporter::for_project(&app, p, trigger.payload.as_ref());

    // The pushed sha is only usable when the push was to the branch this
    // project deploys. Webhook deliveries are branch-gated in the handler, but
    // the paths that bypass it (token-mode senders, an empty project branch)
    // still arrive here with an off-branch payload — reporting against its
    // commit would be a lie. When the ref does not match we fall back to
    // post-sync HEAD.
    let pushed_sha = trigger
        .payload
        .as_ref()
        .filter(|payload| github::ref_matches(payload, &p.branch))
        .and_then(github::payload_sha)
        .map(str::to_string);

    // The author of that pushed commit, when the payload carried one. A
    // manual run has no payload; a merge delivery is normalized without a
    // head commit, so this is None there too — both fall back to the checkout.
    let pushed_author = trigger
        .payload
        .as_ref()
        .filter(|payload| github::ref_matches(payload, &p.branch))
        .and_then(github::payload_author)
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
    // Registered before the `running` record is published, so a cancel issued
    // the instant the run appears in the UI still finds a signal to trip.
    let cancel = CancelGuard::register(&id);
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
    let action = match &mode {
        SyncMode::Forward => "update",
        SyncMode::AsIs => "deploy",
        SyncMode::Rollback { .. } => "rollback",
    };
    writeln!(
        log,
        "# webhookr {action} {id} for {} started {started_at}",
        p.name
    )?;
    // Recorded in the log too, so the raw log answers "why did this run"
    // on its own. The '#' keeps it out of the summary line.
    if let Some(source) = trigger.source.as_deref() {
        writeln!(log, "# trigger: {source}")?;
    }

    // The run a rollback undoes, recorded on both the `running` record and the
    // finalized one so the history shows the chain instead of a mystery jump
    // to an older commit.
    let rollback_to = match &mode {
        SyncMode::Rollback { run_id, .. } => Some(run_id.clone()),
        _ => None,
    };

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
        // Same best-guess treatment as `commit`: the pushed commit's author
        // when the payload named one, otherwise unknown until the sync lands.
        author: pushed_author.clone(),
        triggered_by: trigger.source.clone(),
        // Filled in once the final message has been attempted below.
        telegram: None,
        rollback_to: rollback_to.clone(),
    }) {
        eprintln!("webhookr: could not record run start: {error:#}");
    }

    // The database half, before any git step touches the checkout: a forward
    // run dumps (so a later rollback has data to restore); a rollback with
    // `restore_db` takes a safety dump of what is about to be overwritten and
    // then restores the target run's pre-deploy dump. Both halves run under
    // the per-project lock — nothing else may deploy into a database that is
    // mid-restore — and honour the run's cancel signal, since a dump streams
    // the same child processes a deploy does. A failure here takes the same
    // shape as a sync failure (`error`, no deploy): a deploy whose backup did
    // not land must not change code either, and a rollback whose safety dump
    // failed must not overwrite the only good copy of the current data.
    let db_error = match &mode {
        SyncMode::Forward if p.database.backups => {
            match backup::database_for(p).await {
                Some(db) => {
                    match backup::dump(p, &db, &id, "pre-deploy", &mut log, cancel.notify()).await {
                        // Ok: the dump has already written its own step lines
                        // into the log, and the backup the rollback will want
                        // is now on disk.
                        Ok(_) => None,
                        Err(error) => {
                            let _ = writeln!(log, "--- database backup error ---\n{error:#}");
                            Some(format!("pre-deploy database backup failed: {error:#}"))
                        }
                    }
                }
                // Nothing detected (`off`, or no engine this version can dump):
                // no dump, and this project's rollback story is code-only.
                None => None,
            }
        }
        SyncMode::Rollback {
            run_id,
            restore_db: true,
            ..
        } => match backup::database_for(p).await {
            Some(db) => {
                // The safety dump first: restoring is the one destructive step
                // in a rollback, so it never runs without a copy of the
                // current (bad) state already on disk.
                match backup::dump(p, &db, &id, "pre-rollback", &mut log, cancel.notify()).await {
                    Err(error) => {
                        let _ = writeln!(log, "--- database backup error ---\n{error:#}");
                        Some(format!("pre-rollback database backup failed: {error:#}"))
                    }
                    Ok(_) => match backup::backup_before_run(&p.id, run_id) {
                        None => Some(format!(
                            "no pre-deploy backup recorded for run {run_id} — rerun the rollback with the database restore disabled"
                        )),
                        Some(pre_deploy) => {
                            match backup::restore(p, &db, &pre_deploy, &mut log, cancel.notify())
                                .await
                            {
                                Ok(()) => None,
                                Err(error) => {
                                    let _ =
                                        writeln!(log, "--- database restore error ---\n{error:#}");
                                    Some(format!("database restore failed: {error:#}"))
                                }
                            }
                        }
                    },
                }
            }
            // `off`, a detection failure, or an engine this version cannot
            // dump: the rollback proceeds over code alone, and the log says so
            // plainly rather than letting a silent skip read as a restored
            // database.
            None => {
                writeln!(log, "# no database detected — rolling back code only")?;
                None
            }
        },
        // As-is redeploys, forward runs with backups off, rollbacks with the
        // database half switched off.
        _ => None,
    };

    // The sha to announce against at the start: the pushed one when we have
    // it, otherwise the checkout as it stands. A manual run carries no payload,
    // so it announces against the current HEAD, which is also what lets a
    // redeploy clear a stale red X on the commit that is actually live.
    //
    // A rollback announces nothing here: the checkout sits on the *bad*
    // commit at this point, and the finish status posts against the
    // post-checkout sha — a pending on the bad commit would never resolve.
    // The post-sync re-read of HEAD below is its first and only announcement.
    let (start_sha, start_author) = match &mode {
        SyncMode::Rollback { .. } => (None, None),
        _ => (
            match pushed_sha.clone() {
                Some(sha) => Some(sha),
                None => head_sha(&p.path).await,
            },
            // The author to announce at the start: the pushed commit's when
            // the payload named one, otherwise whoever the checkout is
            // sitting on.
            match pushed_author.clone() {
                Some(author) => Some(author),
                None => head_author(&p.path).await,
            },
        ),
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
            .started(
                &p.name,
                &id,
                start_sha.as_deref(),
                start_author.as_deref(),
                trigger.source.as_deref(),
                &mut log,
            )
            .await;
    }

    // The commit this run ends up deploying, kept for the run history. A
    // synced run re-reads HEAD after the sync, which may have moved past the
    // sha announced above; a no-sync run deploys exactly that HEAD. A rollback
    // starts from neither: the checkout's current HEAD is the commit being
    // undone, and the post-checkout re-read below records the target.
    let mut commit = match &mode {
        SyncMode::Forward => pushed_sha.clone(),
        SyncMode::AsIs => start_sha.clone(),
        SyncMode::Rollback { .. } => None,
    };
    // Its author, tracked the same way: re-read from the checkout after a
    // sync so a moved HEAD names the commit that actually deployed.
    let mut author = match &mode {
        SyncMode::Forward => pushed_author.clone(),
        SyncMode::AsIs => start_author.clone(),
        SyncMode::Rollback { .. } => None,
    };

    let (status, state, message) = if let Some(message) = db_error {
        // The database half failed before any git step ran, so the deployment
        // did not run at all — the same meaning a sync failure carries, and
        // the same shape: `failed`/`error`, no deploy, records and reporting
        // still finalize below.
        ("failed", github::State::Error, message)
    } else {
        // Whether (and how) the checkout moves before the deploy: not at all
        // for an as-is redeploy, to the branch tip for a forward run, to the
        // recorded commit for a rollback. The two synced modes then share the
        // rest of the shape — re-read HEAD, announce, deploy — so it is
        // written once here.
        let synced = match &mode {
            SyncMode::AsIs => None,
            SyncMode::Forward => Some(sync_project(p, &mut log, &log_path, cancel.notify()).await),
            SyncMode::Rollback { sha, .. } => {
                Some(rollback_sync(p, sha, &mut log, &log_path, cancel.notify()).await)
            }
        };
        match synced {
            None => deploy_phase(p, &mut log, &log_path, cancel.notify()).await,
            // The source could never be fetched, so the deployment did not run
            // at all. That is what GitHub's `error` means, as against a deploy
            // that ran and failed.
            Some(Err(message)) => ("failed", github::State::Error, message),
            // A cancel during sync never reached the deploy: report it as
            // cancelled, not failed, so the operator sees what they did.
            Some(Ok(SyncOutcome::Cancelled)) => (
                "cancelled",
                github::State::Error,
                "cancelled by operator".to_string(),
            ),
            Some(Ok(SyncOutcome::Done)) => {
                // The sync may have moved HEAD past what we announced — a
                // newer push, or a run that had already fetched — or, on a
                // rollback, moved it onto the target commit, which nothing has
                // been announced against yet (the bad pre-checkout HEAD was
                // deliberately skipped above). The commit actually deploying
                // is the post-sync HEAD — record it for the history, and
                // announce it so every sha involved gets a final state and
                // none is left pending forever.
                if let Some(head) = head_sha(&p.path).await {
                    commit = Some(head.clone());
                    if let Some(a) = head_author(&p.path).await {
                        author = Some(a);
                    }
                    if let Some(reporter) = reporter.as_mut() {
                        reporter.pending(&head, Some(&mut log)).await;
                    }
                }
                deploy_phase(p, &mut log, &log_path, cancel.notify()).await
            }
        }
    };

    // History before GitHub: the admin UI polls `finished_at` to stop tailing
    // the log, so a slow API call must not hold a finished run visibly open.
    let mut record = finalize(
        &mut log,
        &id,
        &p.id,
        &started_at,
        started,
        status,
        message.clone(),
        commit,
        author,
        trigger.source.clone(),
        rollback_to,
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
        let delivery = telegram.finished(&p.name, &record, &tail, &mut log).await;
        // The outcome goes onto the persisted record, not just the log: the
        // run page stops polling the log as soon as the run leaves `running`,
        // which is before the `# telegram:` note is written. `append_run`
        // replaces by id, and like every history write this is best-effort.
        record.telegram = Some(delivery);
        if let Err(error) = crate::state::append_run(record.clone()) {
            eprintln!("webhookr: could not record the Telegram outcome: {error:#}");
        }
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
    cancel: &Notify,
) -> (&'static str, github::State, String) {
    let outcome = match deploy(p, log, cancel).await {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = writeln!(log, "--- deployment error ---\n{error:#}");
            ExecOutcome::Ran(false)
        }
    };
    match outcome {
        ExecOutcome::Cancelled => (
            "cancelled",
            github::State::Error,
            "cancelled by operator".to_string(),
        ),
        ExecOutcome::Ran(ok) => {
            let message = summary_line(log_path).unwrap_or_else(|| "no output".to_string());
            if ok {
                ("success", github::State::Success, message)
            } else {
                ("failed", github::State::Failure, message)
            }
        }
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

/// The author of the commit currently checked out, read with `git log` the
/// same way [`head_sha`] reads the sha. `None` when there is no checkout yet
/// or `git` is unusable — the run records and messages fall back to "unknown".
async fn head_author(path: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["log", "-1", "--format=%an"])
        .current_dir(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// The token git authenticates with for this project, resolved against the
/// live config so a project linked to a shared github key authenticates with
/// that key's token, and a legacy project with a per-project token still
/// works. Local file remotes (the test fixtures) carry no token and need no
/// auth.
fn live_git_token(p: &ProjectConfig) -> String {
    crate::config::load_config()
        .map(|app| app.git_token_for(p).to_string())
        .unwrap_or_else(|_| p.git_token.trim().to_string())
}

async fn sync_project(
    p: &ProjectConfig,
    log: &mut std::fs::File,
    log_path: &Path,
    cancel: &Notify,
) -> std::result::Result<SyncOutcome, String> {
    let token = live_git_token(p);
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
        let mut command = git_command(&token);
        command.args([
            "clone",
            "--branch",
            p.branch.as_str(),
            "--single-branch",
            p.repository.as_str(),
            p.path.as_str(),
        ]);
        return match exec(command, &cwd, "git clone", log, cancel).await {
            Err(error) => Err(format!("failed to run git clone: {error:#}")),
            Ok(ExecOutcome::Cancelled) => Ok(SyncOutcome::Cancelled),
            Ok(ExecOutcome::Ran(true)) => Ok(SyncOutcome::Done),
            Ok(ExecOutcome::Ran(false)) => Err(last_error("git clone failed", log_path)),
        };
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
        let mut command = git_command(&token);
        command.args(&args);
        match exec(command, &p.path, &label, log, cancel).await {
            Err(error) => return Err(format!("failed to run {label}: {error:#}")),
            Ok(ExecOutcome::Cancelled) => return Ok(SyncOutcome::Cancelled),
            Ok(ExecOutcome::Ran(false)) => {
                return Err(last_error(&format!("{label} failed"), log_path));
            }
            Ok(ExecOutcome::Ran(true)) => {}
        }
    }
    Ok(SyncOutcome::Done)
}

/// A full 40-hex-digit object name — the only commit spelling the rollback
/// path will hand to `git checkout` positionally. Short sha prefixes and
/// anything flag-shaped (a leading `-`) are refused by the caller up front
/// instead of being left for git to interpret.
fn is_full_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The commit a rollback of `target` returns to, once `target` is rollable:
/// a run that succeeded and recorded a full sha (see [`is_full_sha`]).
///
/// Guarded here — before a run id is allocated, the lock taken, or anything
/// executed — so an unrollable target costs nothing, and the error names the
/// run because the caller (CLI, admin UI) points at it by id.
fn rollback_sha(target: &RunRecord) -> Result<String> {
    let commit = target.commit.as_deref().unwrap_or_default();
    if target.status == "success" && is_full_sha(commit) {
        return Ok(commit.to_string());
    }
    bail!(
        "can only roll back to a successful run with a recorded commit — \
         run {} is {} with commit {}",
        target.id,
        target.status,
        if commit.is_empty() { "none" } else { commit }
    );
}

/// Move the checkout to a previously-deployed commit for a rollback run:
/// fetch the branch, then detach at `sha`.
///
/// A sibling of [`sync_project`] rather than another branch inside it because
/// the two share no first step: sync may clone when the checkout is missing, a
/// rollback has nothing to clone into (no checkout means no deployed history
/// to return to) and refuses instead. It never pulls and never touches the
/// branch ref — the branch stays at its remote tip, so the next forward run's
/// `checkout <branch>` + `pull --ff-only` self-heals the detached state with
/// no operator action. `sha` arrives guard-validated to 40 hex digits, so it
/// cannot read as a git flag.
async fn rollback_sync(
    p: &ProjectConfig,
    sha: &str,
    log: &mut std::fs::File,
    log_path: &Path,
    cancel: &Notify,
) -> std::result::Result<SyncOutcome, String> {
    if !Path::new(&p.path).join(".git").exists() {
        return Err(format!("cannot roll back: no checkout at {}", p.path));
    }
    let token = live_git_token(p);
    let git_steps: [(&str, Vec<&str>); 2] = [
        ("fetch", vec!["fetch", "origin", p.branch.as_str()]),
        // --detach is the point: the branch ref is left pointing at the
        // remote tip for the next forward run to heal from.
        ("checkout", vec!["checkout", "--detach", sha]),
    ];
    for (step, args) in git_steps {
        let label = format!("git {step}");
        let mut command = git_command(&token);
        command.args(&args);
        match exec(command, &p.path, &label, log, cancel).await {
            Err(error) => return Err(format!("failed to run {label}: {error:#}")),
            Ok(ExecOutcome::Cancelled) => return Ok(SyncOutcome::Cancelled),
            Ok(ExecOutcome::Ran(false)) => {
                return Err(last_error(&format!("{label} failed"), log_path));
            }
            Ok(ExecOutcome::Ran(true)) => {}
        }
    }
    Ok(SyncOutcome::Done)
}

async fn deploy(
    p: &ProjectConfig,
    log: &mut std::fs::File,
    cancel: &Notify,
) -> Result<ExecOutcome> {
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
            match run_command("docker", &pull, &p.path, "docker compose pull", log, cancel).await? {
                ExecOutcome::Cancelled => return Ok(ExecOutcome::Cancelled),
                ExecOutcome::Ran(false) => return Ok(ExecOutcome::Ran(false)),
                ExecOutcome::Ran(true) => {}
            }
        }
        let mut up = base;
        up.extend(["up", "-d"]);
        if p.deploy_preset == "compose_build" {
            up.push("--build");
        }
        up.push("--remove-orphans");
        run_command("docker", &up, &p.path, "docker compose up", log, cancel).await
    } else {
        run_command(
            "sh",
            &["-c", p.command.as_str()],
            &p.path,
            "custom command",
            log,
            cancel,
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
fn git_command(token: &str) -> Command {
    let mut command = Command::new("git");
    let token = token.trim();
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
    cancel: &Notify,
) -> Result<ExecOutcome> {
    let mut command = Command::new(program);
    command.args(args);
    exec(command, cwd, label, log, cancel).await
}

/// Run a pre-built command, streaming its output into the run log.
///
/// Awaits the child in a `select!` against the run's cancel signal: a cancel
/// kills the child (and reaps it, so no zombie) and returns
/// [`ExecOutcome::Cancelled`], which propagates up to mark the run `cancelled`
/// and free the per-project lock. `kill_on_drop` is still set as a backstop,
/// but the cancel path reaps the child explicitly.
async fn exec(
    mut command: Command,
    cwd: &str,
    label: &str,
    log: &mut std::fs::File,
    cancel: &Notify,
) -> Result<ExecOutcome> {
    writeln!(log, "--- {label} ---")?;
    let stdout = log
        .try_clone()
        .with_context(|| format!("failed to attach log to {label}"))?;
    let stderr = log
        .try_clone()
        .with_context(|| format!("failed to attach log to {label}"))?;

    let mut child = command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        // Turn credential/host-key prompts into fast failures instead of hangs.
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to spawn {label}"))?;

    let status = tokio::select! {
        biased;
        // A stored permit from `cancel_run` makes this fire even if the cancel
        // arrived before this step began awaiting.
        () = cancel.notified() => {
            let _ = child.kill().await;
            // Reap the killed child so it does not linger as a zombie.
            let _ = child.wait().await;
            let _ = writeln!(log, "--- {label} cancelled ---");
            return Ok(ExecOutcome::Cancelled);
        }
        status = child.wait() => status,
    };
    Ok(ExecOutcome::Ran(
        status
            .with_context(|| format!("failed to spawn {label}"))?
            .success(),
    ))
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
///
/// `rollback_to` is threaded from the run's mode onto both the `running`
/// record and this final one, so the history names the run a rollback undoes
/// even when the rollback itself fails halfway.
#[allow(clippy::too_many_arguments)]
fn finalize(
    log: &mut std::fs::File,
    id: &str,
    project_id: &str,
    started_at: &str,
    started: Instant,
    status: &str,
    message: String,
    commit: Option<String>,
    author: Option<String>,
    triggered_by: Option<String>,
    rollback_to: Option<String>,
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
        author,
        triggered_by,
        telegram: None,
        rollback_to,
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

    use super::{rollback_sha, rollback_sync, sync_project, ExecOutcome};
    use crate::config::ProjectConfig;
    use crate::state::RunRecord;

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
        // A fresh, untripped signal: this test never cancels.
        let cancel = tokio::sync::Notify::new();
        sync_project(&project, &mut log, &log_path, &cancel)
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
        sync_project(&project, &mut log, &log_path, &cancel)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(checkout.join("version.txt")).unwrap(),
            "two"
        );

        // A rollback detaches at the "one" commit. The branch must be left at
        // "two" (the remote tip) rather than rewritten, so the next forward
        // run heals the detached state by itself — which the final sync here
        // proves end to end.
        let one = git_output(&seed, &["rev-parse", "HEAD~1"]);
        rollback_sync(&project, &one, &mut log, &log_path, &cancel)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(checkout.join("version.txt")).unwrap(),
            "one"
        );
        // Detached HEAD: `symbolic-ref -q` fails when HEAD names no branch.
        let symbolic = StdCommand::new("git")
            .args(["symbolic-ref", "-q", "HEAD"])
            .current_dir(&checkout)
            .status()
            .unwrap();
        assert!(
            !symbolic.success(),
            "a rollback must detach HEAD, not move the branch"
        );
        sync_project(&project, &mut log, &log_path, &cancel)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(checkout.join("version.txt")).unwrap(),
            "two"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// The rollback guard: only a run that succeeded and recorded a full
    /// 40-hex sha is a rollable target. The sha later reaches `git checkout`
    /// positionally, so a short prefix or anything flag-shaped is refused
    /// here rather than handed to git to interpret.
    #[test]
    fn rollback_guard_requires_success_and_a_full_sha() {
        let sha = "1111111111111111111111111111111111111111";
        let mut target = run_record("success", Some(sha));
        assert_eq!(rollback_sha(&target).unwrap(), sha);

        // A failed run is never rollable, however good its commit looks.
        target.status = "failed".into();
        let error = rollback_sha(&target).unwrap_err().to_string();
        assert!(error.contains("can only roll back"), "{error}");
        assert!(
            error.contains("run-7"),
            "the message must name the run: {error}"
        );

        // Guard-validated sha shapes: a real-but-short prefix, nothing at all,
        // and a flag-shaped 40-char string.
        target.status = "success".into();
        for bad in [
            "1c7ad01".to_string(),
            String::new(),
            format!("-{}", "1".repeat(39)),
        ] {
            target.commit = Some(bad.clone());
            assert!(
                rollback_sha(&target).is_err(),
                "{bad:?} must not be rollable"
            );
        }
        target.commit = None;
        assert!(rollback_sha(&target).is_err(), "no recorded commit");
    }

    /// A minimal run record for the rollback guard tests: only `status` and
    /// `commit` matter to it.
    fn run_record(status: &str, commit: Option<&str>) -> RunRecord {
        RunRecord {
            id: "run-7".into(),
            project_id: "site".into(),
            started_at: String::new(),
            finished_at: None,
            status: status.into(),
            duration_ms: 0,
            message: String::new(),
            commit: commit.map(str::to_string),
            author: None,
            triggered_by: None,
            telegram: None,
            rollback_to: None,
        }
    }

    /// Cancelling mid-run kills the child and returns `Cancelled`, so the run
    /// is marked `cancelled` instead of dragging a hung command out forever.
    #[tokio::test]
    async fn cancelling_a_running_command_kills_the_child() {
        use std::time::Duration;

        let root =
            std::env::temp_dir().join(format!("webhookr-cancel-{}", crate::util::new_run_id()));
        fs::create_dir_all(&root).unwrap();
        let log_path = root.join("run.log");

        // Writes its own pid to a marker then sleeps long enough to cancel.
        let marker = root.join("pid");
        let script = format!("echo $$ > {}\nsleep 30", marker.display());
        let cwd = root.to_string_lossy().into_owned();
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());

        // Run exec in its own task so the child actually spawns and runs while
        // we cancel from here; awaiting a pinned future would not poll it until
        // we awaited, so the cancel would fire before the child existed.
        let cancel_for_task = cancel.clone();
        let handle = tokio::spawn(async move {
            let mut log = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .unwrap();
            let mut command = tokio::process::Command::new("sh");
            command.arg("-c").arg(script);
            super::exec(command, &cwd, "sleep-test", &mut log, &cancel_for_task).await
        });

        // Let the child spawn and write its pid before we cancel.
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.notify_one();
        let outcome = handle.await.unwrap();
        assert!(
            matches!(outcome, Ok(ExecOutcome::Cancelled)),
            "a cancelled step should report Cancelled, got {outcome:?}"
        );

        let pid: i32 = fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .expect("the child wrote its pid");
        // SIGKILL is not instantaneous; give it a beat then prove the pid is gone.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let still_alive = StdCommand::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!still_alive, "child {pid} still alive after cancel");

        fs::remove_dir_all(root).unwrap();
    }

    /// The guard on requirement one: GitHub being unreachable must not change
    /// the outcome of a deploy.
    #[tokio::test]
    // The std guard is held across the test's awaits on purpose: it must span
    // the whole set_var … remove_var window, and nothing on this thread ever
    // re-locks it, so the await points are safe.
    #[allow(clippy::await_holding_lock)]
    async fn a_dead_github_does_not_fail_the_deploy() {
        // Held for the whole test: WEBHOOKR_STATE_DIR is process-wide, so any
        // other test redirecting the state dir concurrently would interleave
        // with this one's set/remove.
        let _state_dir = crate::util::STATE_DIR_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
            &project.status_token,
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
            .started(
                "Site",
                "a1b2c3d4e5f6",
                None,
                None,
                Some("push by alice"),
                &mut log,
            )
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
            author: None,
            triggered_by: Some("push by alice".into()),
            telegram: None,
            rollback_to: None,
        };
        let delivery = notifier
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
        // The caller records the delivery on the run; it must carry the same
        // verdict, still without the token.
        assert!(!delivery.sent);
        assert!(
            !delivery.detail.is_empty(),
            "a reason is worth recording: {delivery:?}"
        );
        assert!(
            !delivery.detail.contains("123:not-a-real-token"),
            "the token must never reach the record: {:?}",
            delivery.detail
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
        let path =
            std::env::temp_dir().join(format!("webhookr-tail-{}", crate::util::new_run_id()));
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
        fs::write(
            &path,
            "# webhookr deploy abc for Site started now\n--- custom command ---\n",
        )
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

    /// Run git and return its stdout, for capturing shas in fixtures.
    fn git_output(cwd: &std::path::Path, args: &[&str]) -> String {
        let output = StdCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }
}
