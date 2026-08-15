//! Configuration model and persistence for webhookr.
//!
//! The whole app is driven by a single JSON file (`config.json`). Both the
//! daemon (`webhookr serve`) and the management UI (`webhookr`, `webhookr add`,
//! ...) read and write this file, so edits made in the TUI/CLI are picked up by
//! the daemon on the next webhook.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Serializes in-process config mutations. Held only across synchronous file
/// I/O, never across an `.await`.
static CONFIG_LOCK: Mutex<()> = Mutex::new(());

/// Directory that holds the config file.
pub fn config_dir() -> PathBuf {
    if let Ok(p) = std::env::var("WEBHOOKR_CONFIG_DIR") {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("webhookr")
}

/// Path to the JSON config file.
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("WEBHOOKR_CONFIG") {
        return PathBuf::from(p);
    }
    config_dir().join("config.json")
}

/// Owner-only credential file used by the Cloudflare Tunnel connector.
pub fn cloudflare_credentials_path() -> PathBuf {
    config_dir().join("cloudflare-credentials.json")
}

/// Directory for runtime state (run history) and logs.
pub fn state_dir() -> PathBuf {
    if let Ok(p) = std::env::var("WEBHOOKR_STATE_DIR") {
        return PathBuf::from(p);
    }
    dirs::data_local_dir()
        .or_else(dirs::config_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("webhookr")
}

/// Directory where per-run logs are written.
pub fn log_dir() -> PathBuf {
    state_dir().join("logs")
}

/// Deployment presets: `(id, label, description)`. Single source of truth for
/// the CLI, the TUI radio list, and the web form.
pub const PRESETS: [(&str, &str, &str); 4] = [
    (
        "compose_build",
        "Compose build",
        "docker compose up -d --build --remove-orphans",
    ),
    (
        "compose_pull",
        "Compose pull",
        "pull images, then compose up in detached mode",
    ),
    (
        "compose_up",
        "Compose up",
        "start the selected Compose file without build or pull",
    ),
    (
        "custom",
        "Custom command",
        "run a shell command after the source is ready",
    ),
];

/// Port portion of a `host:port` listen address, defaulting to 9000.
pub fn listen_port(address: &str) -> u16 {
    address
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(9000)
}

/// Public webhook URL for a project.
///
/// Prefers a dedicated webhook hostname when one is configured. Otherwise the
/// admin hostname doubles as the webhook host, since the admin port serves
/// `/webhook/{id}` alongside the dashboard — that is the single-hostname setup.
pub fn webhook_url(config: &AppConfig, project_id: &str) -> String {
    if let Some(tunnel) = &config.cloudflare {
        if !tunnel.hostname.trim().is_empty() {
            return format!("https://{}/hooks/{project_id}", tunnel.hostname);
        }
        if let Some(admin) = tunnel.admin_hostname.as_deref() {
            return format!("https://{admin}/webhook/{project_id}");
        }
    }
    if config.web.enabled {
        format!("http://{}/webhook/{project_id}", config.web.listen_addr)
    } else {
        format!("http://{}/hooks/{project_id}", config.listen_addr)
    }
}

/// The exact shell command a deployment will run, for display.
///
/// Built from the same fields the executor uses, so what the UI shows is what
/// actually runs.
pub fn deploy_command_preview(project: &ProjectConfig) -> String {
    if !project.uses_compose() {
        return project.command.clone();
    }

    let mut base = format!("docker compose -f {}", project.compose_file);
    for profile in &project.compose_profiles {
        base.push_str(&format!(" --profile {profile}"));
    }

    match project.deploy_preset.as_str() {
        "compose_pull" => format!("{base} pull\n{base} up -d --remove-orphans"),
        "compose_build" => format!("{base} up -d --build --remove-orphans"),
        _ => format!("{base} up -d --remove-orphans"),
    }
}

/// One webhook-triggered deploy target.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectConfig {
    /// URL slug; the endpoint is `POST /hooks/{id}`.
    pub id: String,
    /// Human-friendly display name.
    pub name: String,
    /// Absolute path to the git checkout on disk.
    pub path: String,
    /// Git branch to check out / pull before running the command.
    pub branch: String,
    /// Shell command to run after a successful pull.
    pub command: String,
    /// Secret used to authenticate incoming webhooks.
    pub secret: String,
    /// `github` (verify `X-Hub-Signature-256`) or `token` (verify `X-Webhookr-Key`).
    pub verify_mode: String,
    /// Git remote cloned when `path` does not exist. Empty keeps legacy local-checkout behavior.
    #[serde(default)]
    pub repository: String,
    /// Personal access token for private repositories.
    ///
    /// Passed to git through a credential helper reading an environment
    /// variable, so it never appears in the process list and is never written
    /// into the checkout's `.git/config`.
    #[serde(default)]
    pub git_token: String,
    /// Deployment preset: `custom`, `compose_build`, `compose_pull`, or `compose_up`.
    #[serde(default = "default_deploy_preset")]
    pub deploy_preset: String,
    /// Compose file relative to the project checkout.
    #[serde(default = "default_compose_file")]
    pub compose_file: String,
    /// Optional Compose profiles enabled during deployment.
    #[serde(default)]
    pub compose_profiles: Vec<String>,
}

