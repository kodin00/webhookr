//! Form payloads and their mapping onto [`ProjectConfig`].
//!
//! Validation deliberately ends at [`ProjectConfig::validate`], the same
//! function the CLI and TUI use, so there is exactly one authority on what a
//! valid project looks like.

use anyhow::Result;
use serde::Deserialize;

use crate::config::{DatabaseConfig, ProjectConfig};
use crate::util;

fn default_branch() -> String {
    "main".to_string()
}

/// The single add/edit form. Every field the TUI's nine-step wizard collects,
/// on one page.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectForm {
    pub name: String,
    #[serde(default)]
    pub id: String,
    /// Manual repository URL. Empty when the user picks from the radio list
    /// rendered by the `/f/github-repos` fragment; non-empty takes precedence
    /// over [`Self::repository_pick`].
    #[serde(default)]
    pub repository: String,
    /// Encoded `"clone_url\tkey_id"` from the repo picker radio list. Empty
    /// when the user types a URL manually instead.
    #[serde(default)]
    pub repository_pick: String,
    /// Id of the shared GitHub key to link to, for the manual-URL case. The
    /// radio list carries its own key id inside [`Self::repository_pick`].
    #[serde(default)]
    pub github_key_id: String,
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
    /// Which deployment triggers are ticked. Two independent checkboxes, not
    /// one multi-value control: a urlencoded body cannot carry repeated keys
    /// into a `Vec` field, and a lone `trigger_events=push` is a string, not a
    /// sequence — a single field there failed to deserialize at all. Neither
    /// ticked is the all-events default.
    #[serde(default)]
    pub trigger_push: Option<String>,
    #[serde(default)]
    pub trigger_merge: Option<String>,
    /// PR source branches whose merges may trigger a deploy. Comma- or
    /// newline-separated in the form; split here. A single comma-separated
    /// value is what a urlencoded body carries well — repeated keys are the
    /// thing it cannot feed a list field (see the checkboxes above). Empty
    /// means any merge.
    #[serde(default)]
    pub merge_sources: String,
    /// Present when the "report commit statuses" checkbox is ticked.
    #[serde(default)]
    pub status_reports: Option<String>,
    #[serde(default)]
    pub status_context: String,
    /// Database mode: `auto` (detect from the compose file), `manual` (use
    /// [`Self::db_kind`] + [`Self::db_service`]), or `off`. Blank reads as
    /// `auto`; a blank *section* (see [`Self::to_project`]) means the form
    /// predates rollbacks and the stored block rides along.
    #[serde(default)]
    pub db_mode: String,
    /// Engine for manual mode: `postgres`, `mysql`, or `mariadb`. Ignored (and
    /// blanked) in auto/off, where detection fills the engine in at backup
    /// time — a value typed then abandoned must not lurk in the config
    /// looking meaningful.
    #[serde(default)]
    pub db_kind: String,
    /// Compose service running the database, for manual mode only. Same
    /// blanking rule as [`Self::db_kind`].
    #[serde(default)]
    pub db_service: String,
    /// Compose file the `auto` detector reads, relative to the project path —
    /// a monorepo can keep the database in `ops/db.yml` while the preset
    /// deploys `docker-compose.yml`. Empty is valid: it means the preset's
    /// compose file (or no detection, for a custom-command project). Rides
    /// along on the stored block when a blank section carries it forward
    /// (see [`Self::to_project`]).
    #[serde(default)]
    pub db_compose_file: String,
    /// Present when the "back up before deploys" checkbox is ticked. Same
    /// presence pattern as [`Self::status_reports`]: an absent key is the only
    /// way an unticked checkbox can announce itself.
    #[serde(default)]
    pub db_backups: Option<String>,
    /// Dumps to keep per project, as a string because urlencoded forms carry
    /// strings. Blank or unparsable falls back to 5; 0 is floored to 1 (and
    /// [`ProjectConfig::validate`] would refuse it anyway).
    #[serde(default)]
    pub db_keep: String,
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

        // The repo picker offers a radio list (encoded as "url\tkey_id") and a
        // manual URL field with a separate keys `<select>`. A typed URL wins
        // over a radio pick; a radio pick carries its own key id. The form no
        // longer edits tokens — they are carried forward from the existing
        // project so a legacy per-project token survives until the project is
        // migrated to a shared github key.
        let (repository, github_key_id) = {
            let manual = self.repository.trim();
            if !manual.is_empty() {
                (manual.to_string(), self.github_key_id.trim().to_string())
            } else {
                let pick = self.repository_pick.trim();
                match pick.rsplit_once('\t') {
                    Some((url, key_id)) => (url.trim().to_string(), key_id.trim().to_string()),
                    None => (pick.to_string(), self.github_key_id.trim().to_string()),
                }
            }
        };

        let git_token = existing.map(|p| p.git_token.clone()).unwrap_or_default();
        let status_token = existing.map(|p| p.status_token.clone()).unwrap_or_default();

        let mut trigger_events = Vec::new();
        if self.trigger_push.is_some() {
            trigger_events.push("push".to_string());
        }
        if self.trigger_merge.is_some() {
            trigger_events.push("merge".to_string());
        }

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

        let database = self.database(existing);

        let project = ProjectConfig {
            id,
            name: self.name.trim().to_string(),
            path: self.path.trim().to_string(),
            branch: self.branch.trim().to_string(),
            command: self.command.trim().to_string(),
            secret,
            verify_mode,
            repository,
            trigger_events,
            merge_sources: split_profiles(&self.merge_sources),
            git_token,
            github_key_id,
            deploy_preset,
            compose_file,
            compose_profiles: split_profiles(&self.compose_profiles),
            status_reports: self.status_reports.is_some(),
            status_token,
            status_context: self.status_context.trim().to_string(),
            database,
        };

        project.validate()?;
        Ok(project)
    }

    /// The database block from the submitted fields.
    ///
    /// A form that carries *no* database fields at all (a client posting the
    /// pre-rollback shape — every db_* key absent, so every field blank) is not
    /// an operator asking for defaults; it is a client that never saw the
    /// section. Carrying the stored block forward keeps such an edit from
    /// resetting a `manual`/`off` setup behind the operator's back. On create
    /// there is nothing to carry, so the same branch lands on
    /// [`DatabaseConfig::default`], which is what the section's blanks mean
    /// anyway.
    fn database(&self, existing: Option<&ProjectConfig>) -> DatabaseConfig {
        let section_submitted = !self.db_mode.trim().is_empty()
            || !self.db_kind.trim().is_empty()
            || !self.db_service.trim().is_empty()
            || self.db_backups.is_some()
            || !self.db_keep.trim().is_empty();
        if !section_submitted {
            return existing.map(|p| p.database.clone()).unwrap_or_default();
        }

        let mode = match self.db_mode.trim() {
            "" => "auto".to_string(),
            // A typo is left for validate() to reject — it is the one
            // authority on what a valid project looks like, and silently
            // rewriting a hand-crafted POST would hide the mistake.
            other => other.to_string(),
        };
        // auto/off ignore kind + service entirely, so blank them rather than
        // store a value that only looks like it would be used.
        let (kind, service) = if mode == "manual" {
            (
                self.db_kind.trim().to_string(),
                self.db_service.trim().to_string(),
            )
        } else {
            (String::new(), String::new())
        };

        DatabaseConfig {
            mode,
            kind,
            service,
            // Trimmed as-is: empty is a real answer ("the preset's file /
            // none"), not a blank to be second-guessed — only the fully
            // blank section above carries the stored value forward.
            compose_file: self.db_compose_file.trim().to_string(),
            backups: self.db_backups.is_some(),
            keep: self
                .db_keep
                .trim()
                .parse::<usize>()
                // Includes the blank case: an empty number input posts "".
                .map(|n| n.max(1))
                .unwrap_or(5),
        }
    }

    /// Rebuild a form from a stored project, for the edit page.
    pub fn from_project(project: &ProjectConfig) -> Self {
        Self {
            name: project.name.clone(),
            id: project.id.clone(),
            repository: project.repository.clone(),
            repository_pick: String::new(),
            github_key_id: project.github_key_id.clone(),
            path: project.path.clone(),
            branch: project.branch.clone(),
            command: project.command.clone(),
            deploy_preset: project.deploy_preset.clone(),
            compose_file: project.compose_file.clone(),
            compose_profiles: project.compose_profiles.join(", "),
            verify_mode: project.verify_mode.clone(),
            trigger_push: project
                .trigger_events
                .iter()
                .any(|e| e == "push")
                .then(|| "1".to_string()),
            trigger_merge: project
                .trigger_events
                .iter()
                .any(|e| e == "merge")
                .then(|| "1".to_string()),
            merge_sources: project.merge_sources.join(", "),
            status_reports: project.status_reports.then(|| "1".to_string()),
            status_context: project.status_context.clone(),
            db_mode: project.database.mode.clone(),
            db_kind: project.database.kind.clone(),
            db_service: project.database.service.clone(),
            db_compose_file: project.database.compose_file.clone(),
            db_backups: project.database.backups.then(|| "1".to_string()),
            db_keep: project.database.keep.to_string(),
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
            repository_pick: String::new(),
            github_key_id: String::new(),
            path: "/tmp/webhookr-form-test".into(),
            branch: "main".into(),
            command: "./deploy.sh".into(),
            deploy_preset: "custom".into(),
            compose_file: String::new(),
            compose_profiles: " web , , worker\nextra ".into(),
            verify_mode: "github".into(),
            trigger_push: None,
            trigger_merge: None,
            merge_sources: String::new(),
            status_reports: None,
            status_context: String::new(),
            // All blank: the shape a client predating the database section
            // still posts, and the shape every non-database test wants.
            db_mode: String::new(),
            db_kind: String::new(),
            db_service: String::new(),
            db_compose_file: String::new(),
            db_backups: None,
            db_keep: String::new(),
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
        assert_eq!(
            project.secret, original.secret,
            "secret must survive an edit"
        );
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
    fn ticked_checkboxes_select_triggers() {
        let mut form = form();
        form.trigger_push = Some("1".into());
        form.trigger_merge = Some("1".into());
        let project = form.to_project(None).unwrap();
        assert_eq!(project.trigger_events, vec!["push", "merge"]);

        // The edit page re-ticks from the stored list, and an edit that
        // unticks both falls back to the all-events default.
        let rebuilt = ProjectForm::from_project(&project);
        assert!(rebuilt.trigger_push.is_some() && rebuilt.trigger_merge.is_some());
        assert_eq!(
            rebuilt.to_project(Some(&project)).unwrap().trigger_events,
            vec!["push", "merge"]
        );

        let cleared = ProjectForm {
            trigger_push: None,
            trigger_merge: None,
            ..ProjectForm::from_project(&project)
        };
        assert!(cleared
            .to_project(Some(&project))
            .unwrap()
            .trigger_events
            .is_empty());
    }

    #[test]
    fn trigger_checkboxes_deserialize_from_a_form_body() {
        // Regression against the wire format the checkboxes actually send.
        // A single `trigger_events: Vec<String>` field could not read it:
        // `trigger_events=push` deserialized as a string ("invalid type:
        // string, expected a sequence"), and ticking both boxes sent the key
        // twice, which the struct rejected as a duplicate field.
        let body = "name=My+Site&repository=https://example.com/site.git&\
                    path=/tmp/webhookr-form-test&branch=main&command=./deploy.sh&\
                    trigger_push=1&trigger_merge=1";
        let form = serde_urlencoded::from_str::<ProjectForm>(body).unwrap();
        assert_eq!(
            form.to_project(None).unwrap().trigger_events,
            vec!["push", "merge"]
        );

        let body = "name=My+Site&repository=https://example.com/site.git&\
                    path=/tmp/webhookr-form-test&branch=main&command=./deploy.sh&\
                    trigger_merge=1";
        let form = serde_urlencoded::from_str::<ProjectForm>(body).unwrap();
        assert_eq!(form.to_project(None).unwrap().trigger_events, vec!["merge"]);

        // Neither ticked: the field is simply absent from the body.
        let body = "name=My+Site&repository=https://example.com/site.git&\
                    path=/tmp/webhookr-form-test&branch=main&command=./deploy.sh";
        let form = serde_urlencoded::from_str::<ProjectForm>(body).unwrap();
        assert!(form.to_project(None).unwrap().trigger_events.is_empty());
    }

    #[test]
    fn merge_sources_split_and_round_trip() {
        let mut form = form();
        form.merge_sources = " develop , , release\nstaging ".into();
        let project = form.to_project(None).unwrap();
        assert_eq!(project.merge_sources, vec!["develop", "release", "staging"]);

        // The edit page rejoins them, and an edit that clears the field drops
        // to the any-merge default.
        let rebuilt = ProjectForm::from_project(&project);
        assert_eq!(rebuilt.merge_sources, "develop, release, staging");
        assert_eq!(
            rebuilt.to_project(Some(&project)).unwrap().merge_sources,
            vec!["develop", "release", "staging"]
        );

        let cleared = ProjectForm {
            merge_sources: String::new(),
            ..ProjectForm::from_project(&project)
        };
        assert!(cleared
            .to_project(Some(&project))
            .unwrap()
            .merge_sources
            .is_empty());
    }

    #[test]
    fn merge_sources_deserialize_from_a_comma_separated_form_body() {
        let body = "name=My+Site&repository=https://example.com/site.git&\
                    path=/tmp/webhookr-form-test&branch=main&command=./deploy.sh&\
                    merge_sources=develop%2Cstaging";
        let form = serde_urlencoded::from_str::<ProjectForm>(body).unwrap();
        assert_eq!(
            form.to_project(None).unwrap().merge_sources,
            vec!["develop", "staging"]
        );
    }

    #[test]
    fn invalid_input_is_rejected() {
        let mut bad = form();
        bad.branch = "--upload-pack=x".into();
        assert!(bad.to_project(None).is_err());
    }

    #[test]
    fn database_section_parses_and_normalizes() {
        let mut form = form();
        form.db_mode = " manual ".into();
        form.db_kind = " postgres ".into();
        form.db_service = " db ".into();
        form.db_compose_file = " ops/db.yml ".into();
        form.db_backups = Some("1".into());
        form.db_keep = "0".into();
        let project = form.to_project(None).unwrap();
        assert_eq!(project.database.mode, "manual");
        assert_eq!(project.database.kind, "postgres");
        assert_eq!(project.database.service, "db");
        assert_eq!(project.database.compose_file, "ops/db.yml");
        assert!(project.database.backups);
        // 0 would prune every backup; validate() refuses it, and the form
        // floors it so saving from the UI can never trip that.
        assert_eq!(project.database.keep, 1);

        // Junk keep falls back to the default rather than failing the save.
        let mut junk = form.clone();
        junk.db_keep = "lots".into();
        assert_eq!(junk.to_project(None).unwrap().database.keep, 5);

        // A submitted section with the compose file cleared stores empty —
        // that is the "use the preset's file" answer, not a blank to fill
        // from somewhere.
        let mut cleared = form.clone();
        cleared.db_compose_file = "   ".into();
        assert_eq!(cleared.to_project(None).unwrap().database.compose_file, "");
    }

    #[test]
    fn database_auto_and_off_blank_kind_and_service() {
        // A value typed while in manual mode must not survive a switch back
        // to auto/off, where it is dead config that only looks meaningful.
        for mode in ["auto", "off"] {
            let mut form = form();
            form.db_mode = mode.into();
            form.db_kind = "postgres".into();
            form.db_service = "db".into();
            let project = form.to_project(None).unwrap();
            assert_eq!(project.database.mode, mode);
            assert!(project.database.kind.is_empty());
            assert!(project.database.service.is_empty());
        }
    }

    #[test]
    fn blank_database_section_carries_the_stored_block_forward() {
        // The wire shape a client predating the section sends: no db_* keys
        // at all. An edit from such a form must not reset a hand-configured
        // block to defaults behind the operator's back.
        let mut stored = form().to_project(None).unwrap();
        stored.database.mode = "manual".into();
        stored.database.kind = "mysql".into();
        stored.database.service = "mysql-db".into();
        stored.database.compose_file = "ops/db.yml".into();
        stored.database.backups = false;
        stored.database.keep = 9;

        let edited = form().to_project(Some(&stored)).unwrap();
        assert_eq!(edited.database, stored.database);
        // Called out because it is the newest field: the blank section must
        // carry the pinned compose file along with the rest of the block.
        assert_eq!(edited.database.compose_file, "ops/db.yml");
    }

    #[test]
    fn database_mode_defaults_to_auto_when_only_other_fields_arrive() {
        // The section is present (the checkbox is ticked) but the select came
        // through empty — read as auto, not as a carry-forward.
        let mut form = form();
        form.db_backups = Some("1".into());
        let project = form.to_project(None).unwrap();
        assert_eq!(project.database.mode, "auto");
        assert!(project.database.backups);
    }
}
