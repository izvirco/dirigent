//! Self-update. Release builds ask the update api (`dirigent_api`) for their channel's
//! release every `CHECK_EVERY`; a newer one is downloaded to `<data dir>/updates/`, checked
//! against its minisign signature, and only then offered: the sidebar's update button
//! (`sidebar.rs`), whose menu restarts into it now, or once no local agent is working.
//!
//! Restarting runs the downloaded installer (`start_installer`): on Windows the setup exe
//! (`packaging/windows/dirigent.iss`), on Linux the tarball's `install.sh`
//! (`packaging/linux/install.sh`). It waits for this process to exit, replaces the install
//! and starts Dirigent again. So each update is installed by the new version's own
//! installer, and this file ships with every version too: both the app and its updater can
//! change in any release. What must stay put for installs in the field: the api's url and
//! `Release`, how `start_installer` runs an installer, and the signing keys
//! (`release::KEYS`).
//!
//! Dev and nix builds have no update url and never check. Tests point `DIRIGENT_UPDATE_URL`
//! at a test api (`dirigent_api serve` with an `S3_PREFIX`) and, in debug builds,
//! `DIRIGENT_UPDATE_KEY` at their own signing key.

use crate::app::Dirigent;
use anyhow::{Context as _, Result, bail};
use dirigent_protocol::release::{self, Release};
use gpui::Context;
use semver::Version;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

const CHECK_EVERY: Duration = Duration::from_secs(5 * 60);
/// "Restart when idle" waits for the local agents to be quiet this long: a sub-agent that
/// finishes hands its parent a new turn right after.
const IDLE_FOR: Duration = Duration::from_secs(3);

/// A verified installer, ready to run.
#[derive(Clone)]
pub struct Ready {
    pub version: String,
    /// In `<data dir>/updates/`: the setup exe (Windows) or the tarball (Linux).
    pub installer: PathBuf,
}

/// The update button's state, on `Dirigent`.
#[derive(Default)]
pub struct State {
    pub ready: Option<Ready>,
    /// The user picked "Restart when idle".
    pub when_idle: bool,
    /// Since when no local agent has been working, while `when_idle`.
    idle_since: Option<Instant>,
}

/// The channel's update endpoint, `…/api/v0/updates/{channel}`: baked in by the release
/// workflow (`DIRIGENT_BUILD_UPDATE_URL` at compile time; a name of its own, so a test's
/// run-time variable can't end up in a build), overridden by `DIRIGENT_UPDATE_URL` at run
/// time. `None`: updates are off.
fn endpoint() -> Option<String> {
    let url = std::env::var("DIRIGENT_UPDATE_URL").ok().or_else(|| option_env!("DIRIGENT_BUILD_UPDATE_URL").map(str::to_owned));
    url.filter(|u| !u.is_empty()).map(|u| u.trim_end_matches('/').to_owned())
}

