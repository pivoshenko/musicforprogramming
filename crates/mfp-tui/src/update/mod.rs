//! Checking what the latest release is, and replacing the installed binaries with it.
//!
//! A GitHub release is the only authority on what the latest version is: there is no update
//! server to reach and nothing here is consulted during playback. Every failure is reported
//! rather than fatal - `self update` names what went wrong and exits non-zero, the notice
//! stays silent - because neither the player nor a script that drives it should ever stop
//! working because api.github.com did.
//!
//! A release ships `mfp` and `mfp-daemon` as a pair, so both are replaced together.

pub mod install;
pub mod notice;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

/// The repository releases are cut from.
const REPO: &str = "pivoshenko/musicforprogramming";

/// The binaries a release archive carries, in the order they are reported.
const BINARIES: [&str; 2] = ["mfp", "mfp-daemon"];

/// The running version, as `Cargo.toml` declares it.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The GitHub API refuses a request without one.
const USER_AGENT: &str = concat!("mfp/", env!("CARGO_PKG_VERSION"));

/// Long enough for a slow connection to answer, short enough that a hung endpoint does not
/// hold a command open.
const API_TIMEOUT: Duration = Duration::from_secs(10);

/// The archives are a few megabytes and a home connection is not always quick.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

impl Release {
    /// The tag without its `v`, which is what every version here is compared as.
    pub fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }

    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }
}

/// What `self update` did, for `--json`.
#[derive(Debug, serde::Serialize)]
struct Outcome {
    current_version: String,
    latest_version: String,
    status: &'static str,
}

fn client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .build()
        .context("Cannot build an HTTP client")
}

/// Asks GitHub for the latest release. A pre-release is not "latest" to that endpoint, so
/// one never becomes an update anybody is offered.
pub fn fetch_latest_release() -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let response = client(API_TIMEOUT)?
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .context("Cannot reach api.github.com")?;

    // The endpoint answers 404 for a repository with no releases at all, which is a
    // different thing from a request GitHub turned away
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("{REPO} has published no releases yet");
    }

    let body = response
        .error_for_status()
        .context("GitHub refused the request for the latest release")?
        .text()
        .context("Cannot read the release response")?;
    serde_json::from_str(&body).context("Cannot parse the release response")
}

/// Whether `latest` is a later `major.minor.patch` than `current`.
///
/// Compares the three numbers and nothing else: a suffix such as `-rc.1` is dropped rather
/// than ordered, which is why this is only ever asked about a release GitHub already calls
/// latest.
pub fn is_newer(current: &str, latest: &str) -> bool {
    triple(latest) > triple(current)
}

fn triple(version: &str) -> (u64, u64, u64) {
    let mut parts = version
        .split('.')
        .map(|part| part.split(['-', '+']).next().unwrap_or(part))
        .map(|part| part.parse().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// The release target triple this build runs on, spelled as the release workflow names its
/// archives.
fn current_target() -> String {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "macos" => format!("{arch}-apple-darwin"),
        "linux" => format!("{arch}-unknown-linux-gnu"),
        other => format!("{arch}-unknown-{other}"),
    }
}

fn archive_name(target: &str) -> String {
    format!("musicforprogramming-{target}.tar.gz")
}

/// Where this copy came from, which decides whether replacing the binaries is ours to do.
#[derive(Debug, PartialEq, Eq)]
pub enum InstallMethod {
    /// Homebrew owns the files and records their hashes; writing over them breaks `brew`
    Homebrew,
    /// Built by cargo, which is no longer an installation this project offers. Still
    /// recognised, because a copy in a cargo bin directory is one `self update` must not
    /// write over: cargo built it from source and a downloaded binary is not what it
    /// produced, whether or not anybody is told to install that way any more
    Cargo,
    /// The standalone installer, or a hand-placed copy: nothing else is tracking these
    Installer,
}

impl InstallMethod {
    /// The command that upgrades a copy from this source.
    pub fn upgrade_command(&self) -> &'static str {
        match self {
            Self::Homebrew => "brew upgrade pivoshenko/tap/musicforprogramming",
            Self::Cargo => "curl -fsSL https://pivoshenko.dev/mfp.sh | sh",
            Self::Installer => "mfp self update",
        }
    }

    /// Where this copy came from, as the refusal names it.
    fn installer_name(&self) -> &'static str {
        match self {
            Self::Homebrew => "Homebrew",
            Self::Cargo => "cargo",
            Self::Installer => "the standalone installer",
        }
    }
}

pub fn detect_install_method() -> InstallMethod {
    let exe = std::env::current_exe().unwrap_or_default();
    classify(&exe.to_string_lossy())
}