impl ProjectConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        name: String,
        path: String,
        branch: String,
        command: String,
        secret: String,
        verify_mode: String,
    ) -> Self {
        Self {
            id,
            name,
            path,
            branch,
            command,
            secret,
            verify_mode,
            repository: String::new(),
            git_token: String::new(),
            deploy_preset: default_deploy_preset(),
            compose_file: default_compose_file(),
            compose_profiles: Vec::new(),
        }
    }

    /// Basic sanity checks before a project can be used.
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || !self
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("project id must be a non-empty slug (a-z, 0-9, '-', '_')");
        }
        if self.name.is_empty() {
            bail!("project name is required");
        }
        if self.path.is_empty() {
            bail!("project path is required");
        }
        if !Path::new(&self.path).exists() && self.repository.trim().is_empty() {
            bail!("project path does not exist: {}", self.path);
        }
        if self.secret.is_empty() {
            bail!("project secret must not be empty");
        }
        // `branch` and `repository` are passed to git positionally, so a value
        // starting with '-' would be read as a flag (e.g. --upload-pack=...).
        //
        // An empty branch is deliberately *not* rejected here: it is broken for
        // anything that pulls, and git says so plainly, but a custom-command
        // project that only ever redeploys works fine without one. Failing it
        // in validate would break such a config on upgrade for no security gain.
        if self.branch.starts_with('-') {
            bail!("branch may not start with '-'");
        }
        if self.repository.starts_with('-') {
            bail!("repository URL may not start with '-'");
        }
        if self.verify_mode != "github" && self.verify_mode != "token" {
            bail!("verify_mode must be 'github' or 'token'");
        }
        match self.deploy_preset.as_str() {
            "custom" => {
                if self.command.trim().is_empty() {
                    bail!("custom deployment command is required");
                }
            }
            "compose_build" | "compose_pull" | "compose_up" => {
                validate_relative_file(&self.compose_file)?;
            }
            _ => bail!(
                "deploy_preset must be 'custom', 'compose_build', 'compose_pull', or 'compose_up'"
            ),
        }
        for profile in &self.compose_profiles {
            if profile.trim().is_empty() || profile.starts_with('-') {
                bail!("compose profiles must be non-empty names and may not start with '-'");
            }
        }
        Ok(())
    }

    pub fn uses_compose(&self) -> bool {
        self.deploy_preset.starts_with("compose_")
    }

    pub fn preset_label(&self) -> &'static str {
        match self.deploy_preset.as_str() {
            "compose_build" => "Compose build",
            "compose_pull" => "Compose pull",
            "compose_up" => "Compose up",
            _ => "Custom command",
        }
    }
}

fn default_deploy_preset() -> String {
    "custom".to_string()
}

fn default_compose_file() -> String {
    "compose.yaml".to_string()
}

fn validate_relative_file(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.trim().is_empty() || path.is_absolute() {
        bail!("compose file must be a relative path inside the project");
    }
    if path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        bail!("compose file may not leave the project directory");
    }
    Ok(())
}

/// Remotely-managed Cloudflare Tunnel attached to the webhook listener.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CloudflareConfig {
    /// Public hostname for the webhook listener.
    pub hostname: String,
    pub account_id: String,
    /// Zone of [`Self::hostname`].
    pub zone_id: String,
    pub tunnel_id: String,
    pub tunnel_name: String,
    /// Public hostname routed to the admin UI port, when one is configured.
    /// The admin UI needs its own hostname: putting Access on the webhook
    /// hostname would break GitHub, which cannot complete an Access login.
    #[serde(default)]
    pub admin_hostname: Option<String>,
    /// Zone of [`Self::admin_hostname`] when it differs from [`Self::zone_id`].
    #[serde(default)]
    pub admin_zone_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudflareCredentials {
    pub tunnel_token: String,
}

