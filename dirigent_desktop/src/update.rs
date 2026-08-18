//! Checks for releases and replaces the portable Dirigent executable.

use std::{
    env, fs,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use async_channel::Sender;
use dirigent_server::contract::{VersionArtifact, VersionResponse};
use fs2::FileExt as _;
use semver::Version;
use sha2::{Digest, Sha256};

use crate::platform;

const DEFAULT_API_URL: &str = "https://dirigent.sebba.dev";
const CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);
const APPLY_UPDATE_ARGUMENT: &str = "--dirigent-apply-update";

#[derive(Clone)]
pub(crate) enum UpdateState {
    Checking,
    Current,
    Available(VersionResponse),
    Downloading {
        release: VersionResponse,
        downloaded: u64,
        total: u64,
    },
    Ready {
        release: VersionResponse,
        path: PathBuf,
    },
    Failed {
        release: VersionResponse,
    },
}

pub(crate) enum UpdateEvent {
    Checked(Option<VersionResponse>),
    CheckFailed(String),
    DownloadProgress {
        downloaded: u64,
        total: u64,
    },
    Downloaded {
        release: VersionResponse,
        path: PathBuf,
    },
    DownloadFailed {
        release: VersionResponse,
        error: String,
    },
}

pub(crate) fn channel() -> &'static str {
    env!("DIRIGENT_UPDATE_CHANNEL")
}

pub(crate) fn current_version() -> &'static str {
    env!("DIRIGENT_RELEASE_VERSION")
}

pub(crate) fn update_target() -> &'static str {
    env!("DIRIGENT_UPDATE_TARGET")
}

/// Exercises update checking and downloading without replacing the app.
pub(crate) fn dry_run_enabled() -> bool {
    env::var_os("DIRIGENT_UPDATE_DRY_RUN").is_some()
}

fn updates_enabled() -> bool {
    !cfg!(debug_assertions) || env::var_os("DIRIGENT_ENABLE_UPDATES").is_some() || dry_run_enabled()
}

pub(crate) fn start_checker(events: Sender<UpdateEvent>) {
    if !updates_enabled() {
        let _ = events.send_blocking(UpdateEvent::Checked(None));
        return;
    }
    thread::Builder::new()
        .name("dirigent-update-checker".into())
        .spawn(move || {
            let client = match update_client() {
                Ok(client) => client,
                Err(error) => {
                    let _ = events.send_blocking(UpdateEvent::CheckFailed(error));
                    return;
                }
            };
            loop {
                if events.send_blocking(check_event(&client)).is_err() {
                    break;
                }
                thread::sleep(CHECK_INTERVAL);
            }
        })
        .expect("could not start update checker");
}

pub(crate) fn check_now(events: Sender<UpdateEvent>) {
    if !updates_enabled() {
        return;
    }
    thread::Builder::new()
        .name("dirigent-update-check-now".into())
        .spawn(move || {
            let event = match update_client() {
                Ok(client) => check_event(&client),
                Err(error) => UpdateEvent::CheckFailed(error),
            };
            let _ = events.send_blocking(event);
        })
        .expect("could not start update check");
}

fn update_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())
}

fn check_event(client: &reqwest::blocking::Client) -> UpdateEvent {
    match check(client) {
        Ok(release) => UpdateEvent::Checked(release),
        Err(error) => UpdateEvent::CheckFailed(error),
    }
}

