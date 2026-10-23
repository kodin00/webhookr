//! Database backups: the data half of a rollback.
//!
//! Rollback's git half replays a recorded commit; this module makes the *data*
//! half possible by dumping the project's database before every forward
//! deploy, so a rollback can restore the state the previously-good deploy saw.
//! Dump/restore run through `docker compose exec` against the database's own
//! service, resolving credentials from the container's environment so tokens
//! never reach the webhookr config, the run log, or the process list.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use tokio::process::Command;
use tokio::sync::Notify;

use crate::config::{self, ProjectConfig};
use crate::util;

/// Basename of the manifest inside a project's backups directory: a JSON
/// `Vec<Backup>`, stored newest first.
const INDEX_FILE: &str = "index.json";

/// Serializes the read-modify-write of backup manifests within this process,
/// so a pre-deploy dump and a rollback's safety dump cannot lose each other's
/// rows. Held only across synchronous file I/O, never across an `.await` —
/// the same discipline as the run index's lock in `crate::state`.
static INDEX_LOCK: Mutex<()> = Mutex::new(());

/// A database engine this module can dump and restore.
///
/// Mongo/Redis are deliberately absent: they can be *detected* in a compose
/// file, but v1 has no dump/restore for them, so detection reports them as
/// "code only" rollbacks instead of pretending data is covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    Postgres,
    Mysql,
    Mariadb,
}

impl DbKind {
    /// Lowercase engine name, used in the manifest and shown in the UI.
    pub fn as_str(&self) -> &'static str {
        match self {
            DbKind::Postgres => "postgres",
            DbKind::Mysql => "mysql",
            DbKind::Mariadb => "mariadb",
        }
    }
}

/// A project's database, however it was found.
#[derive(Debug, Clone)]
pub struct DbInfo {
    pub kind: DbKind,
    /// Compose service running the database (e.g. `db`).
    pub service: String,
}

/// The database a project has, if any.
///
/// `auto` asks Compose itself (`docker compose -f … config --format json`)
/// which services run a known database image — delegating the parsing means
/// `env_file`, variables and `extends` all resolve the same way they do at
/// deploy time. `manual` trusts the operator's `kind`/`service`. `off` (and
/// any detection failure) is `None`: no backups, and a rollback restores code
/// only.
pub async fn database_for(p: &ProjectConfig) -> Option<DbInfo> {
    match p.database.mode.as_str() {
        "off" => None,
        "manual" => Some(DbInfo {
            // `validate` confines a manual `kind` to the three engines; a
            // hand-edited typo still has to map somewhere, and treating it as
            // postgres matches how the image names spell the default.
            kind: match p.database.kind.as_str() {
                "mysql" => DbKind::Mysql,
                "mariadb" => DbKind::Mariadb,
                _ => DbKind::Postgres,
            },
            service: p.database.service.clone(),
        }),
        // Only a compose deployment has a file to probe; a custom-command
        // project has nothing `docker compose config` could resolve.
        "auto" if p.uses_compose() => detect_via_compose(p).await,
        _ => None,
    }
}

/// Ask Compose which services the project really runs, and look for a known
/// database among them.
///
/// Any failure — no docker on PATH, a non-zero exit from a broken compose
/// file, unparseable output — is `None`: detection is best-effort by
/// contract, and a rollback then restores code only, which is honest.
async fn detect_via_compose(p: &ProjectConfig) -> Option<DbInfo> {
    let mut args = vec![
        "compose".to_string(),
        "-f".to_string(),
        p.compose_file.clone(),
    ];
    for profile in &p.compose_profiles {
        args.extend(["--profile".to_string(), profile.clone()]);
    }
    args.extend([
        "config".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ]);

    let output = Command::new("docker")
        .args(&args)
        .current_dir(&p.path)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let (kind, service) = detect_service(&String::from_utf8_lossy(&output.stdout))?;
    Some(DbInfo { kind, service })
}

/// Find a known database service in `docker compose config --format json`
/// output. Pure, so the matching rules are testable without docker.
///
/// Both shapes docker has printed over the years are tolerated: today's full
/// config object (a `services` key beside `volumes`/`networks`) and older
/// releases' bare services map. Images are compared by name only — registry
/// prefix and `:tag` stripped, so `registry.example.com:5000/postgres:16` is
/// postgres. When several services match, one literally named `db` or
/// `database` wins (the convention every compose quickstart uses), else the
/// first in the map.
fn detect_service(compose_config_json: &str) -> Option<(DbKind, String)> {
    let value: serde_json::Value = serde_json::from_str(compose_config_json).ok()?;
    let services = value
        .get("services")
        .and_then(serde_json::Value::as_object)
        // The bare shape: a payload with no `services` key is read as the
        // services map itself.
        .or_else(|| value.as_object())?;

    let mut matches: Vec<(DbKind, String)> = Vec::new();
    for (service, definition) in services {
        let kind = definition
            .get("image")
            .and_then(serde_json::Value::as_str)
            .and_then(kind_for_image);
        if let Some(kind) = kind {
            matches.push((kind, service.clone()));
        }
    }
    matches
        .iter()
        .find(|(_, service)| service == "db" || service == "database")
        .or_else(|| matches.first())
        .cloned()
}

