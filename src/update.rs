//! Self-update: what build is running, what is published, and replacing one
//! with the other.
//!
//! Releases are a single rolling `latest` tag, so the tag name says nothing
//! about what is in it. The workflow therefore publishes a `VERSION` asset
//! holding the crate version and the commit it was built from, and bakes the
//! same commit into the binary as `WEBHOOKR_BUILD_SHA`. Comparing the two is
//! what makes "am I actually running the new code?" answerable — which is the
//! question a rolling release makes surprisingly hard.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// Set by the release workflow. Absent in a local `cargo build`, which is
/// exactly how an unreleased build identifies itself.
const BUILD_SHA: Option<&str> = option_env!("WEBHOOKR_BUILD_SHA");

/// Exit code used to hand a restart to the service supervisor.
///
/// Deliberately non-zero: the unit `install.sh` wrote before this existed uses
/// `Restart=on-failure`, and a clean exit there would stop the service instead
/// of restarting it into the new binary.
pub const RESTART_EXIT_CODE: i32 = 70;

/// Longest a download may take. Generous — the binary is several megabytes and
/// this runs on whatever connection the server has.
const TIMEOUT: Duration = Duration::from_secs(120);

static CLIENT: LazyLock<Option<reqwest::Client>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(concat!("webhookr/", env!("CARGO_PKG_VERSION")))
        .timeout(TIMEOUT)
        .build()
        .map_err(|error| eprintln!("webhookr: no HTTP client for updates: {error}"))
        .ok()
});

fn client() -> Result<&'static reqwest::Client> {
    CLIENT.as_ref().context("no HTTP client available")
}

/// Where the release workflow publishes its assets.
fn download_base() -> String {
    format!(
        "{}/releases/latest/download",
        env!("CARGO_PKG_REPOSITORY").trim_end_matches('/')
    )
}

/// A build of webhookr: its crate version, and the commit it came from when it
/// was built by the release workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub version: String,
    /// `None` for a build made outside the release workflow.
    pub commit: Option<String>,
}

impl Build {
    /// The build that is running right now.
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit: BUILD_SHA
                .map(str::trim)
                .filter(|sha| !sha.is_empty())
                .map(short_commit),
        }
    }

    /// One line for a status display, e.g. `0.2.0 (73deef9)`.
    pub fn label(&self) -> String {
        match &self.commit {
            Some(commit) => format!("{} ({commit})", self.version),
            None => format!("{} (local build)", self.version),
        }
    }
}

/// Version string for `--version`, so the running commit is visible without
/// opening the UI. Borrowed from a static because clap wants `&'static str`.
pub fn version_string() -> &'static str {
    static VERSION: LazyLock<String> = LazyLock::new(|| Build::current().label());
    VERSION.as_str()
}

/// Commits are compared and displayed short, the way git and GitHub show them.
fn short_commit(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Parse the published `VERSION` asset: the crate version and the commit it was
/// built from, whitespace separated.
fn parse_version_asset(text: &str) -> Option<Build> {
    let mut fields = text.split_whitespace();
    let version = fields.next()?.to_string();
    Some(Build {
        version,
        commit: fields.next().map(short_commit),
    })
}

/// The build currently published as the rolling `latest` release.
pub async fn latest() -> Result<Build> {
    let url = format!("{}/VERSION", download_base());
    let response = client()?
        .get(&url)
        .send()
        .await
        .context("could not reach the release download host")?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!(
            "the published release has no VERSION asset, so its build cannot be \
             identified. Releases published before self-update existed look like \
             this; the next one will carry it."
        );
    }
    let response = response
        .error_for_status()
        .context("the release download host rejected the request")?;
    let text = response
        .text()
        .await
        .context("could not read the VERSION asset")?;

    parse_version_asset(&text)
        .with_context(|| format!("unexpected VERSION asset contents: {:?}", text.trim()))
}

