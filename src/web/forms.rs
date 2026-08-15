//! Form payloads and their mapping onto [`ProjectConfig`].
//!
//! Validation deliberately ends at [`ProjectConfig::validate`], the same
//! function the CLI and TUI use, so there is exactly one authority on what a
//! valid project looks like.

use anyhow::Result;
use serde::Deserialize;

use crate::config::ProjectConfig;
use crate::util;

fn default_branch() -> String {
    "main".to_string()
}

/// The single add/edit form. Every field the TUI's nine-step wizard collects,
/// on one page.
#[derive(Debug, Deserialize)]
pub struct ProjectForm {
    pub name: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub deploy_preset: String,
    #[serde(default)]
    pub compose_file: String,
    /// Comma- or newline-separated in the form; split here.
    #[serde(default)]
    pub compose_profiles: String,
    #[serde(default)]
    pub verify_mode: String,
}

impl ProjectForm {
    /// Build a validated [`ProjectConfig`].
    ///
    /// `existing` supplies the immutable id and the current secret when
    /// editing; on create the id is derived from the name and a fresh secret is
    /// generated.
    pub fn to_project(&self, existing: Option<&ProjectConfig>) -> Result<ProjectConfig> {
        let id = match existing {
            Some(project) => project.id.clone(),
            None => {
                let requested = self.id.trim();
                if requested.is_empty() {
                    crate::cli::slugify(&self.name)
                } else {
                    crate::cli::slugify(requested)
                }
            }
        };

        let secret = match existing {
            Some(project) => project.secret.clone(),
            None => util::generate_secret(),
        };

        let compose_file = if self.compose_file.trim().is_empty() {
            "compose.yaml".to_string()
        } else {
            self.compose_file.trim().to_string()
        };

        let deploy_preset = if self.deploy_preset.trim().is_empty() {
            "custom".to_string()
        } else {
            self.deploy_preset.trim().to_string()
        };

        let verify_mode = if self.verify_mode.trim().is_empty() {
            "github".to_string()
        } else {
            self.verify_mode.trim().to_string()
        };

        let project = ProjectConfig {
            id,
            name: self.name.trim().to_string(),
            path: self.path.trim().to_string(),
            branch: self.branch.trim().to_string(),
            command: self.command.trim().to_string(),
            secret,
            verify_mode,
            repository: self.repository.trim().to_string(),
            deploy_preset,
            compose_file,
            compose_profiles: split_profiles(&self.compose_profiles),
        };

        project.validate()?;
        Ok(project)
    }

    /// Rebuild a form from a stored project, for the edit page.
    pub fn from_project(project: &ProjectConfig) -> Self {
        Self {
            name: project.name.clone(),
            id: project.id.clone(),
            repository: project.repository.clone(),
            path: project.path.clone(),
            branch: project.branch.clone(),
            command: project.command.clone(),
            deploy_preset: project.deploy_preset.clone(),
            compose_file: project.compose_file.clone(),
            compose_profiles: project.compose_profiles.join(", "),
            verify_mode: project.verify_mode.clone(),
        }
    }
}

/// Split a comma/newline separated profile list, dropping blanks.
fn split_profiles(raw: &str) -> Vec<String> {
    raw.split([',', '\n'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

/// Turn a raw validation failure into something a person can act on.
///
/// `ProjectConfig::validate` is written for a terminal; the path rule in
/// particular ("project path does not exist") reads as a dead end in a browser
/// when the fix is to fill in a repository URL.
pub fn explain(error: &anyhow::Error) -> String {
    let message = format!("{error:#}");
    if message.contains("project path does not exist") {
        format!(
            "{message}. Either create that directory, pick another one, \
             or set a repository URL so webhookr can clone it on the first run."
        )
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> ProjectForm {
        ProjectForm {
            name: "My Site".into(),
            id: String::new(),
            repository: "https://example.com/site.git".into(),
            path: "/tmp/webhookr-form-test".into(),
            branch: "main".into(),
            command: "./deploy.sh".into(),
            deploy_preset: "custom".into(),
            compose_file: String::new(),
            compose_profiles: " web , , worker\nextra ".into(),
            verify_mode: "github".into(),
        }
    }

    #[test]
    fn derives_slug_and_secret_on_create() {
        let project = form().to_project(None).unwrap();
        assert_eq!(project.id, "my-site");
        assert_eq!(project.compose_file, "compose.yaml");
        assert_eq!(project.compose_profiles, vec!["web", "worker", "extra"]);
        assert_eq!(project.secret.len(), 32);
    }

    #[test]
    fn edit_keeps_id_and_secret() {
        let original = form().to_project(None).unwrap();
        let mut edited = form();
        edited.name = "Renamed".into();
        edited.id = "ignored".into();

        let project = edited.to_project(Some(&original)).unwrap();
        assert_eq!(project.id, original.id, "id must be immutable");
        assert_eq!(project.secret, original.secret, "secret must survive an edit");
        assert_eq!(project.name, "Renamed");
    }

    #[test]
    fn round_trips_through_from_project() {
        let original = form().to_project(None).unwrap();
        let rebuilt = ProjectForm::from_project(&original)
            .to_project(Some(&original))
            .unwrap();
        assert_eq!(original, rebuilt);
    }

    #[test]
    fn invalid_input_is_rejected() {
        let mut bad = form();
        bad.branch = "--upload-pack=x".into();
        assert!(bad.to_project(None).is_err());
    }
}