/// Which engine an image runs, if it is one this module can dump.
///
/// Redis, Mongo and friends are deliberately unmatched: they can be detected
/// in a compose file, but v1 has no dump/restore for them, so a rollback for
/// them restores code only rather than pretending the data is covered.
fn kind_for_image(image: &str) -> Option<DbKind> {
    // A `:tag` only ever follows the last path separator — the port in a
    // registry prefix (`registry:5000/postgres`) must not be mistaken for one.
    let repository = image.rsplit_once('/').map_or(image, |(_, name)| name);
    let name = repository.split(':').next().unwrap_or(repository);
    match name {
        "postgres" | "postgresql" | "postgis" => Some(DbKind::Postgres),
        "mysql" => Some(DbKind::Mysql),
        "mariadb" => Some(DbKind::Mariadb),
        _ => None,
    }
}

/// One dump on disk, as recorded in the project's manifest.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Backup {
    /// Basename of the dump file inside the project's backups directory.
    pub file: String,
    /// RFC3339 creation time.
    pub created_at: String,
    /// Run id this dump was taken immediately before (the run whose rollback
    /// restores it). Empty for dumps taken outside a run.
    pub before_run: String,
    /// Engine the dump came from (`postgres`, `mysql`, `mariadb`).
    pub kind: String,
    /// Why the dump exists: `pre-deploy`, `pre-rollback` (the safety dump a
    /// rollback takes of the current state before overwriting it), or `manual`
    /// (the project page's "Back up now" button / `webhookr backup`).
    pub label: String,
    /// Dump size in bytes, for the confirm page.
    pub size: u64,
}

/// Directory holding a project's dumps and manifest.
pub fn backups_dir(project_id: &str) -> PathBuf {
    let dir = config::state_dir().join("backups").join(project_id);
    // Created here rather than at startup so the first dump on a fresh
    // install needs no ordering dependency on `ensure_dirs`. Best-effort: a
    // reader of a missing dir sees "no backups yet", and a writer that cannot
    // create it fails with a real error naming the path.
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The project's manifest, newest first. Missing file is an empty vec.
pub fn load_index(project_id: &str) -> Vec<Backup> {
    let path = backups_dir(project_id).join(INDEX_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<Backup>>(&text) {
        Ok(mut backups) => {
            // Persisted newest-first already; sorting again costs nothing and
            // keeps a hand-edited or partially-ordered file honest.
            backups.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            backups
        }
        Err(error) => {
            // The corrupt file is deliberately left in place — deleting the
            // only record of which dumps exist would orphan them — and reads
            // as empty; the next successful dump rewrites it.
            eprintln!(
                "webhookr: backup manifest at {} is unreadable ({error}); treating it as empty",
                path.display()
            );
            Vec::new()
        }
    }
}

/// The dump taken immediately before `run_id` started, if there was one.
///
/// This is the exact data state a rollback of that run restores: dump N is
/// taken before deploy N, so undoing deploy N means restoring dump N.
pub fn backup_before_run(project_id: &str, run_id: &str) -> Option<Backup> {
    load_index(project_id)
        .into_iter()
        .find(|backup| backup.before_run == run_id)
}

/// Dump the database to `<state>/backups/<project>/`, record it in the
/// manifest, prune past `keep`, and return the manifest entry.
///
/// Output streams into `log` so the run log shows the dump happening; a
/// cancel kills the dump child. Credentials resolve inside the container
/// (`$POSTGRES_USER`, `$MYSQL_ROOT_PASSWORD`, …) so nothing secret reaches
/// this process or the log.
pub async fn dump(
    p: &ProjectConfig,
    db: &DbInfo,
    before_run: &str,
    label: &str,
    log: &mut File,
    cancel: &Notify,
) -> Result<Backup> {
    let dir = backups_dir(&p.id);
    let file_name = dump_file_name(before_run, label);
    let dump_path = dir.join(&file_name);

    // Open before anything else: a state directory that cannot host the file
    // is a local failure worth a precise error, not a dump with nowhere to
    // land. Truncate, so a dump retried after a cancelled one can never
    // splice itself onto a partial predecessor.
    let dump_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&dump_path)
        .with_context(|| format!("failed to create {}", dump_path.display()))?;

    writeln!(log, "--- database dump → {file_name} ---")?;
    let stderr = log
        .try_clone()
        .with_context(|| "failed to attach the run log to the database dump")?;

    let mut command = Command::new("docker");
    command
        .args(compose_exec_args(p, &db.service, dump_command(db.kind)))
        .current_dir(&p.path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(dump_file))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            // The dump file was created above; an empty `.sql` sitting in the
            // backups directory would be a lie.
            let _ = std::fs::remove_file(&dump_path);
            return Err(error)
                .with_context(|| format!("failed to spawn the database dump for {}", p.id));
        }
    };

    match settle(child, "database dump", log, cancel).await? {
        StepOutcome::Ran(true) => {}
        StepOutcome::Ran(false) => {
            let _ = writeln!(log, "--- database dump failed ---");
            // A failed dump is a partial file; letting a later restore replay
            // half a schema is worse than having nothing.
            let _ = std::fs::remove_file(&dump_path);
            bail!("database dump failed; the run log has the dump tool's output");
        }
        StepOutcome::Cancelled => {
            let _ = std::fs::remove_file(&dump_path);
            bail!("database dump cancelled");
        }
    }

    let size = dump_path.metadata().map(|m| m.len()).unwrap_or(0);
    writeln!(log, "--- database dump done ({size} bytes) ---")?;

    let entry = Backup {
        file: file_name,
        created_at: util::now_iso(),
        before_run: before_run.to_string(),
        kind: db.kind.as_str().to_string(),
        label: label.to_string(),
        size,
    };
    // A manifest failure fails the dump — a deploy must not proceed on a dump
    // no rollback will ever find — but the dump file itself is kept: it holds
    // good data, and deleting it would not fix the manifest.
    append_backup(&p.id, entry.clone())?;
    prune(&p.id, p.database.keep);
    Ok(entry)
}

