//! Updating the app from its GitHub releases: a newer release is offered on the titlebar's Update button, downloads with
//! real progress, is verified to come from the same signing team, swaps in
//! place in one step, and the app restarts into it.
//!
//! Everything runs through macOS's own tools (curl, shasum, ditto, codesign),
//! so the app carries no HTTP or TLS library. The blocking functions here run
//! off the UI thread.

use std::{
    ffi::CString,
    os::unix::{ffi::OsStrExt as _, process::CommandExt as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;

/// The GitHub repository ("owner/name") whose releases are this app's
/// updates, and the Apple team that signs them. Both are given only to
/// official release builds, at compile time, by the maintainer's private
/// release script; any other build has neither and never updates, so builds
/// made from source never offer or try to install the official releases.
const REPOSITORY: Option<&str> = option_env!("FILETREE_UPDATE_REPOSITORY");
const TEAM: Option<&str> = option_env!("FILETREE_RELEASE_TEAM");

/// How often a running app looks for a new release, after launch.
pub(super) const CHECK_EVERY: Duration = Duration::from_secs(3 * 60 * 60);

/// A newer release to install.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Release {
    pub version: String,
    url: String,
    size: u64,
    /// GitHub's SHA-256 of the asset, when it reports one.
    sha256: Option<String>,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

impl Release {
    /// A pretend release for previewing the Update button in debug builds;
    /// installing it only counts the download up.
    pub(super) fn preview() -> Self {
        Self {
            version: "preview".into(),
            url: String::new(),
            size: 0,
            sha256: None,
        }
    }

    pub(super) fn is_preview(&self) -> bool {
        self.url.is_empty()
    }
}

/// Whether this build updates itself: an official release build, given an
/// update source, running from an app bundle signed by its release team.
/// Developer builds and builds made from source never do. Reads the app's signature; call off the UI thread.
pub(super) fn supported() -> bool {
    let Some(team) = TEAM.filter(|_| REPOSITORY.is_some() && !cfg!(debug_assertions)) else {
        return false;
    };
    let Ok(bundle) = bundle() else {
        return false;
    };
    // codesign reports the signature's details on stderr.
    let Ok(output) = Command::new("/usr/bin/codesign")
        .args(["-d", "--verbose=2"])
        .arg(&bundle)
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .any(|line| line.trim() == format!("TeamIdentifier={team}"))
}

/// The latest release, if it is newer than this build.
pub(super) fn check() -> Result<Option<Release>> {
    let repository = REPOSITORY.context("This build has no update source")?;
    let url = format!("https://api.github.com/repos/{repository}/releases/latest");
    let body = run(Command::new("/usr/bin/curl")
        .args(["--fail", "--silent", "--show-error", "--location"])
        .args(["--max-time", "20", "--max-filesize", "1000000"])
        .args(["--header", "Accept: application/vnd.github+json"])
        .arg(url))?;
    newer(&body, env!("CARGO_PKG_VERSION"))
}

/// The release described by GitHub's JSON, if newer than `current` and
/// carrying a zipped app.
fn newer(json: &str, current: &str) -> Result<Option<Release>> {
    let release: GitHubRelease = serde_json::from_str(json)?;
    let version = release.tag_name.trim_start_matches('v').to_owned();
    if release.draft || release.prerelease || parse(&version) <= parse(current) {
        return Ok(None);
    }
    let Some(asset) = release
        .assets
        .into_iter()
        .find(|asset| asset.name.ends_with(".zip"))
    else {
        return Ok(None);
    };
    Ok(Some(Release {
        version,
        url: asset.browser_download_url,
        size: asset.size,
        sha256: asset
            .digest
            .and_then(|digest| digest.strip_prefix("sha256:").map(str::to_owned)),
    }))
}

/// Major, minor and patch; anything unparsable counts as zero.
fn parse(version: &str) -> (u64, u64, u64) {
    let mut parts = version.split('.').map(|part| part.parse().unwrap_or(0));
    let mut next = || parts.next().unwrap_or(0);
    (next(), next(), next())
}

/// The running .app.
fn bundle() -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let bundle = executable
        .ancestors()
        .nth(3)
        .context("Not running from an app bundle")?;
    ensure!(
        bundle
            .extension()
            .is_some_and(|extension| extension == "app"),
        "Not running from an app bundle"
    );
    Ok(bundle.to_path_buf())
}

/// A folder beside the app for the download, removed with the old app in it
/// whether or not the install succeeds.
struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Downloads, verifies, and swaps in `release`, reporting download progress
/// from 0 to 1. On any failure the running app is left untouched.
pub(super) fn install(release: &Release, progress: impl Fn(f32)) -> Result<()> {
    let bundle = bundle()?;
    // Beside the bundle, so the final swap is a same-volume rename.
    let staging = Staging(
        bundle
            .parent()
            .context("App bundle has no parent")?
            .join(format!(".filetree-update-{}", std::process::id())),
    );
    let _ = std::fs::remove_dir_all(&staging.0);
    std::fs::create_dir(&staging.0)?;
    let archive = staging.0.join("update.zip");
    download(release, &archive, progress)?;

    let extracted = staging.0.join("extracted");
    run(Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(&archive)
        .arg(&extracted))?;
    let app = std::fs::read_dir(&extracted)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.extension().is_some_and(|extension| extension == "app"))
        .context("The update contains no app")?;

    // Only a build macOS treats as this same app, signed by the same team, may
    // replace it, so a compromised download cannot install anything else.
    let designated = run(Command::new("/usr/bin/codesign")
        .args(["-d", "-r-"])
        .arg(&bundle))?;
    let requirement = designated
        .lines()
        .find_map(|line| line.trim().strip_prefix("designated => "))
        .context("The app has no designated requirement")?;
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict", "--deep", "-R"])
        .arg(format!("={requirement}"))
        .arg(&app))?;
    let shipped = run(Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
        .arg(app.join("Contents/Info.plist")))?;
    ensure!(shipped.trim() == release.version, "Update version mismatch");

    // One atomic exchange, so an interruption never leaves the app missing.
    // The old bundle ends up in staging and is removed with it.
    exchange(&app, &bundle)
}