/// Start checking, on a thread of its own (downloads block): at once, then every
/// `CHECK_EVERY`. Each new verified download lands in `Dirigent::updater.ready`.
pub fn start(cx: &mut Context<Dirigent>) {
    let Some(endpoint) = endpoint() else { return };
    info!(%endpoint, "checking for updates");
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    std::thread::spawn(move || {
        let mut offered: Option<String> = None;
        // Offline, every check fails: logged once until one works again.
        let mut failing = false;
        loop {
            match check(&endpoint, offered.as_deref()) {
                Ok(ready) => {
                    failing = false;
                    if let Some(ready) = ready {
                        offered = Some(ready.version.clone());
                        if tx.unbounded_send(ready).is_err() {
                            return;
                        }
                    }
                }
                Err(e) if !std::mem::replace(&mut failing, true) => warn!("update check: {e:#}"),
                Err(_) => {}
            }
            std::thread::sleep(CHECK_EVERY);
        }
    });
    cx.spawn(async move |this, cx| {
        use futures::StreamExt;
        while let Some(ready) = rx.next().await {
            info!(version = %ready.version, "update ready");
            let shown = this.update(cx, |this, cx| {
                this.updater.ready = Some(ready);
                cx.notify();
            });
            if shown.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// One check: the channel's release, downloaded and verified if it is newer than this build.
/// `offered`: the version already handed to the UI, not looked at again.
fn check(endpoint: &str, offered: Option<&str>) -> Result<Option<Ready>> {
    let platform = release::platform();
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .user_agent(format!("Dirigent/{} ({platform})", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(20)))
            .build(),
    );
    let url = format!("{endpoint}/{platform}");
    let release: Release = match agent.get(&url).call() {
        Ok(mut res) => serde_json::from_str(&res.body_mut().read_to_string()?).with_context(|| format!("{url}: not a release"))?,
        // Nothing released on this channel for this platform.
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("GET {url}")),
    };
    let dir = dirigent_service::data_dir().join("updates");
    if Version::parse(&release.version)? <= Version::parse(env!("CARGO_PKG_VERSION"))? {
        clean(&dir, None);
        return Ok(None);
    }
    let name = release.url.rsplit('/').next().filter(|n| !n.is_empty() && !n.contains(['\\', ':']) && *n != "..");
    let installer = dir.join(name.context("the release url has no file name")?);
    if offered == Some(&release.version) && installer.exists() {
        return Ok(None);
    }
    let signature = agent.get(format!("{}.minisig", release.url)).call()?.body_mut().read_to_string()?;
    // Tests sign with their own key (debug builds only).
    let test_key = std::env::var("DIRIGENT_UPDATE_KEY").ok().filter(|_| cfg!(debug_assertions));
    let keys: Vec<String> = release::KEYS.iter().map(|k| k.to_string()).chain(test_key).collect();
    let verify = |file: &Path| verify(file, &signature, &release.version, &platform, &keys);
    // A download from an earlier check may be here already.
    if !installer.exists() || verify(&installer).is_err() {
        info!(version = %release.version, "downloading update");
        std::fs::create_dir_all(&dir)?;
        let part = installer.with_extension("part");
        let mut res = agent.get(&release.url).call().with_context(|| format!("GET {}", release.url))?;
        std::io::copy(&mut res.body_mut().as_reader(), &mut std::fs::File::create(&part)?)?;
        verify(&part)?;
        std::fs::rename(&part, &installer)?;
    }
    clean(&dir, Some(&installer));
    Ok(Some(Ready { version: release.version, installer }))
}

/// Whether `file` is `version` for `platform`, signed by one of `keys`.
fn verify(file: &Path, signature: &str, version: &str, platform: &str, keys: &[String]) -> Result<()> {
    let signature = minisign_verify::Signature::decode(signature)?;
    if signature.trusted_comment() != release::trusted_comment(version, platform) {
        bail!("the signature is for {:?}, not {version} on {platform}", signature.trusted_comment());
    }
    for key in keys {
        let key = minisign_verify::PublicKey::from_base64(key)?;
        let Ok(mut stream) = key.verify_stream(&signature) else { continue };
        let mut file = std::fs::File::open(file)?;
        let mut buf = vec![0; 1 << 16];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            stream.update(&buf[..n]);
        }
        if stream.finalize().is_ok() {
            return Ok(());
        }
    }
    bail!("{} isn't signed by a release key", file.display())
}

/// Remove what earlier checks and updates left in `dir` (downloads, an unpacked tarball):
/// everything but `keep`.
fn clean(dir: &Path, keep: Option<&Path>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if keep != Some(path.as_path()) {
            let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        }
    }
}

/// Start the installer, which waits for this process to exit, installs and starts Dirigent
/// again. Windows: the setup exe, silent; `/UPDATE` makes it relaunch the app.
#[cfg(windows)]
fn start_installer(installer: &Path) -> Result<()> {
    std::process::Command::new(installer)
        .args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/UPDATE"])
        .arg(format!("/WAITPID={}", std::process::id()))
        .spawn()
        .with_context(|| format!("start {}", installer.display()))?;
    Ok(())
}

/// Linux: unpack the tarball next to it and run its `dirigent/install.sh --update <pid>`,
/// with this app's `--data-dir` / `--config-dir` to start it again on. Its output goes to
/// `<data dir>/logs/install.log`.
#[cfg(unix)]
fn start_installer(installer: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let dir = installer.with_file_name("unpacked");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let tar = std::process::Command::new("tar").arg("-xzf").arg(installer).arg("-C").arg(&dir).output().context("run tar")?;
    if !tar.status.success() {
        bail!("unpack {}: {}", installer.display(), String::from_utf8_lossy(&tar.stderr).trim());
    }
    let log = std::fs::File::create(dirigent_service::data_dir().join("logs").join("install.log"))?;
    std::process::Command::new("sh")
        .arg(dir.join("dirigent").join("install.sh"))
        .arg("--update")
        .arg(std::process::id().to_string())
        .args(dirigent_service::dir_args())
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // A process group of its own: it outlives this app.
        .process_group(0)
        .spawn()
        .context("start install.sh")?;
    Ok(())
}