/// Browser admin UI.
///
/// Disabled unless explicitly turned on: the UI has no login of its own, so an
/// upgrade or a stray config edit must never bring it up by accident.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebConfig {
    /// Master switch.
    #[serde(default)]
    pub enabled: bool,
    /// Bind address. Loopback by default — `cloudflared` connects from the same
    /// host, so the tunnel still works while the port stays off the LAN and off
    /// the public interface, where it would bypass Cloudflare Access entirely.
    #[serde(default = "default_web_addr")]
    pub listen_addr: String,
    /// Public hostname routed to the admin port through `cloudflared`.
    #[serde(default)]
    pub hostname: Option<String>,
    /// Reject requests without a `Cf-Access-Jwt-Assertion` header. A presence
    /// check only — it does not validate the JWT — so it is defence in depth
    /// behind Access, never a substitute for it.
    #[serde(default)]
    pub require_access_header: bool,
}

fn default_web_addr() -> String {
    "127.0.0.1:9001".to_string()
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_addr: default_web_addr(),
            hostname: None,
            require_access_header: false,
        }
    }
}

impl WebConfig {
    /// Check the admin listener can coexist with the webhook listener.
    pub fn validate(&self, webhook_addr: &str) -> Result<()> {
        if self.listen_addr.trim().is_empty() {
            bail!("web listen address is required");
        }
        if self.listen_addr.rsplit_once(':').is_none() {
            bail!(
                "web listen address must be host:port, got {}",
                self.listen_addr
            );
        }
        if listen_port(&self.listen_addr) == listen_port(webhook_addr) {
            bail!(
                "web admin UI port {} collides with the webhook listener; pick another",
                listen_port(&self.listen_addr)
            );
        }
        Ok(())
    }
}

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Bind address for the HTTP server.
    pub listen_addr: String,
    /// Configured projects.
    pub projects: Vec<ProjectConfig>,
    /// Public hostname routed to the local listener through `cloudflared`.
    #[serde(default)]
    pub cloudflare: Option<CloudflareConfig>,
    /// Browser admin UI settings.
    #[serde(default)]
    pub web: WebConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        let port = std::env::var("WEBHOOKR_PORT").unwrap_or_else(|_| "9000".to_string());
        Self {
            listen_addr: format!("0.0.0.0:{port}"),
            projects: Vec::new(),
            cloudflare: None,
            web: WebConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn get(&self, id: &str) -> Option<&ProjectConfig> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut ProjectConfig> {
        self.projects.iter_mut().find(|p| p.id == id)
    }

    /// Insert or replace a project keyed by `id`.
    pub fn upsert(&mut self, p: ProjectConfig) {
        match self.projects.iter_mut().find(|x| x.id == p.id) {
            Some(existing) => *existing = p,
            None => self.projects.push(p),
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<ProjectConfig> {
        match self.projects.iter().position(|x| x.id == id) {
            Some(i) => Some(self.projects.remove(i)),
            None => None,
        }
    }
}

/// Load config from disk, returning defaults when the file doesn't exist yet.
pub fn load_config() -> Result<AppConfig> {
    let path = config_path();
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let cfg: AppConfig = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse config at {}", path.display()))?;
    Ok(cfg)
}

/// Write `text` to `path` atomically: fill a temp file in the same directory,
/// fsync it, then rename over the target. A concurrent reader therefore sees
/// either the old file or the new one, never a truncated one.
///
/// When `owner_only`, the mode is set on the temp file *before* the rename so
/// the contents are never briefly world-readable.
pub(crate) fn write_atomic(path: &Path, text: &str, owner_only: bool) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));

    let write = |tmp: &Path| -> Result<()> {
        let mut file = fs::File::create(tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to flush {}", tmp.display()))?;
        Ok(())
    };

    if let Err(error) = write(&tmp).and_then(|()| {
        if owner_only {
            set_owner_only(&tmp)?;
        }
        fs::rename(&tmp, path)
            .with_context(|| format!("failed to replace {}", path.display()))
    }) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

/// Persist config to disk. Takes [`CONFIG_LOCK`]; see [`save_config_inner`].
pub fn save_config(cfg: &AppConfig) -> Result<()> {
    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    save_config_inner(cfg)
}

/// Persist config **without** taking [`CONFIG_LOCK`].
///
/// `std::sync::Mutex` is not reentrant, so every caller that already holds the
/// lock (i.e. [`update_config`]) must come through here instead of
/// [`save_config`], or it deadlocks immediately.
fn save_config_inner(cfg: &AppConfig) -> Result<()> {
    let text = serde_json::to_string_pretty(cfg)?;
    // Owner-only: this file stores every project's webhook secret in plaintext.
    write_atomic(&config_path(), &text, true)
}

/// Load, mutate and persist the config while holding [`CONFIG_LOCK`], so two
/// concurrent callers cannot interleave a read-modify-write and lose an edit.
///
/// If `f` returns `Err`, nothing is written — validation failures roll back for
/// free. `f` is synchronous by design: never hold this guard across an `.await`.
pub fn update_config<T>(f: impl FnOnce(&mut AppConfig) -> Result<T>) -> Result<T> {
    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut cfg = load_config()?;
    let out = f(&mut cfg)?;
    save_config_inner(&cfg)?;
    Ok(out)
}

pub fn load_cloudflare_credentials() -> Result<CloudflareCredentials> {
    let path = cloudflare_credentials_path();
    let text = fs::read_to_string(&path).with_context(|| {
        format!(
            "failed to read Cloudflare credentials at {}",
            path.display()
        )
    })?;
    serde_json::from_str(&text).with_context(|| {
        format!(
            "failed to parse Cloudflare credentials at {}",
            path.display()
        )
    })
}

pub fn save_cloudflare_credentials(credentials: &CloudflareCredentials) -> Result<()> {
    let text = serde_json::to_string_pretty(credentials)?;
    write_atomic(&cloudflare_credentials_path(), &text, true)
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure credentials at {}", path.display()))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

/// Ensure the state/log directories exist.
pub fn ensure_dirs() -> Result<()> {
    fs::create_dir_all(log_dir()).with_context(|| "failed to create log dir")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AppConfig, ProjectConfig};

    #[test]
    fn old_config_deserializes_with_deployment_defaults() {
        let json = r#"{
            "listen_addr": "0.0.0.0:9000",
            "projects": [{
                "id": "site",
                "name": "Site",
                "path": "/srv/site",
                "branch": "main",
                "command": "./deploy.sh",
                "secret": "secret",
                "verify_mode": "github"
            }]
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        let project = &config.projects[0];
        assert_eq!(project.deploy_preset, "custom");
        assert_eq!(project.compose_file, "compose.yaml");
        assert!(project.repository.is_empty());
        assert!(config.cloudflare.is_none());
        // A config written before the web UI existed must not switch it on.
        assert!(!config.web.enabled);
        assert_eq!(config.web.listen_addr, "127.0.0.1:9001");
    }

    #[test]
    fn web_ui_is_off_by_default() {
        assert!(!AppConfig::default().web.enabled);
        assert!(!super::WebConfig::default().enabled);
        // A hand-edited partial block still loads, and still binds loopback.
        let partial: AppConfig =
            serde_json::from_str(r#"{"listen_addr":"0.0.0.0:9000","projects":[],"web":{"enabled":true}}"#)
                .unwrap();
        assert!(partial.web.enabled);
        assert_eq!(partial.web.listen_addr, "127.0.0.1:9001");
    }

    #[test]
    fn web_port_may_not_collide_with_the_webhook_port() {
        let collides = super::WebConfig {
            listen_addr: "127.0.0.1:9000".into(),
            ..Default::default()
        };
        assert!(collides.validate("0.0.0.0:9000").is_err());
        assert!(super::WebConfig::default().validate("0.0.0.0:9000").is_ok());
    }

    #[test]
    fn clone_target_may_not_exist_yet() {
        let project = ProjectConfig {
            id: "site".into(),
            name: "Site".into(),
            path: "/tmp/webhookr-does-not-exist".into(),
            branch: "main".into(),
            command: String::new(),
            secret: "secret".into(),
            verify_mode: "github".into(),
            repository: "https://github.com/example/site.git".into(),
            git_token: String::new(),
            deploy_preset: "compose_up".into(),
            compose_file: "deploy/compose.yaml".into(),
            compose_profiles: vec!["web".into()],
        };
        project.validate().unwrap();
    }

    #[test]
    fn git_arguments_cannot_be_read_as_flags() {
        let base = || {
            let mut p = ProjectConfig::new(
                "site".into(),
                "Site".into(),
                "/tmp".into(),
                "main".into(),
                "true".into(),
                "secret".into(),
                "github".into(),
            );
            p.repository = "https://example.com/site.git".into();
            p
        };

        let mut leading_dash_branch = base();
        leading_dash_branch.branch = "--upload-pack=touch /tmp/x".into();
        assert!(leading_dash_branch.validate().is_err());

        // An empty branch stays valid on purpose: rejecting it would break
        // existing custom-command projects that never pull.
        let mut empty_branch = base();
        empty_branch.branch = String::new();
        assert!(empty_branch.validate().is_ok());

        let mut leading_dash_repo = base();
        leading_dash_repo.repository = "--upload-pack=touch /tmp/x".into();
        assert!(leading_dash_repo.validate().is_err());

        assert!(base().validate().is_ok());
    }

    #[test]
    fn compose_file_cannot_escape_checkout() {
        let mut project = ProjectConfig::new(
            "site".into(),
            "Site".into(),
            "/tmp".into(),
            "main".into(),
            String::new(),
            "secret".into(),
            "github".into(),
        );
        project.deploy_preset = "compose_up".into();
        project.compose_file = "../compose.yaml".into();
        assert!(project.validate().is_err());
    }
}