fn download(release: &Release, path: &Path, progress: impl Fn(f32)) -> Result<()> {
    let mut curl = Command::new("/usr/bin/curl")
        .args(["--fail", "--silent", "--show-error", "--location"])
        .args(["--connect-timeout", "20", "--max-time", "900"])
        .arg("--max-filesize")
        .arg((release.size + 1).to_string())
        .arg("--output")
        .arg(path)
        .arg(&release.url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    // Progress is the file growing, read while the download runs.
    let status = loop {
        if let Some(status) = curl.try_wait()? {
            break status;
        }
        let received = std::fs::metadata(path).map_or(0, |meta| meta.len());
        progress((received as f32 / release.size.max(1) as f32).min(1.));
        std::thread::sleep(Duration::from_millis(100));
    };
    if !status.success() {
        let mut message = String::new();
        if let Some(mut stderr) = curl.stderr.take() {
            use std::io::Read as _;
            let _ = stderr.read_to_string(&mut message);
        }
        bail!("Download failed: {}", message.trim());
    }
    progress(1.);
    let received = std::fs::metadata(path)?.len();
    ensure!(received == release.size, "Update download incomplete");
    if let Some(expected) = &release.sha256 {
        let output = run(Command::new("/usr/bin/shasum")
            .args(["-a", "256"])
            .arg(path))?;
        let actual = output.split_whitespace().next().unwrap_or_default();
        ensure!(
            actual.eq_ignore_ascii_case(expected),
            "Update download damaged"
        );
    }
    Ok(())
}

/// Swaps two same-volume paths in one step; on failure neither moves.
fn exchange(a: &Path, b: &Path) -> Result<()> {
    let a = CString::new(a.as_os_str().as_bytes())?;
    let b = CString::new(b.as_os_str().as_bytes())?;
    // SAFETY: both are valid NUL-terminated paths for the duration of the call.
    if unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// Opens the app again once this process has exited. Call just before
/// quitting, after the new build is in place.
pub(super) fn relaunch_after_exit() -> Result<()> {
    let bundle = bundle()?;
    Command::new("/bin/sh")
        .arg("-c")
        .arg("while kill -0 \"$0\" 2>/dev/null; do sleep 0.1; done; open \"$1\"")
        .arg(std::process::id().to_string())
        .arg(bundle)
        // Its own process group, so it outlives this app quitting.
        .process_group(0)
        .spawn()?;
    Ok(())
}

/// Runs a system tool, returning its output or its failure.
fn run(command: &mut Command) -> Result<String> {
    let output = command.output()?;
    ensure!(
        output.status.success(),
        "{} failed: {}",
        command.get_program().to_string_lossy(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