impl Dirigent {
    /// Local threads a restart would cut off: working, still being set up, or running
    /// `async_bash` jobs. (Waiting on an answer is fine: asks are kept by the service.)
    pub fn local_busy(&self) -> usize {
        let Some(host) = self.host(dirigent_service::hosts::LOCAL) else { return 0 };
        host.threads.iter().filter(|t| t.running() || t.jobs > 0 || t.setup.is_some()).count()
    }

    /// The update menu's restart: now if no local agent is working, else once none is.
    pub fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        if self.local_busy() == 0 {
            self.apply_update(cx);
        } else {
            self.updater.when_idle = true;
            self.updater.idle_since = None;
        }
        cx.notify();
    }

    pub fn cancel_restart(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        self.updater.when_idle = false;
        cx.notify();
    }

    /// Every second (`tick`): restart once the local agents have been idle `IDLE_FOR`.
    pub fn tick_update(&mut self, cx: &mut Context<Self>) {
        if !self.updater.when_idle {
            return;
        }
        if self.local_busy() > 0 {
            self.updater.idle_since = None;
        } else if self.updater.idle_since.get_or_insert_with(Instant::now).elapsed() >= IDLE_FOR {
            self.updater.when_idle = false;
            self.apply_update(cx);
        }
    }

    /// Install the ready update and start again: start the installer (`start_installer`),
    /// which waits for this process to exit, then quit. Taken from `ready` meanwhile, so a
    /// second restart can't start another installer over the first one.
    fn apply_update(&mut self, cx: &mut Context<Self>) {
        let Some(ready) = self.updater.ready.take() else { return };
        info!(version = %ready.version, "restarting to update");
        // Off the app's borrow and thread: unpacking takes a moment, and on Windows starting
        // a process can pump window messages, which would re-enter the app.
        cx.spawn(async move |this, cx| {
            let installer = ready.installer.clone();
            let started = cx.background_executor().spawn(async move { start_installer(&installer) }).await;
            match started {
                Ok(()) => cx.update(|cx| cx.quit()),
                Err(e) => {
                    error!("start the update: {e:#}");
                    this.update(cx, |this, cx| {
                        this.updater.ready = Some(ready);
                        this.toast(format!("Couldn't start the update: {e:#}"), cx);
                    })
                    .ok();
                }
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Made with the minisign CLI: `minisign -S -m payload -t "dirigent 1.2.3 test-platform"`.
    const KEY: &str = "RWRe9G0q34tFvBAF73zIZUL9dKvcm2lxf6ZaGGrgte90weV+749I+VZ5";
    const PAYLOAD: &[u8] = b"dirigent update test payload\n";
    const SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RURe9G0q34tFvJlpaNkQeP74VVpYbV9BLta4/BglNpka2gfdLWz59EAFQ2+nCD2HHNjwo2MOZfKP40D+xzTIcQu9I9zYfrOGCAo=
trusted comment: dirigent 1.2.3 test-platform
/ISCyMq0dwbO9g4CteEvkBvOBComNxoEEvUtz73AXR1Q2i+NXLU2it0S3G2WgV9mLJYO8cBwuGpPu05u/ToSCg==
";

    #[test]
    fn verifies_signed_release_only() {
        let dir = std::env::temp_dir().join(format!("dirigent-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (good, bad) = (dir.join("good"), dir.join("bad"));
        std::fs::write(&good, PAYLOAD).unwrap();
        std::fs::write(&bad, b"dirigent update test payloaf\n").unwrap();
        let keys = [KEY.to_owned()];
        let check = |file: &Path, version: &str, platform: &str, keys: &[String]| verify(file, SIGNATURE, version, platform, keys);
        assert!(check(&good, "1.2.3", "test-platform", &keys).is_ok());
        // The trusted comment pins version and platform: no replaying an old signature.
        assert!(check(&good, "1.2.4", "test-platform", &keys).is_err());
        assert!(check(&good, "1.2.3", "other-platform", &keys).is_err());
        assert!(check(&bad, "1.2.3", "test-platform", &keys).is_err());
        // The real release keys didn't sign it.
        let release_keys: Vec<String> = release::KEYS.iter().map(|k| k.to_string()).collect();
        assert!(check(&good, "1.2.3", "test-platform", &release_keys).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