/// Restore a dump into the database, replacing what is there.
///
/// Resets first (Postgres: drop/recreate the public schema; MySQL/MariaDB:
/// drop/recreate the database) so tables added by the bad deploy do not
/// survive, then pipes the dump in through the container's own client.
/// Callers take a `pre-rollback` safety dump first — restoring is the one
/// destructive step in a rollback.
pub async fn restore(
    p: &ProjectConfig,
    db: &DbInfo,
    backup: &Backup,
    log: &mut File,
    cancel: &Notify,
) -> Result<()> {
    // Dump formats are engine-specific down to the quoting, so a cross-kind
    // restore would fail half-applied at best. Refuse it before anything runs.
    if backup.kind != db.kind.as_str() {
        bail!(
            "backup of {} cannot be restored into {}",
            backup.kind,
            db.kind.as_str()
        );
    }
    // Checked before the reset: dropping the live schema with no dump to pipe
    // in afterwards would destroy the data twice over.
    let dump_path = backups_dir(&p.id).join(&backup.file);
    if !dump_path.is_file() {
        bail!("dump file {} does not exist", dump_path.display());
    }

    // The MySQL dump command falls back to `--all-databases` exactly when the
    // container names no MYSQL_DATABASE, so that environment — same compose
    // file, same env — is how an all-databases dump is recognized here, and
    // refused: replaying one over a live server is out of scope for v1.
    if matches!(db.kind, DbKind::Mysql | DbKind::Mariadb) {
        match exec_step(
            p,
            &db.service,
            r#"test -n "$MYSQL_DATABASE""#,
            "database scope check",
            None,
            log,
            cancel,
        )
        .await?
        {
            StepOutcome::Ran(true) => {}
            StepOutcome::Ran(false) => bail!(
                "this dump was taken with --all-databases; restoring an all-databases \
                 dump over a live database server is out of scope — restore it by hand"
            ),
            StepOutcome::Cancelled => bail!("database restore cancelled"),
        }
    }

    match exec_step(
        p,
        &db.service,
        reset_command(db.kind),
        "database reset",
        None,
        log,
        cancel,
    )
    .await?
    {
        StepOutcome::Ran(true) => {}
        StepOutcome::Ran(false) => bail!("database reset failed; the run log has the output"),
        StepOutcome::Cancelled => bail!("database restore cancelled"),
    }

    // The dump file is the child's stdin, so the container's own client
    // streams it straight from disk — webhookr never buffers it in memory.
    let dump = File::open(&dump_path)
        .with_context(|| format!("failed to open {}", dump_path.display()))?;
    match exec_step(
        p,
        &db.service,
        restore_command(db.kind),
        "database restore",
        Some(dump),
        log,
        cancel,
    )
    .await?
    {
        StepOutcome::Ran(true) => Ok(()),
        StepOutcome::Ran(false) => bail!("database restore failed; the run log has the output"),
        StepOutcome::Cancelled => bail!("database restore cancelled"),
    }
}