/// What a self-update did.
#[derive(Debug)]
pub enum Outcome {
    /// Already running the published build; nothing was touched.
    UpToDate(Build),
    /// The binary on disk was replaced. The process must restart to run it.
    Replaced {
        from: Build,
        to: Build,
        path: PathBuf,
    },
}

/// Download the published build and replace this binary with it.
///
/// Does not restart anything: the caller knows whether it is a daemon that can
/// exit and be brought back, or a one-shot command that must not.
pub async fn install() -> Result<Outcome> {
    let current = Build::current();
    let latest = latest().await?;
    if latest == current {
        return Ok(Outcome::UpToDate(current));
    }

    // Resolve symlinks, so a linked `webhookr` is not replaced by a regular
    // file while the real binary is left stale.
    let target = std::env::current_exe().context("could not locate the running binary")?;
    let target = std::fs::canonicalize(&target).unwrap_or(target);

    let asset = asset_name()?;
    let base = download_base();
    let bytes = fetch(&format!("{base}/{asset}"))
        .await
        .with_context(|| format!("could not download {asset}"))?;
    let sums = fetch(&format!("{base}/SHA256SUMS"))
        .await
        .context("could not download SHA256SUMS")?;
    let sums = String::from_utf8(sums).context("SHA256SUMS is not text")?;

    // Verify before anything touches the filesystem: this is a binary that is
    // about to be executed as the deploy user.
    let expected =
        expected_sum(&sums, asset).with_context(|| format!("{asset} is not listed in SHA256SUMS"))?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != expected {
        bail!("checksum mismatch for {asset}: expected {expected}, got {actual}");
    }

    replace_binary(&target, &bytes)?;
    Ok(Outcome::Replaced {
        from: current,
        to: latest,
        path: target,
    })
}

async fn fetch(url: &str) -> Result<Vec<u8>> {
    let response = client()?
        .get(url)
        .send()
        .await
        .context("request failed")?
        .error_for_status()
        .context("the server rejected the request")?;
    Ok(response.bytes().await.context("download failed")?.to_vec())
}

/// The release asset for the platform this is running on.
fn asset_name() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("webhookr-linux-x86_64"),
        ("linux", "aarch64") => Ok("webhookr-linux-aarch64"),
        (os, arch) => bail!(
            "there is no published build for {os}/{arch}; releases cover Linux \
             x86_64 and aarch64 only, so update this one from source"
        ),
    }
}

/// The hash `SHA256SUMS` records for `asset`.
fn expected_sum(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let hash = fields.next()?;
        // `sha256sum` writes "<hash>  <name>", with a binary marker on some
        // platforms; the name is the last field either way.
        let name = fields.last()?.trim_start_matches('*');
        (name == asset).then(|| hash.to_ascii_lowercase())
    })
}

