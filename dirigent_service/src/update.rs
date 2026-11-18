use anyhow::{Context, Result, bail, ensure};
use dirigent_protocol::release::{self, Release};
use semver::Version;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{error, info};

pub(crate) const REPLY_FLUSH_GRACE_BEFORE_RESTART: Duration = Duration::from_millis(500);

pub struct Transfer {
    pub received: u64,
    pub total: Option<u64>,
    pub per_second: f64,
}

pub struct Updater {
    endpoint: String,
    dir: PathBuf,
    agent: ureq::Agent,
}

impl Updater {
    /// The desktop's. `None` in a build that never updates itself: the release workflow
    /// baked no channel in (a dev or nix build) and `DIRIGENT_UPDATE_URL` names none (tests).
    pub fn desktop() -> Option<Self> {
        Self::new("updates", "")
    }

    fn service() -> Option<Self> {
        Self::new("updates-service", "; service")
    }

    // MUST KILL: rename: endpoint's two env vars → a baked BUILD_UPDATE_URL const and a run-time override fn
    fn new(dir: &str, who: &str) -> Option<Self> {
        let url = std::env::var("DIRIGENT_UPDATE_URL").ok().or_else(|| option_env!("DIRIGENT_BUILD_UPDATE_URL").map(str::to_owned));
        let endpoint = url.filter(|u| !u.is_empty())?.trim_end_matches('/').to_owned();
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .user_agent(format!("Dirigent/{} ({}{who})", release::VERSION, release::platform()))
                .timeout_connect(Some(Duration::from_secs(20)))
                .timeout_per_read(Some(Duration::from_secs(30)))
                .build(),
        );
        Some(Self { endpoint, dir: crate::data_dir().join(dir), agent })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The channel's release, if it is newer than this build. When it isn't, nothing
    /// downloaded before is kept.
    pub fn newer(&self) -> Result<Option<Release>> {
        let url = format!("{}/{}", self.endpoint, release::platform());
        let release: Release = match self.agent.get(&url).call() {
            Ok(mut res) => serde_json::from_str(&res.body_mut().read_to_string()?).with_context(|| format!("{url}: not a release"))?,
            Err(ureq::Error::StatusCode(404)) => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("GET {url}")),
        };
        if Version::parse(&release.version)? <= Version::parse(release::VERSION)? {
            remove_all_but(&self.dir, None);
            return Ok(None);
        }
        Ok(Some(release))
    }

    /// Where `download` puts the release's installer.
    pub fn installer(&self, release: &Release) -> Result<PathBuf> {
        let name = release.url.rsplit('/').next().filter(|n| !n.is_empty() && !n.contains(['\\', ':']) && *n != "..");
        Ok(self.dir.join(name.context("the release url has no file name")?))
    }

    /// The release's installer, signed by a release key for this version and platform:
    /// the one downloaded before, or downloaded now, which `progress` hears of every
    /// second. Every other download is removed.
    pub fn download(&self, release: &Release, progress: impl Fn(Transfer)) -> Result<PathBuf> {
        let installer = self.installer(release)?;
        let signature = format!("{}.minisig", release.url);
        let signature = self.agent.get(&signature).call().with_context(|| format!("GET {signature}"))?.body_mut().read_to_string()?;
        let test_key = std::env::var("DIRIGENT_UPDATE_KEY").ok().filter(|_| cfg!(debug_assertions));
        let keys: Vec<String> = release::KEYS.iter().map(|k| k.to_string()).chain(test_key).collect();
        let verify = |file: &Path| verify(file, &signature, &release.version, &release::platform(), &keys);
        if !installer.exists() || verify(&installer).is_err() {
            info!(version = %release.version, "downloading update");
            std::fs::create_dir_all(&self.dir)?;
            let part = installer.with_extension("part");
            let mut res = self.agent.get(&release.url).call().with_context(|| format!("GET {}", release.url))?;
            let total = res.body().content_length();
            let sample = |received, per_second| progress(Transfer { received, total, per_second });
            let (mut body, mut file) = (res.body_mut().as_reader(), std::fs::File::create(&part)?);
            let (mut buf, mut done, mut speed, mut mark) = (vec![0; 1 << 16], 0u64, None, (Instant::now(), 0u64));
            loop {
                let n = body.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n])?;
                done += n as u64;
                let since = mark.0.elapsed();
                if since >= Duration::from_secs(1) {
                    let per_second = (done - mark.1) as f64 / since.as_secs_f64();
                    (speed, mark) = (Some(per_second), (Instant::now(), done));
                    sample(done, per_second);
                }
            }
            drop(file);
            sample(done, speed.unwrap_or_else(|| done as f64 / mark.0.elapsed().as_secs_f64()));
            verify(&part)?;
            std::fs::rename(&part, &installer)?;
        }
        remove_all_but(&self.dir, Some(&installer));
        Ok(installer)
    }
}

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