/// Drop the oldest dumps past `keep`, both files and manifest rows.
pub fn prune(project_id: &str, keep: usize) {
    let _guard = INDEX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = backups_dir(project_id);
    let mut index = load_index(project_id);
    if index.len() <= keep {
        return;
    }
    // The manifest is newest-first, so everything past the first `keep` rows
    // is by definition the oldest. Individual unlink failures are ignored: a
    // file already gone must not abort the sweep, and the manifest written
    // below stays the source of truth for what exists.
    let doomed = index.split_off(keep);
    for backup in &doomed {
        let _ = std::fs::remove_file(dir.join(&backup.file));
    }
    if let Err(error) = save_index(project_id, &mut index) {
        eprintln!("webhookr: could not prune the backup manifest for {project_id}: {error:#}");
    }
}

/// What happened to one dump/restore step.
enum StepOutcome {
    /// The command ran to completion; the flag is its exit status.
    Ran(bool),
    /// The run's cancel signal fired; the child was killed and reaped.
    Cancelled,
}

/// Await a spawned dump/restore child in a `select!` against the run's cancel
/// signal — the tail of executor's `exec`, factored out because the dump path
/// wires the child's stdout to a file while every other step logs both
/// streams. A cancel kills and reaps the child (no zombies) and reports
/// [`StepOutcome::Cancelled`]; `kill_on_drop` on the spawn is the backstop,
/// this path is the reaper.
async fn settle(
    mut child: tokio::process::Child,
    label: &str,
    log: &mut File,
    cancel: &Notify,
) -> Result<StepOutcome> {
    let status = tokio::select! {
        biased;
        // A stored permit from a cancel issued before this step began
        // awaiting fires immediately (Notify semantics), so the gap between
        // tripping the signal and the child dying stays bounded.
        () = cancel.notified() => {
            let _ = child.kill().await;
            // Reap the killed child so it does not linger as a zombie.
            let _ = child.wait().await;
            let _ = writeln!(log, "--- {label} cancelled ---");
            return Ok(StepOutcome::Cancelled);
        }
        status = child.wait() => status,
    };
    Ok(StepOutcome::Ran(
        status
            .with_context(|| format!("failed to reap {label}"))?
            .success(),
    ))
}

/// Run one `docker compose exec` step inside the database's service with both
/// output streams going to the run log — the workhorse of the restore path
/// (and of the MySQL scope probe). Cancel-aware through [`settle`].
async fn exec_step(
    p: &ProjectConfig,
    service: &str,
    inner: &str,
    label: &str,
    stdin: Option<File>,
    log: &mut File,
    cancel: &Notify,
) -> Result<StepOutcome> {
    writeln!(log, "--- {label} ---")?;
    let stdout = log
        .try_clone()
        .with_context(|| format!("failed to attach the run log to {label}"))?;
    let stderr = log
        .try_clone()
        .with_context(|| format!("failed to attach the run log to {label}"))?;

    let mut command = Command::new("docker");
    command
        .args(compose_exec_args(p, service, inner))
        .current_dir(&p.path)
        .stdin(match stdin {
            // Restore pipes the dump file in through here.
            Some(file) => Stdio::from(file),
            None => Stdio::null(),
        })
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    let child = command
        .spawn()
        .with_context(|| format!("failed to spawn {label}"))?;
    settle(child, label, log, cancel).await
}

/// The argv that reaches the database's container: the compose base built
/// exactly the way `executor::deploy` builds its own (`-f` plus profiles, so
/// `env_file`, variables and `extends` resolve at backup time precisely as
/// they did at deploy time), then `exec -T <service> sh -c <inner>`.
///
/// `-T` matters twice over: it keeps compose from allocating a TTY, which
/// would mangle a piped dump with CRs and echo, and it is what lets stdin
/// flow from a redirected file during restore.
fn compose_exec_args(p: &ProjectConfig, service: &str, inner: &str) -> Vec<String> {
    let mut args = vec![
        "compose".to_string(),
        "-f".to_string(),
        p.compose_file.clone(),
    ];
    for profile in &p.compose_profiles {
        args.extend(["--profile".to_string(), profile.clone()]);
    }
    args.extend([
        "exec".to_string(),
        "-T".to_string(),
        service.to_string(),
        "sh".to_string(),
        "-c".to_string(),
        inner.to_string(),
    ]);
    args
}