fn check(client: &reqwest::blocking::Client) -> Result<Option<VersionResponse>, String> {
    let base = env::var("DIRIGENT_UPDATE_API").unwrap_or_else(|_| DEFAULT_API_URL.into());
    let response = client
        .get(format!(
            "{}/api/v0/version/{}",
            base.trim_end_matches('/'),
            channel()
        ))
        .send()
        .map_err(|error| format!("could not check for updates: {error}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let response = response
        .error_for_status()
        .map_err(|error| format!("update server returned an error: {error}"))?;
    let release = response
        .json::<VersionResponse>()
        .map_err(|error| format!("could not decode update response: {error}"))?;
    if release.channel != channel() {
        return Err("update server returned the wrong channel".into());
    }
    if !is_newer(&release.version)? {
        return Ok(None);
    }
    if !release
        .artifacts
        .iter()
        .any(|artifact| artifact.target == update_target())
    {
        tracing::warn!(
            target = update_target(),
            "release has no artifact for this platform"
        );
        return Ok(None);
    }
    if !can_replace_current_executable() {
        tracing::warn!("Dirigent is installed in a directory that cannot be updated in place");
        return Ok(None);
    }
    Ok(Some(release))
}

fn is_newer(remote: &str) -> Result<bool, String> {
    match channel() {
        "stable" => {
            let remote = Version::parse(remote)
                .map_err(|error| format!("server returned invalid SemVer: {error}"))?;
            let current = Version::parse(current_version())
                .map_err(|error| format!("this build has invalid SemVer: {error}"))?;
            Ok(remote > current)
        }
        _ => {
            validate_branch_version(remote)?;
            validate_branch_version(current_version())?;
            Ok(remote > current_version())
        }
    }
}

fn validate_branch_version(version: &str) -> Result<(), String> {
    let bytes = version.as_bytes();
    let valid = bytes.len() == 16
        && bytes[8] == b'T'
        && bytes[15] == b'Z'
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[9..15].iter().all(u8::is_ascii_digit);
    valid
        .then_some(())
        .ok_or_else(|| "branch versions must use YYYYMMDDTHHMMSSZ UTC".into())
}

fn can_replace_current_executable() -> bool {
    let Ok(executable) = env::current_exe() else {
        return false;
    };
    let Some(parent) = executable.parent() else {
        return false;
    };
    let probe = parent.join(format!(".dirigent-update-probe-{}", std::process::id()));
    match OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(probe);
            true
        }
        Err(_) => false,
    }
}

pub(crate) fn download(release: VersionResponse, events: Sender<UpdateEvent>) {
    thread::Builder::new()
        .name("dirigent-update-download".into())
        .spawn(move || {
            let result = matching_artifact(&release)
                .and_then(|artifact| download_artifact(artifact, &events));
            let event = match result {
                Ok(path) => UpdateEvent::Downloaded {
                    release: release.clone(),
                    path,
                },
                Err(error) => UpdateEvent::DownloadFailed {
                    release: release.clone(),
                    error,
                },
            };
            let _ = events.send_blocking(event);
        })
        .expect("could not start update download");
}

fn matching_artifact(release: &VersionResponse) -> Result<&VersionArtifact, String> {
    release
        .artifacts
        .iter()
        .find(|artifact| artifact.target == update_target())
        .ok_or_else(|| format!("release has no {} artifact", update_target()))
}

pub(crate) fn artifact_size(release: &VersionResponse) -> u64 {
    matching_artifact(release).map_or(0, |artifact| artifact.size)
}

pub(crate) fn display_version(release: &VersionResponse) -> String {
    if release.channel == "stable" {
        release.version.clone()
    } else {
        format!("{}-{}", release.channel, release.version)
    }
}