fn remove_all_but(dir: &Path, keep: Option<&Path>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if keep != Some(path.as_path()) {
            let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        }
    }
}

/// Unpack a release's tarball beside it. Returns its `install.sh`.
#[cfg(unix)]
pub fn unpack(installer: &Path) -> Result<PathBuf> {
    let dir = installer.with_file_name("unpacked");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let tar = Command::new("tar").arg("-xzf").arg(installer).arg("-C").arg(&dir).output().context("run tar")?;
    if !tar.status.success() {
        bail!("unpack {}: {}", installer.display(), String::from_utf8_lossy(&tar.stderr).trim());
    }
    Ok(dir.join("dirigent").join("install.sh"))
}

/// An install swaps the file under the running process, which then no longer knows where it
/// came from.
static SELF_UPDATING_EXE: OnceLock<PathBuf> = OnceLock::new();
static UPDATE_STARTED: AtomicBool = AtomicBool::new(false);

/// Lets clients update this service: `dirigent_service`'s `main` says so, as the process
/// is the service and nothing else. The service a desktop embeds is updated with the
/// desktop, by whoever sits at it.
pub fn enable() {
    let exe = std::env::current_exe().and_then(|exe| if cfg!(windows) { Ok(exe) } else { exe.canonicalize() });
    match exe {
        Ok(exe) => drop(SELF_UPDATING_EXE.set(exe)),
        Err(e) => error!("no updates: this executable's path is unknown: {e}"),
    }
}

fn self_updater() -> Option<Updater> {
    let exe = SELF_UPDATING_EXE.get()?;
    if cfg!(unix) && install_prefix(exe).is_none() {
        return None;
    }
    let restart_keeps_arguments = cfg!(unix);
    if !restart_keeps_arguments && !crate::dir_args().is_empty() {
        return None;
    }
    Updater::service()
}

fn install_prefix(exe: &Path) -> Option<&Path> {
    let dir = exe.parent()?;
    let lib = dir.parent()?;
    let installed = dir.file_name()? == release::app() && lib.file_name()? == "lib" && dir.join("install.sh").is_file();
    lib.parent().filter(|_| installed)
}

pub(crate) fn available() -> Result<Option<String>> {
    let Some(updater) = self_updater() else { return Ok(None) };
    Ok(updater.newer()?.map(|release| release.version))
}

pub(crate) fn install() -> Result<()> {
    let updater = self_updater().context("this service doesn't update itself")?;
    ensure!(!UPDATE_STARTED.swap(true, Ordering::SeqCst), "an update is being installed already");
    let installed = (|| {
        let release = updater.newer()?.context("there is no newer release")?;
        info!(version = %release.version, "updating");
        run_installer(&updater.download(&release, |_| {})?)
    })();
    if installed.is_err() {
        UPDATE_STARTED.store(false, Ordering::SeqCst);
    }
    installed
}