/// The shell run *inside* the container to write a dump to stdout.
///
/// One shell string handed to the container's `sh -c`, so credentials resolve
/// container-side: `$POSTGRES_USER`, `$MYSQL_ROOT_PASSWORD` and friends are
/// expanded by the database's own shell and the values never reach this
/// process, the host's process list, or the run log.
/// `${POSTGRES_DB:+"--dbname=$POSTGRES_DB"}` names the database only where
/// the image set one (else the connection user's default database is dumped);
/// the MySQL guard picks the per-database dump when the image named a
/// database and `--all-databases` otherwise.
fn dump_command(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Postgres => {
            r#"pg_dump -U "${POSTGRES_USER:-postgres}" ${POSTGRES_DB:+"--dbname=$POSTGRES_DB"}"#
        }
        DbKind::Mysql | DbKind::Mariadb => concat!(
            r#"[ -n "$MYSQL_DATABASE" ] && "#,
            r#"mysqldump -u root -p"$MYSQL_ROOT_PASSWORD" "$MYSQL_DATABASE" || "#,
            r#"mysqldump -u root -p"$MYSQL_ROOT_PASSWORD" --all-databases"#
        ),
    }
}

/// The shell run *inside* the container to empty the database before a
/// restore, so tables the bad deploy added cannot survive beside the dump.
/// Same credential discipline as [`dump_command`]. The MySQL half assumes the
/// scope probe in [`restore`] already proved `$MYSQL_DATABASE` is set — the
/// backticks keep a dotted database name one argument.
fn reset_command(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Postgres => concat!(
            r#"psql -U "${POSTGRES_USER:-postgres}" ${POSTGRES_DB:+"--dbname=$POSTGRES_DB"} "#,
            r#"-c "DROP SCHEMA public CASCADE; CREATE SCHEMA public;""#
        ),
        DbKind::Mysql | DbKind::Mariadb => concat!(
            r#"mysql -u root -p"$MYSQL_ROOT_PASSWORD" -e "DROP DATABASE IF EXISTS "#,
            r#"\`$MYSQL_DATABASE\`; CREATE DATABASE \`$MYSQL_DATABASE\`;""#
        ),
    }
}

/// The shell run *inside* the container to eat a dump from stdin — the same
/// clients (and credential discipline) as [`reset_command`], minus the reset.
fn restore_command(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Postgres => {
            r#"psql -U "${POSTGRES_USER:-postgres}" ${POSTGRES_DB:+"--dbname=$POSTGRES_DB"}"#
        }
        DbKind::Mysql | DbKind::Mariadb => {
            r#"mysql -u root -p"$MYSQL_ROOT_PASSWORD" "$MYSQL_DATABASE""#
        }
    }
}

/// Filesystem-safe dump name: `<run-or-timestamp>_<label>.sql`.
///
/// Dumps taken inside a run are keyed by the run id (stable, and the id a
/// rollback already knows); ones taken outside a run fall back to the
/// timestamp, whose `:` separators are folded to `-` because they are illegal
/// on several filesystems. Everything is sanitized — a name is the one part
/// of a dump that never gets rewritten.
fn dump_file_name(before_run: &str, label: &str) -> String {
    let stem = if before_run.is_empty() {
        util::now_iso().replace(':', "-")
    } else {
        before_run.to_string()
    };
    format!("{}_{}.sql", file_safe(&stem), file_safe(label))
}

/// Replace anything a filename should not carry with `-`.
fn file_safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Record a dump in the project's manifest, keeping it newest-first.
///
/// The whole load-modify-save runs under [`INDEX_LOCK`], so concurrent dumps
/// for the same project cannot lose each other's rows.
fn append_backup(project_id: &str, backup: Backup) -> Result<()> {
    let _guard = INDEX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut index = load_index(project_id);
    index.push(backup);
    save_index(project_id, &mut index)
}