fn download_artifact(
    artifact: &VersionArtifact,
    events: &Sender<UpdateEvent>,
) -> Result<PathBuf, String> {
    let directory = platform::updates_directory()?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let extension = if cfg!(target_os = "windows") {
        ".exe"
    } else {
        ""
    };
    let path = directory.join(format!(
        "dirigent-download-{}{}",
        std::process::id(),
        extension
    ));
    let temporary = path.with_extension("partial");
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30 * 60))
        .build()
        .map_err(|error| format!("could not initialize update download: {error}"))?;
    let mut response = client
        .get(&artifact.url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| format!("could not download update: {error}"))?;
    let mut file = File::create(&temporary)
        .map_err(|error| format!("could not create {}: {error}", temporary.display()))?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut reported_size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|error| format!("could not download update: {error}"))?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        if size > artifact.size {
            return Err("downloaded artifact is larger than advertised".into());
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|error| format!("could not write update: {error}"))?;
        if size == artifact.size || size.saturating_sub(reported_size) >= 256 * 1024 {
            let _ = events.send_blocking(UpdateEvent::DownloadProgress {
                downloaded: size,
                total: artifact.size,
            });
            reported_size = size;
        }
    }
    file.sync_all()
        .map_err(|error| format!("could not flush update: {error}"))?;
    if size != artifact.size {
        return Err(format!(
            "downloaded artifact has size {size}, expected {}",
            artifact.size
        ));
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != artifact.sha256 {
        return Err("downloaded artifact checksum does not match".into());
    }
    fs::rename(&temporary, &path)
        .map_err(|error| format!("could not finish update download: {error}"))?;
    make_executable(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("could not read update permissions: {error}"))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .map_err(|error| format!("could not make update executable: {error}"))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Starts a copy of the current binary which waits on this returned lock before replacing us.
pub(crate) fn launch_updater(staged: &Path) -> Result<File, String> {
    let current = env::current_exe()
        .map_err(|error| format!("could not locate the running executable: {error}"))?;
    let directory = platform::updates_directory()?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let extension = if cfg!(target_os = "windows") {
        ".exe"
    } else {
        ""
    };
    let helper = directory.join(format!(
        "dirigent-updater-{}{}",
        std::process::id(),
        extension
    ));
    fs::copy(&current, &helper)
        .map_err(|error| format!("could not prepare update helper: {error}"))?;
    let lock_path = directory.join(format!("dirigent-update-{}.lock", std::process::id()));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| format!("could not create update lock: {error}"))?;
    lock.lock_exclusive()
        .map_err(|error| format!("could not lock update handoff: {error}"))?;

    let mut command = Command::new(&helper);
    platform::hide_command_window(&mut command);
    command
        .arg(APPLY_UPDATE_ARGUMENT)
        .arg(&current)
        .arg(staged)
        .arg(&lock_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start update helper: {error}"))?;
    Ok(lock)
}

/// Handles the private updater invocation before GPUI or logging are initialized.
pub(crate) fn run_updater_from_args() -> Option<Result<(), String>> {
    let mut arguments = env::args_os();
    let _executable = arguments.next()?;
    if arguments.next()?.to_str() != Some(APPLY_UPDATE_ARGUMENT) {
        return None;
    }
    let result = (|| {
        let target = arguments
            .next()
            .map(PathBuf::from)
            .ok_or_else(|| "update target is missing".to_string())?;
        let staged = arguments
            .next()
            .map(PathBuf::from)
            .ok_or_else(|| "staged update is missing".to_string())?;
        let lock_path = arguments
            .next()
            .map(PathBuf::from)
            .ok_or_else(|| "update lock is missing".to_string())?;
        if arguments.next().is_some() {
            return Err("unexpected updater argument".into());
        }
        apply_update(&target, &staged, &lock_path)
    })();
    Some(result)
}

fn apply_update(target: &Path, staged: &Path, lock_path: &Path) -> Result<(), String> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(|error| format!("could not open update lock: {error}"))?;
    lock.lock_exclusive()
        .map_err(|error| format!("could not wait for Dirigent to exit: {error}"))?;

    let backup = appended_path(target, ".old");
    let _ = fs::remove_file(&backup);

    #[cfg(target_os = "windows")]
    {
        // Keep the desktop entry intact: renaming it makes Explorer move the replacement icon.
        if let Err(error) = fs::copy(target, &backup) {
            let _ = fs::remove_file(&backup);
            let _ = Command::new(target).spawn();
            return Err(format!("could not back up the old executable: {error}"));
        }
        if let Err(error) = fs::copy(staged, target) {
            if fs::copy(&backup, target).is_ok() {
                let _ = fs::remove_file(&backup);
            }
            let _ = Command::new(target).spawn();
            return Err(format!("could not install the new executable: {error}"));
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let replacement = appended_path(target, ".new");
        let _ = fs::remove_file(&replacement);
        if let Err(error) = fs::copy(staged, &replacement) {
            let _ = Command::new(target).spawn();
            return Err(format!("could not stage replacement: {error}"));
        }
        if let Err(error) = fs::rename(target, &backup) {
            let _ = fs::remove_file(&replacement);
            let _ = Command::new(target).spawn();
            return Err(format!("could not move the old executable: {error}"));
        }
        if let Err(error) = fs::rename(&replacement, target) {
            let _ = fs::rename(&backup, target);
            let _ = Command::new(target).spawn();
            return Err(format!("could not install the new executable: {error}"));
        }
    }

    if let Err(error) = Command::new(target).spawn() {
        #[cfg(target_os = "windows")]
        if fs::copy(&backup, target).is_ok() {
            let _ = fs::remove_file(&backup);
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = fs::remove_file(target);
            let _ = fs::rename(&backup, target);
        }
        let _ = Command::new(target).spawn();
        return Err(format!("could not restart updated Dirigent: {error}"));
    }
    let _ = fs::remove_file(&backup);
    let _ = fs::remove_file(staged);
    let _ = fs2::FileExt::unlock(&lock);
    let _ = fs::remove_file(lock_path);
    Ok(())
}

fn appended_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_versions_sort_by_time() {
        assert!(validate_branch_version("20260310T123456Z").is_ok());
        assert!("20260310T123457Z" > "20260310T123456Z");
        assert!(validate_branch_version("2026-03-10T12:34:56Z").is_err());
    }
}