fn classify(path: &str) -> InstallMethod {
    if path.contains("/Cellar/")
        || path.contains("/opt/homebrew/")
        || path.contains("/Homebrew/")
        || path.contains("/linuxbrew/")
    {
        InstallMethod::Homebrew
    } else if path.contains("/.cargo/bin/") || path.contains("/cargo/bin/") {
        InstallMethod::Cargo
    } else {
        InstallMethod::Installer
    }
}

/// Where each binary of the pair lives.
///
/// `mfp` is wherever this process was started from; `mfp-daemon` is what the client would
/// autostart - `$MFP_DAEMON` if it names one, otherwise the copy beside `mfp`. A daemon
/// found neither way is not hunted down on `PATH`: replacing a binary this process only
/// guessed at is worse than leaving it to the next `mfp` run to report.
fn targets() -> Result<Vec<(&'static str, PathBuf)>> {
    let mfp = std::env::current_exe().context("Cannot locate the running executable")?;
    let daemon = match std::env::var("MFP_DAEMON") {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => mfp.with_file_name("mfp-daemon"),
    };

    let mut found = vec![(BINARIES[0], mfp)];
    if daemon.is_file() {
        found.push((BINARIES[1], daemon));
    }
    Ok(found)
}

/// Runs `mfp self update`, returning the process exit code.
///
/// `check` reports and stops. `json` replaces every line with one document, so a script can
/// ask whether an update exists without parsing prose.
pub fn run(check: bool, json: bool) -> u8 {
    match update(check, json) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error:#}");
            crate::cli::EXIT_REJECTED
        }
    }
}

fn update(check: bool, json: bool) -> Result<u8> {
    let release = fetch_latest_release()?;
    let latest = release.version().to_string();

    if !is_newer(CURRENT, &latest) {
        report(json, &latest, "up_to_date")?;
        if !json {
            println!("musicforprogramming {CURRENT} is the latest release.");
        }
        return Ok(crate::cli::EXIT_OK);
    }

    if check {
        report(json, &latest, "available")?;
        if !json {
            println!("Update available: {CURRENT} -> {latest}");
            println!("Run `{}` to install it.", upgrade_hint());
        }
        return Ok(crate::cli::EXIT_OK);
    }

    // Refused rather than attempted: a package manager records the hash of what it placed,
    // and a binary swapped out from under it is a broken install that reports itself as fine
    let method = detect_install_method();
    if method != InstallMethod::Installer {
        bail!(
            "This copy was installed with {}; run `{}` instead",
            method.installer_name(),
            method.upgrade_command()
        );
    }

    let target = current_target();
    let archive = archive_name(&target);
    let asset = release
        .asset(&archive)
        .ok_or_else(|| anyhow!("Release v{latest} ships no archive for {target}"))?;
    let checksums = release
        .asset("checksums.txt")
        .ok_or_else(|| anyhow!("Release v{latest} ships no checksums.txt"))?;

    let destinations = targets()?;

    if !json {
        println!("Update available: {CURRENT} -> {latest}");
    }

    let http = client(DOWNLOAD_TIMEOUT)?;
    let body = install::download(&http, &asset.browser_download_url)
        .with_context(|| format!("Cannot download {archive}"))?;
    let expected = install::download(&http, &checksums.browser_download_url)
        .context("Cannot download checksums.txt")?;
    let expected = String::from_utf8(expected).context("checksums.txt is not text")?;
    install::verify(&body, &archive, &expected)?;
    if !json {
        println!("Downloaded and verified {archive}");
    }

    install::replace(&body, &destinations)?;

    report(json, &latest, "updated")?;
    if !json {
        for (name, path) in &destinations {
            println!("Installed {name} to {}", path.display());
        }
        if destinations.len() < BINARIES.len() {
            println!(
                "mfp-daemon was not beside mfp and was left alone; update it the way it was installed."
            );
        }
        // The daemon outlives every client on purpose, so the one already running is still
        // the old build until something ends it
        println!("Run `mfp shutdown` when convenient to restart the daemon on {latest}.");
    }

    Ok(crate::cli::EXIT_OK)
}

/// The command to suggest for the copy that is running, which is not `self update` when
/// something else owns the files.
fn upgrade_hint() -> &'static str {
    detect_install_method().upgrade_command()
}