/// Persist the manifest atomically, sorted newest first.
///
/// Callers must already hold [`INDEX_LOCK`]: `std::sync::Mutex` is not
/// reentrant, so taking it here would deadlock the read-modify-write paths.
fn save_index(project_id: &str, index: &mut Vec<Backup>) -> Result<()> {
    index.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let text = serde_json::to_string_pretty(index)?;
    config::write_atomic(&backups_dir(project_id).join(INDEX_FILE), &text, false)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{backups_dir, detect_service, load_index, Backup, DbKind};
    use crate::config::ProjectConfig;

    fn backup(file: &str, created_at: &str, before_run: &str) -> Backup {
        Backup {
            file: file.into(),
            created_at: created_at.into(),
            before_run: before_run.into(),
            kind: "postgres".into(),
            label: "pre-deploy".into(),
            size: 1024,
        }
    }

    /// The shape `docker compose config --format json` prints today: the
    /// whole resolved config with the services under a `services` key.
    fn full_config(services: &str) -> String {
        format!(r#"{{"name":"app","services":{services}}}"#)
    }

    #[test]
    fn detects_a_tagged_postgres_image() {
        assert_eq!(
            detect_service(&full_config(
                r#"{"db":{"image":"postgres:16","restart":"always"}}"#
            )),
            Some((DbKind::Postgres, "db".to_string()))
        );
        // Untagged images appear when a compose file relies on a default tag.
        assert_eq!(
            detect_service(&full_config(r#"{"db":{"image":"postgres"}}"#)),
            Some((DbKind::Postgres, "db".to_string()))
        );
    }

    #[test]
    fn detects_postgis_as_postgres() {
        // postgis rides the `postgis/postgis` hub namespace.
        assert_eq!(
            detect_service(&full_config(r#"{"db":{"image":"postgis/postgis:16-3.4"}}"#)),
            Some((DbKind::Postgres, "db".to_string()))
        );
    }

    #[test]
    fn detects_mysql_and_mariadb() {
        assert_eq!(
            detect_service(&full_config(r#"{"db":{"image":"mysql:8.0"}}"#)),
            Some((DbKind::Mysql, "db".to_string()))
        );
        assert_eq!(
            detect_service(&full_config(r#"{"db":{"image":"mariadb:11"}}"#)),
            Some((DbKind::Mariadb, "db".to_string()))
        );
    }

    #[test]
    fn detects_through_a_registry_prefix_and_port() {
        // The registry port must not be mistaken for a tag separator.
        assert_eq!(
            detect_service(&full_config(
                r#"{"db":{"image":"registry.example.com:5000/team/postgres:16"}}"#
            )),
            Some((DbKind::Postgres, "db".to_string()))
        );
    }

    #[test]
    fn finds_no_database_in_a_plain_service_stack() {
        // Redis is deliberately unmatched: v1 has no dump/restore for it, so
        // a redis-only project's rollback restores code rather than
        // pretending the data is covered.
        let config = full_config(r#"{"web":{"image":"nginx:1.27"},"cache":{"image":"redis:7"}}"#);
        assert_eq!(detect_service(&config), None);
    }

    #[test]
    fn a_service_without_an_image_is_skipped() {
        assert_eq!(
            detect_service(&full_config(r#"{"web":{"build":{"context":"."}}}"#)),
            None
        );
        // Unparseable compose output is a detection failure, not a panic.
        assert_eq!(detect_service("not json"), None);
    }

    #[test]
    fn prefers_the_service_literally_named_db() {
        // `analytics` comes first in the map, so this only passes because of
        // the name preference.
        let config =
            full_config(r#"{"analytics":{"image":"mysql:8"},"db":{"image":"postgres:16"}}"#);
        assert_eq!(
            detect_service(&config),
            Some((DbKind::Postgres, "db".to_string()))
        );
    }

    #[test]
    fn prefers_the_service_literally_named_database() {
        let config =
            full_config(r#"{"app-db":{"image":"mysql:8"},"database":{"image":"mariadb:11"}}"#);
        assert_eq!(
            detect_service(&config),
            Some((DbKind::Mariadb, "database".to_string()))
        );
    }

    #[test]
    fn reads_the_bare_services_map_shape() {
        // Older docker releases printed the services map without the wrapping
        // config object; both shapes must answer.
        assert_eq!(
            detect_service(r#"{"db":{"image":"postgres:16"}}"#),
            Some((DbKind::Postgres, "db".to_string()))
        );
    }

    #[tokio::test]
    async fn manual_and_off_modes_resolve_without_docker() {
        let mut p = ProjectConfig::new(
            "site".into(),
            "Site".into(),
            "/tmp".into(),
            "main".into(),
            "true".into(),
            "secret".into(),
            "github".into(),
        );
        // Custom preset: `auto` has no compose file to probe, so no database.
        p.database.mode = "auto".into();
        assert!(super::database_for(&p).await.is_none());

        p.database.mode = "off".into();
        assert!(super::database_for(&p).await.is_none());

        p.database.mode = "manual".into();
        p.database.kind = "mariadb".into();
        p.database.service = "mydb".into();
        let db = super::database_for(&p)
            .await
            .expect("manual is trusted verbatim");
        assert_eq!(db.kind, DbKind::Mariadb);
        assert_eq!(db.service, "mydb");
    }

    #[test]
    fn dump_names_are_filesystem_safe() {
        assert_eq!(
            super::dump_file_name("run-1", "pre-deploy"),
            "run-1_pre-deploy.sql"
        );
        // Outside a run the timestamp stands in, minus its colons.
        let stamp = super::dump_file_name("", "pre-rollback");
        assert!(stamp.ends_with("_pre-rollback.sql"), "{stamp}");
        assert!(!stamp.contains(':'), "colons are not portable: {stamp}");
        // Hostile input can at worst produce dashes, never a path.
        assert_eq!(
            super::dump_file_name("../etc/passwd", "a b/c"),
            "..-etc-passwd_a-b-c.sql"
        );
    }

    /// Round-trips the manifest through a temp state dir: append, newest-first
    /// load, run lookup, prune, and the corrupt-file policy.
    ///
    /// `config::state_dir()` can only be redirected through the process-wide
    /// `WEBHOOKR_STATE_DIR`, so this test takes the shared lock that every
    /// env-redirecting test holds — without it another such test running in
    /// parallel under the default harness would interleave with the set/unset
    /// here — and the guard below unsets the variable even when an assertion
    /// fails.
    #[test]
    fn manifest_round_trip_prune_and_run_lookup() {
        let _state_dir = crate::util::STATE_DIR_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        struct UnsetStateDir;
        impl Drop for UnsetStateDir {
            fn drop(&mut self) {
                std::env::remove_var("WEBHOOKR_STATE_DIR");
            }
        }

        let root =
            std::env::temp_dir().join(format!("webhookr-backup-{}", crate::util::new_run_id()));
        std::env::set_var("WEBHOOKR_STATE_DIR", &root);
        let _unset = UnsetStateDir;

        // A project that has never been dumped has no manifest at all.
        assert!(load_index("site").is_empty());

        let old = backup("run-1_pre-deploy.sql", "2026-09-01T00:00:00Z", "run-1");
        let mid = backup("run-2_pre-deploy.sql", "2026-09-02T00:00:00Z", "run-2");
        let outside = backup(
            "2026-09-03T00-00-00Z_pre-rollback.sql",
            "2026-09-03T00:00:00Z",
            "",
        );
        // Appended oldest first, to prove the sort (not insertion order) is
        // what answers.
        super::append_backup("site", old.clone()).unwrap();
        super::append_backup("site", mid.clone()).unwrap();
        super::append_backup("site", outside.clone()).unwrap();

        let index = load_index("site");
        assert_eq!(
            index.iter().map(|b| b.file.as_str()).collect::<Vec<_>>(),
            vec![outside.file.as_str(), mid.file.as_str(), old.file.as_str()],
            "the manifest must answer newest first"
        );

        // The lookup a rollback makes: the dump taken immediately before the
        // run being undone.
        assert_eq!(
            super::backup_before_run("site", "run-2").map(|b| b.file),
            Some(mid.file.clone())
        );
        assert!(super::backup_before_run("site", "run-9").is_none());
        assert!(super::backup_before_run("other", "run-1").is_none());

        // Prune keeps the newest `keep` rows and deletes their files too.
        let dir = backups_dir("site");
        for entry in [&old, &mid, &outside] {
            fs::write(dir.join(&entry.file), "-- dump").unwrap();
        }
        super::prune("site", 2);
        let index = load_index("site");
        assert_eq!(index.len(), 2, "keep = 2 of 3");
        assert!(!index.iter().any(|b| b.file == old.file));
        assert!(
            !dir.join(&old.file).exists(),
            "the pruned row's file must go with it"
        );
        assert!(dir.join(&mid.file).exists() && dir.join(&outside.file).exists());

        // A corrupt manifest reads as empty but is left in place — deleting
        // the only record of which dumps exist would orphan them.
        fs::write(dir.join(super::INDEX_FILE), "not json").unwrap();
        assert!(load_index("site").is_empty());
        assert!(dir.join(super::INDEX_FILE).exists());

        let _ = fs::remove_dir_all(root);
    }

    /// Whether a container runtime answers, so docker-dependent tests can skip
    /// (with a note) instead of failing on machines without one.
    async fn docker_available() -> bool {
        tokio::process::Command::new("docker")
            .arg("info")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// Run `docker compose <args>` inside the stack's directory. Compose
    /// derives its project name from the directory, which is unique per test
    /// run, so `down -v` cleans up exactly this stack.
    async fn compose(root: &std::path::Path, args: &[&str]) -> String {
        let output = tokio::process::Command::new("docker")
            .arg("compose")
            .args(args)
            .current_dir(root)
            .output()
            .await
            .expect("docker compose runs");
        assert!(
            output.status.success(),
            "docker compose {:?} failed:\n{}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// One SQL statement through the stack's psql, `-t -A` so the result is
    /// bare rows — assertable without parsing table borders.
    async fn psql(root: &std::path::Path, sql: &str) -> String {
        let output = tokio::process::Command::new("docker")
            .args([
                "compose", "exec", "-T", "db", "psql", "-U", "app", "-d", "appdb", "-t", "-A",
                "-c", sql,
            ])
            .current_dir(root)
            .output()
            .await
            .expect("psql runs");
        assert!(
            output.status.success(),
            "psql {sql} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// The rollback data contract, end to end against a real Postgres: dump
    /// the good state, let a "bad deploy" mutate the database (new rows and a
    /// whole new table), restore, and the good state is back — the bad rows
    /// gone and the junk table dropped by the schema reset. Skips, with a
    /// note, when no docker is available: a machine without a container
    /// runtime cannot run it, and CI always can.
    #[tokio::test]
    // The std guard spans the test's awaits on purpose: it must cover the
    // whole WEBHOOKR_STATE_DIR set/unset window, and nothing on this thread
    // re-locks it.
    #[allow(clippy::await_holding_lock)]
    async fn dump_and_restore_round_trip_a_real_postgres() {
        if !docker_available().await {
            eprintln!("skipping: docker is not available on this machine");
            return;
        }
        let _state_dir = crate::util::STATE_DIR_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        struct UnsetStateDir;
        impl Drop for UnsetStateDir {
            fn drop(&mut self) {
                std::env::remove_var("WEBHOOKR_STATE_DIR");
            }
        }

        let root =
            std::env::temp_dir().join(format!("webhookr-dbround-{}", crate::util::new_run_id()));
        fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEBHOOKR_STATE_DIR", &root);
        let _unset = UnsetStateDir;

        // A minimal stack shaped like a real compose project: the database is
        // reached only through its service, exactly as dump/restore do it.
        fs::write(
            root.join("compose.yaml"),
            "services:\n  db:\n    image: postgres:16-alpine\n    environment:\n      \
             POSTGRES_USER: app\n      POSTGRES_PASSWORD: round-trip-only\n      \
             POSTGRES_DB: appdb\n",
        )
        .unwrap();

        let mut project = ProjectConfig::new(
            "dbsite".into(),
            "DB Site".into(),
            root.to_string_lossy().into_owned(),
            "main".into(),
            "true".into(),
            "secret".into(),
            "github".into(),
        );
        project.deploy_preset = "compose_up".into();
        project.compose_file = "compose.yaml".into();
        project.database.mode = "manual".into();
        project.database.kind = "postgres".into();
        project.database.service = "db".into();
        project.database.backups = true;
        project.database.keep = 3;

        compose(&root, &["up", "-d", "--quiet-pull"]).await;
        // Postgres takes a moment to accept connections after the container
        // reports up; pg_isready inside the service is the authoritative
        // probe, polled so a slow first-time image pull cannot flake it.
        let mut ready = false;
        for _ in 0..90 {
            let ok = tokio::process::Command::new("docker")
                .args(["compose", "exec", "-T", "db", "pg_isready", "-U", "app"])
                .current_dir(&root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await
                .map(|status| status.success())
                .unwrap_or(false);
            if ok {
                ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        assert!(ready, "postgres never became ready");

        // The "previously good deploy" the dump must capture.
        psql(
            &root,
            "CREATE TABLE items (name text); INSERT INTO items VALUES ('good-state');",
        )
        .await;

        let db = super::database_for(&project)
            .await
            .expect("manual mode resolves without probing");
        assert_eq!(db.kind, DbKind::Postgres);
        assert_eq!(db.service, "db");

        let log_path = root.join("roundtrip.log");
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        let cancel = tokio::sync::Notify::new();

        let backup = super::dump(&project, &db, "run-1", "pre-deploy", &mut log, &cancel)
            .await
            .expect("dump succeeds");
        let dump_text = fs::read_to_string(backups_dir("dbsite").join(&backup.file)).unwrap();
        assert!(
            dump_text.contains("good-state"),
            "the dump must carry the good state's data"
        );
        assert!(
            super::backup_before_run("dbsite", "run-1").is_some(),
            "the manifest must answer the lookup a rollback makes"
        );

        // The "bad deploy": rows the rollback must remove, and a table the
        // schema reset must drop.
        psql(
            &root,
            "INSERT INTO items VALUES ('bad-state'); CREATE TABLE migration_junk (x int);",
        )
        .await;
        assert_eq!(
            psql(&root, "SELECT count(*) FROM items;").await.trim(),
            "2",
            "precondition: the bad state really landed"
        );

        super::restore(&project, &db, &backup, &mut log, &cancel)
            .await
            .expect("restore succeeds");

        assert_eq!(
            psql(&root, "SELECT name FROM items ORDER BY name;").await,
            "good-state\n",
            "the restore must bring back exactly the dumped state"
        );
        assert_eq!(
            psql(
                &root,
                "SELECT to_regclass('public.migration_junk') IS NULL;"
            )
            .await
            .trim(),
            "t",
            "the schema reset must have dropped the bad deploy's table"
        );

        compose(&root, &["down", "-v", "--remove-orphans"]).await;
        let _ = fs::remove_dir_all(root);
    }
}