/// Swap in the new binary.
///
/// Written beside the target and renamed over it. A running executable cannot
/// be written to — the kernel returns `ETXTBSY` — but it can be renamed over:
/// the running process keeps the old inode until it exits, which is what makes
/// a daemon able to replace itself and then restart into the new file.
fn replace_binary(target: &Path, bytes: &[u8]) -> Result<()> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".webhookr-update.{}", std::process::id()));

    let swap = || -> Result<()> {
        std::fs::write(&tmp, bytes)
            .with_context(|| format!("could not write {}", tmp.display()))?;
        set_executable(&tmp)?;
        std::fs::rename(&tmp, target)
            .with_context(|| format!("could not replace {}", target.display()))
    };

    if let Err(error) = swap() {
        let _ = std::fs::remove_file(&tmp);
        // By far the most common failure: /usr/local/bin is root-owned and the
        // daemon deliberately is not root. Say so, rather than leaving a bare
        // "permission denied" for the operator to interpret.
        return Err(error.context(format!(
            "cannot replace {} as this process's user; run the installer with sudo instead",
            target.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("could not make {} executable", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_running_build_identifies_itself() {
        let current = Build::current();
        assert_eq!(current.version, env!("CARGO_PKG_VERSION"));
        // Tests run from `cargo test`, never from the release workflow, so this
        // build must not claim to be a release.
        assert_eq!(current.commit, None);
        assert!(
            current.label().ends_with("(local build)"),
            "{}",
            current.label()
        );

        let released = Build {
            version: "0.2.0".into(),
            commit: Some("73deef9".into()),
        };
        assert_eq!(released.label(), "0.2.0 (73deef9)");
    }

    #[test]
    fn reads_the_published_version_asset() {
        assert_eq!(
            parse_version_asset("0.2.0 73deef97f22730548cf22f60edf7892634801de1\n"),
            Some(Build {
                version: "0.2.0".into(),
                // Compared short, so a full sha and a short one still match.
                commit: Some("73deef9".into()),
            })
        );
        // A version with no commit still identifies the release.
        assert_eq!(
            parse_version_asset("0.2.0"),
            Some(Build {
                version: "0.2.0".into(),
                commit: None
            })
        );
        assert_eq!(parse_version_asset("   \n"), None);
        assert_eq!(parse_version_asset(""), None);
    }

    #[test]
    fn a_local_build_never_matches_a_release() {
        // Guards the comparison in `install`: a local build has no commit, so it
        // must not be mistaken for the published one just because the crate
        // version happens to agree.
        let local = Build {
            version: "0.2.0".into(),
            commit: None,
        };
        let released = Build {
            version: "0.2.0".into(),
            commit: Some("73deef9".into()),
        };
        assert_ne!(local, released);
    }

    #[test]
    fn finds_the_checksum_for_an_asset() {
        let sums = "\
aaaa1111  webhookr-linux-aarch64
BBBB2222  webhookr-linux-x86_64
";
        assert_eq!(
            expected_sum(sums, "webhookr-linux-x86_64").as_deref(),
            Some("bbbb2222"),
            "hashes compare case-insensitively"
        );
        assert_eq!(
            expected_sum(sums, "webhookr-linux-aarch64").as_deref(),
            Some("aaaa1111")
        );
        assert_eq!(expected_sum(sums, "webhookr-linux-riscv64"), None);

        // `sha256sum --binary` marks the name with a '*'.
        assert_eq!(
            expected_sum("cccc3333 *webhookr-linux-x86_64", "webhookr-linux-x86_64").as_deref(),
            Some("cccc3333")
        );
    }

    #[test]
    fn swaps_the_binary_by_rename_and_makes_it_executable() {
        let dir = std::env::temp_dir().join(format!("webhookr-swap-{}", crate::util::new_run_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("webhookr");
        std::fs::write(&target, b"old binary").unwrap();

        replace_binary(&target, b"new binary").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new binary");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "the replacement must be executable");
        }
        // No debris beside it: the temp file is renamed, not left behind.
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|e| e.file_name()))
            .filter(|name| name != "webhookr")
            .collect();
        assert!(strays.is_empty(), "left behind: {strays:?}");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn an_unwritable_install_dir_explains_itself() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("webhookr-ro-{}", crate::util::new_run_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("webhookr");
        std::fs::write(&target, b"old binary").unwrap();
        // The real-world case: /usr/local/bin is root-owned, the daemon is not.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        let error = replace_binary(&target, b"new binary")
            .expect_err("a read-only directory cannot be written to");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("run the installer with sudo instead"),
            "an operator needs to be told what to do about it: {rendered}"
        );

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"old binary",
            "a failed swap must leave the working binary in place"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_download_url_points_at_the_rolling_release() {
        let base = download_base();
        assert!(base.starts_with("https://github.com/"), "{base}");
        assert!(base.ends_with("/releases/latest/download"), "{base}");
    }

    #[test]
    fn only_published_platforms_can_self_update() {
        // Whatever this test runs on, the answer must be a decision rather than
        // a panic — and on a platform with no published build, an explanation.
        match asset_name() {
            Ok(asset) => assert!(asset.starts_with("webhookr-linux-")),
            Err(error) => {
                let rendered = format!("{error:#}");
                assert!(rendered.contains("no published build"), "{rendered}");
            }
        }
    }
}