fn report(json: bool, latest: &str, status: &'static str) -> Result<()> {
    if !json {
        return Ok(());
    }
    let line = serde_json::to_string(&Outcome {
        current_version: CURRENT.to_string(),
        latest_version: latest.to_string(),
        status,
    })
    .context("Cannot render the outcome as JSON")?;
    println!("{line}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_patch_minor_or_major_is_newer() {
        assert!(is_newer("1.0.0", "1.0.1"));
        assert!(is_newer("1.0.0", "1.1.0"));
        assert!(is_newer("1.9.9", "2.0.0"));
    }

    #[test]
    fn the_same_or_an_earlier_version_is_not_newer() {
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("2.0.0", "1.9.9"));
    }

    #[test]
    fn a_prerelease_suffix_is_dropped_rather_than_ordered() {
        assert!(!is_newer("1.0.0", "1.0.0-rc.1"));
        assert!(is_newer("1.0.0", "1.0.1-rc.1"));
    }

    #[test]
    fn a_malformed_version_compares_as_zero_rather_than_panicking() {
        assert!(!is_newer("1.0.0", "banana"));
        assert!(is_newer("banana", "0.0.1"));
    }

    #[test]
    fn the_current_version_is_never_newer_than_itself() {
        assert!(!is_newer(CURRENT, CURRENT));
    }

    #[test]
    fn a_tag_is_compared_without_its_v() {
        let release = Release {
            tag_name: "v1.2.3".into(),
            assets: Vec::new(),
        };
        assert_eq!(release.version(), "1.2.3");
    }

    #[test]
    fn the_archive_name_matches_what_the_release_workflow_publishes() {
        assert_eq!(
            archive_name("aarch64-apple-darwin"),
            "musicforprogramming-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn the_current_target_is_one_of_the_published_triples() {
        let target = current_target();
        assert!(
            target.ends_with("-apple-darwin") || target.ends_with("-unknown-linux-gnu"),
            "{target}"
        );
    }

    #[test]
    fn a_homebrew_prefix_is_recognised() {
        for path in [
            "/opt/homebrew/bin/mfp",
            "/usr/local/Cellar/musicforprogramming/1.0.0/bin/mfp",
            "/home/linuxbrew/.linuxbrew/bin/mfp",
        ] {
            assert_eq!(classify(path), InstallMethod::Homebrew, "{path}");
        }
    }

    #[test]
    fn a_cargo_bin_directory_is_recognised() {
        assert_eq!(classify("/home/me/.cargo/bin/mfp"), InstallMethod::Cargo);
    }

    #[test]
    fn anything_else_is_treated_as_the_installers_own_copy() {
        for path in ["/usr/local/bin/mfp", "/home/me/.local/bin/mfp", ""] {
            assert_eq!(classify(path), InstallMethod::Installer, "{path}");
        }
    }

    #[test]
    fn each_install_method_names_its_own_upgrade_command() {
        assert!(
            InstallMethod::Homebrew
                .upgrade_command()
                .starts_with("brew ")
        );
        assert_eq!(
            InstallMethod::Installer.upgrade_command(),
            "mfp self update"
        );
    }

    /// Cargo is no longer an installation this project offers, so a copy it built is sent to
    /// one that is rather than told to reinstall the way it came.
    #[test]
    fn a_cargo_built_copy_is_pointed_at_a_supported_installation() {
        let command = InstallMethod::Cargo.upgrade_command();
        assert!(!command.starts_with("cargo "), "{command}");
        assert!(command.contains("mfp.sh"), "{command}");
    }

    #[test]
    fn an_asset_is_matched_by_its_exact_name() {
        let release = Release {
            tag_name: "v1.1.0".into(),
            assets: vec![
                Asset {
                    name: "musicforprogramming-aarch64-apple-darwin.tar.gz".into(),
                    browser_download_url: "https://example.com/a".into(),
                },
                Asset {
                    name: "checksums.txt".into(),
                    browser_download_url: "https://example.com/c".into(),
                },
            ],
        };
        assert!(
            release
                .asset("musicforprogramming-aarch64-apple-darwin.tar.gz")
                .is_some()
        );
        assert!(release.asset("checksums.txt").is_some());
        assert!(
            release
                .asset("musicforprogramming-x86_64-apple-darwin.tar.gz")
                .is_none()
        );
    }

    #[test]
    fn the_release_response_parses_into_the_fields_that_are_used() {
        let release: Release = serde_json::from_str(
            r#"{"tag_name":"v1.1.0","name":"ignored","assets":[
                {"name":"checksums.txt","browser_download_url":"https://example.com/c","size":9}
            ]}"#,
        )
        .unwrap();
        assert_eq!(release.version(), "1.1.0");
        assert_eq!(release.assets.len(), 1);
    }
}
