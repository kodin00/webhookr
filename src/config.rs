//! Configuration model and persistence for webhookr.
//!
//! The whole app is driven by a single JSON file (`config.json`). Both the
//! daemon (`webhookr serve`) and the management UI (`webhookr`, `webhookr add`,
//! ...) read and write this file, so edits made in the TUI/CLI are picked up by
//! the daemon on the next webhook.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

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
    pub hostname: String,
    pub account_id: String,
    pub zone_id: String,
    pub tunnel_id: String,
    pub tunnel_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudflareCredentials {
    pub tunnel_token: String,
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
}

impl Default for AppConfig {
    fn default() -> Self {
        let port = std::env::var("WEBHOOKR_PORT").unwrap_or_else(|_| "9000".to_string());
        Self {
            listen_addr: format!("0.0.0.0:{port}"),
            projects: Vec::new(),
            cloudflare: None,
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

/// Persist config to disk, creating parent directories as needed.
pub fn save_config(cfg: &AppConfig) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(cfg)?;
    fs::write(&path, text)
        .with_context(|| format!("failed to write config at {}", path.display()))?;
    Ok(())
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
    let path = cloudflare_credentials_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(credentials)?;
    fs::write(&path, text).with_context(|| {
        format!(
            "failed to write Cloudflare credentials at {}",
            path.display()
        )
    })?;
    set_owner_only(&path)?;
    Ok(())
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
            deploy_preset: "compose_up".into(),
            compose_file: "deploy/compose.yaml".into(),
            compose_profiles: vec!["web".into()],
        };
        project.validate().unwrap();
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
