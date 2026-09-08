//! Checks releases and installs immutable, side-by-side desktop versions.

use std::{
    env, fs,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, OnceLock},
    thread,
    time::Duration,
};

use async_channel::Sender;
use dirigent_launcher::{self as installation, DESKTOP_EXE, LAUNCHER_EXE, Selection};
use dirigent_server::contract::{ArtifactKind, VersionArtifact, VersionResponse};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const DEFAULT_API_URL: &str = "https://dirigent.sebba.dev";
const CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);
static RESTART_ROOT: OnceLock<PathBuf> = OnceLock::new();

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
        path: Arc<TempDir>,
    },
    Installing {
        release: VersionResponse,
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
        path: Arc<TempDir>,
    },
    DownloadFailed {
        release: VersionResponse,
        error: String,
    },
}

/// Exercises update checking and downloading without selecting or restarting a build.
pub(crate) fn dry_run_enabled() -> bool {
    cfg!(feature = "update-dry-run")
}

fn installation_root() -> Result<PathBuf, String> {
    let executable = env::current_exe().map_err(|e| e.to_string())?;
    installation::installation_root(
        &executable,
        crate::build_info::channel(),
        crate::build_info::version(),
    )
}

pub(crate) fn start_checker(events: Sender<UpdateEvent>) {
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
    if !dry_run_enabled() && installation_root().is_err() {
        return Ok(None);
    }
    let base = env::var("DIRIGENT_UPDATE_API").unwrap_or_else(|_| DEFAULT_API_URL.into());
    let response = client
        .get(format!(
            "{}/api/v0/version/{}",
            base.trim_end_matches('/'),
            crate::build_info::channel()
        ))
        .send()
        .map_err(|error| format!("could not check for updates: {error}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let release = response
        .error_for_status()
        .map_err(|error| format!("update server returned an error: {error}"))?
        .json::<VersionResponse>()
        .map_err(|error| format!("could not decode update response: {error}"))?;
    if release.channel != crate::build_info::channel() {
        return Err("update server returned the wrong channel".into());
    }
    if !installation::is_newer(
        &release.channel,
        &release.version,
        crate::build_info::version(),
    )? {
        return Ok(None);
    }
    // Another open instance may already have updated this installation.
    if !dry_run_enabled() {
        let selected = Selection::read(&installation_root()?)?;
        if !installation::is_newer(&release.channel, &release.version, &selected.version)? {
            return Ok(None);
        }
    }
    if matching_artifact(&release).is_err() {
        tracing::warn!(
            target = crate::build_info::target(),
            "release has no application artifact for this platform"
        );
        return Ok(None);
    }
    Ok(Some(release))
}

pub(crate) fn download(release: VersionResponse, events: Sender<UpdateEvent>) {
    thread::Builder::new()
        .name("dirigent-update-download".into())
        .spawn(move || {
            let result = matching_artifact(&release)
                .and_then(|artifact| download_artifact(artifact, &events));
            let event = match result {
                Ok(path) => UpdateEvent::Downloaded {
                    release,
                    path: Arc::new(path),
                },
                Err(error) => UpdateEvent::DownloadFailed { release, error },
            };
            let _ = events.send_blocking(event);
        })
        .expect("could not start update download");
}

fn matching_artifact(release: &VersionResponse) -> Result<&VersionArtifact, String> {
    release
        .artifacts
        .iter()
        .find(|artifact| {
            artifact.target == crate::build_info::target()
                && artifact.kind == ArtifactKind::Application
        })
        .ok_or_else(|| {
            format!(
                "release has no {} application artifact",
                crate::build_info::target()
            )
        })
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
) -> Result<TempDir, String> {
    let directory = if dry_run_enabled() {
        crate::platform::cache_path()?
            .parent()
            .ok_or("cache has no parent")?
            .join("updates")
    } else {
        installation_root()?.join("versions")
    };
    fs::create_dir_all(&directory)
        .map_err(|e| format!("could not create {}: {e}", directory.display()))?;
    // Staging on the install filesystem makes publication a directory rename, not an EXE copy.
    // TempDir ownership follows the UI state and removes cancelled/failed downloads.
    let staged = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(&directory)
        .map_err(|e| format!("could not stage update: {e}"))?;
    let path = staged.path().join(DESKTOP_EXE);
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
    let mut file = File::create(&path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;
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
    drop(file);
    if size != artifact.size {
        return Err(format!(
            "downloaded artifact has size {size}, expected {}",
            artifact.size
        ));
    }
    if format!("{:x}", hasher.finalize()) != artifact.sha256 {
        return Err("downloaded artifact checksum does not match".into());
    }
    make_executable(&path)?;
    Ok(staged)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("could not make update executable: {error}"))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Only publication and the tiny selection write happen on the UI thread.
/// Restart is deferred until GPUI and the old application state have shut down.
pub(crate) fn install_update(release: &VersionResponse, staged: &Path) -> Result<(), String> {
    if release.channel != crate::build_info::channel() {
        return Err("update belongs to a different channel".into());
    }
    let root = installation_root()?;
    let _lock = installation::lock(&root, true)?;
    installation::publish(
        &root,
        staged,
        Selection::new(&release.channel, &release.version)?,
    )?;
    let _ = RESTART_ROOT.set(root);
    tracing::info!(version = %release.version, "selected updated Dirigent; restarting after shutdown");
    Ok(())
}

pub(crate) fn restart_after_shutdown() -> Result<(), String> {
    let Some(root) = RESTART_ROOT.get() else {
        return Ok(());
    };
    Command::new(root.join(LAUNCHER_EXE))
        .args(env::args_os().skip(1))
        .spawn()
        .map_err(|e| format!("could not restart Dirigent through its launcher: {e}"))?;
    Ok(())
}

pub(crate) fn cleanup_old_versions() {
    let Ok(root) = installation_root() else {
        return;
    };
    if let Err(error) = thread::Builder::new()
        .name("dirigent-update-cleanup".into())
        .spawn(move || {
            let result = (|| {
                let _lock = installation::lock(&root, true)?;
                installation::cleanup(&root, crate::build_info::version())
            })();
            if let Err(error) = result {
                tracing::warn!(%error, "could not clean up old versions");
            }
        })
    {
        tracing::warn!(%error, "could not start version cleanup");
    }
}