#[cfg(unix)]
fn run_installer(installer: &Path) -> Result<()> {
    let prefix = SELF_UPDATING_EXE.get().and_then(|exe| install_prefix(exe)).context("not running from an install")?;
    let script = unpack(installer)?;
    let out = Command::new("sh").arg(&script).arg("--update-service").env("DIRIGENT_PREFIX", prefix).output().context("run install.sh")?;
    let said = String::from_utf8_lossy(&[out.stdout, out.stderr].concat()).trim().to_owned();
    ensure!(out.status.success(), "install.sh failed ({}): {said}", out.status);
    info!(prefix = %prefix.display(), %said, "installed");
    Ok(())
}

#[cfg(windows)]
fn run_installer(installer: &Path) -> Result<()> {
    Command::new(installer)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SERVICE"])
        .arg(format!("/WAITPID={}", std::process::id()))
        .spawn()
        .with_context(|| format!("start {}", installer.display()))?;
    Ok(())
}

/// On unix this process becomes the new executable, with the arguments and environment it
/// has: whatever started the service (systemd, launchd, a terminal) keeps the process it knows.
pub(crate) fn restart() -> ! {
    info!("restarting to update");
    crate::shutdown();
    crate::tunnel::close_all();
    #[cfg(unix)]
    if let Some(exe) = SELF_UPDATING_EXE.get() {
        use std::os::unix::process::CommandExt;
        let failed = Command::new(exe).args(std::env::args_os().skip(1)).exec();
        error!("start {}: {failed}", exe.display());
        std::process::exit(1);
    }
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "RWRe9G0q34tFvBAF73zIZUL9dKvcm2lxf6ZaGGrgte90weV+749I+VZ5";
    const PAYLOAD: &[u8] = b"dirigent update test payload\n";
    const SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RURe9G0q34tFvJlpaNkQeP74VVpYbV9BLta4/BglNpka2gfdLWz59EAFQ2+nCD2HHNjwo2MOZfKP40D+xzTIcQu9I9zYfrOGCAo=
trusted comment: dirigent 1.2.3 test-platform
/ISCyMq0dwbO9g4CteEvkBvOBComNxoEEvUtz73AXR1Q2i+NXLU2it0S3G2WgV9mLJYO8cBwuGpPu05u/ToSCg==
";

    #[test]
    fn verifies_signed_release_only() {
        let dir = tempfile::tempdir().unwrap();
        let (good, bad) = (dir.path().join("good"), dir.path().join("bad"));
        std::fs::write(&good, PAYLOAD).unwrap();
        std::fs::write(&bad, b"dirigent update test payloaf\n").unwrap();
        let keys = [KEY.to_owned()];
        let check = |file: &Path, version: &str, platform: &str, keys: &[String]| verify(file, SIGNATURE, version, platform, keys);
        assert!(check(&good, "1.2.3", "test-platform", &keys).is_ok());
        assert!(check(&good, "1.2.4", "test-platform", &keys).is_err());
        assert!(check(&good, "1.2.3", "other-platform", &keys).is_err());
        assert!(check(&bad, "1.2.3", "test-platform", &keys).is_err());
        let release_keys: Vec<String> = release::KEYS.iter().map(|k| k.to_string()).collect();
        assert!(check(&good, "1.2.3", "test-platform", &release_keys).is_err());
    }

    #[test]
    fn only_an_install_has_a_prefix() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("lib").join(release::app());
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("dirigent_service");
        assert_eq!(install_prefix(&exe), None);
        std::fs::write(dir.join("install.sh"), "").unwrap();
        assert_eq!(install_prefix(&exe), Some(root.path()));
        let unpacked = root.path().join("dirigent");
        std::fs::create_dir_all(&unpacked).unwrap();
        std::fs::write(unpacked.join("install.sh"), "").unwrap();
        assert_eq!(install_prefix(&unpacked.join("dirigent_service")), None);
    }
}
