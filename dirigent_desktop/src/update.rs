use crate::app::{Dirigent, Menu};
use crate::net::HostStatus;
use crate::projects::HostState;
use anyhow::{Context as _, Result};
use dirigent_protocol::ClientMessage;
use dirigent_service::update::{Transfer, Updater};
use gpui::Context;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

const CHECK_EVERY: Duration = Duration::from_secs(5 * 60);
pub const HOST_CHECK_SECS: u64 = 5 * 60;
// MUST KILL: rename: IDLE_FOR → SUB_AGENT_HANDOFF_GRACE
const IDLE_FOR: Duration = Duration::from_secs(3);

// MUST KILL: rename: Ready → VerifiedInstaller, apply_update runs it unchecked
#[derive(Clone)]
pub struct Ready {
    pub version: String,
    pub installer: PathBuf,
}

pub struct Download {
    pub version: String,
    pub transfer: Option<Transfer>,
}

enum Report {
    Progress(Option<Download>),
    Ready(Ready),
}

#[derive(Default)]
pub struct State {
    pub ready: Option<Ready>,
    pub downloading: Option<Download>,
    pub when_idle: bool,
    idle_since: Option<Instant>,
}

#[derive(Default)]
pub struct HostUpdate {
    unanswered_check: Option<u64>,
    pub offer: Option<Offer>,
}

pub struct Offer {
    pub version: String,
    pub stage: Stage,
}

impl HostUpdate {
    pub(crate) fn disconnected(&mut self) {
        self.unanswered_check = None;
        self.offer.take_if(|offer| !matches!(offer.stage, Stage::WhenIdle { .. }));
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Stage {
    Offered,
    WhenIdle { idle_since: Option<Instant> },
    Installing { request: u64 },
}

pub fn start(cx: &mut Context<Dirigent>) {
    let Some(updater) = Updater::desktop() else { return };
    info!(endpoint = %updater.endpoint(), "checking for updates");
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    std::thread::spawn(move || {
        let mut offered: Option<String> = None;
        let mut failing = false;
        let progress = |download| {
            let _ = tx.unbounded_send(Report::Progress(Some(download)));
        };
        loop {
            match check(&updater, offered.as_deref(), progress) {
                Ok(ready) => {
                    failing = false;
                    if let Some(ready) = ready {
                        offered = Some(ready.version.clone());
                        if tx.unbounded_send(Report::Ready(ready)).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.unbounded_send(Report::Progress(None));
                    if !std::mem::replace(&mut failing, true) {
                        warn!("update check: {e:#}");
                    }
                }
            }
            std::thread::sleep(CHECK_EVERY);
        }
    });
    cx.spawn(async move |this, cx| {
        use futures::StreamExt;
        while let Some(report) = rx.next().await {
            let shown = this.update(cx, |this, cx| {
                match report {
                    Report::Progress(download) => this.updater.downloading = download,
                    Report::Ready(ready) => {
                        info!(version = %ready.version, "update ready");
                        this.updater.ready = Some(ready);
                        this.updater.downloading = None;
                    }
                }
                this.close_spent_update_menu();
                cx.notify();
            });
            if shown.is_err() {
                break;
            }
        }
    })
    .detach();
}

fn check(updater: &Updater, offered: Option<&str>, progress: impl Fn(Download)) -> Result<Option<Ready>> {
    let Some(release) = updater.newer()? else { return Ok(None) };
    if offered == Some(&release.version) && updater.installer(&release)?.exists() {
        return Ok(None);
    }
    let report = |transfer| progress(Download { version: release.version.clone(), transfer });
    report(None);
    let installer = updater.download(&release, |transfer| report(Some(transfer)))?;
    Ok(Some(Ready { version: release.version, installer }))
}

#[cfg(windows)]
fn start_installer(installer: &Path) -> Result<()> {
    std::process::Command::new(installer)
        .args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/UPDATE"])
        .arg(format!("/WAITPID={}", std::process::id()))
        .spawn()
        .with_context(|| format!("start {}", installer.display()))?;
    Ok(())
}

#[cfg(unix)]
fn start_installer(installer: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let script = dirigent_service::update::unpack(installer)?;
    let log = std::fs::File::create(dirigent_service::data_dir().join("logs").join("install.log"))?;
    std::process::Command::new("sh")
        .arg(script)
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

fn settled(busy: bool, idle_since: &mut Option<Instant>) -> bool {
    if busy {
        *idle_since = None;
        return false;
    }
    idle_since.get_or_insert_with(Instant::now).elapsed() >= IDLE_FOR
}

impl HostState {
    pub fn threads_a_restart_would_cut_off(&self) -> usize {
        self.threads.iter().filter(|t| t.running() || t.jobs > 0 || t.setup.is_some()).count()
    }
}

impl Dirigent {
    // MUST KILL: rename: local_busy → threads_a_restart_would_cut_off (jobs and setup count, asks don't)
    pub fn local_busy(&self) -> usize {
        self.host(dirigent_service::hosts::LOCAL).map_or(0, HostState::threads_a_restart_would_cut_off)
    }

    pub fn has_updates(&self) -> bool {
        self.updater.ready.is_some() || self.updater.downloading.is_some() || self.hosts.iter().any(|h| h.update.offer.is_some())
    }

    // MUST KILL: rearchitecture: close_spent_update_menu → a Menu::Update that cannot outlive its button
    pub(crate) fn close_spent_update_menu(&mut self) {
        if self.menu == Some(Menu::Update) && !self.has_updates() {
            self.menu = None;
        }
    }

    pub(crate) fn check_host_update(&mut self, host: &str) {
        let Some(h) = self.host(host) else { return };
        let installing = matches!(h.update.offer, Some(Offer { stage: Stage::Installing { .. }, .. }));
        if h.status != HostStatus::Connected || h.update.unanswered_check.is_some() || installing {
            return;
        }
        let id = self.request(host, |id| ClientMessage::CheckUpdate { id });
        if let Some(h) = self.host_mut(host) {
            h.update.unanswered_check = Some(id);
        }
    }

    pub(crate) fn check_host_updates(&mut self) {
        let hosts: Vec<String> = self.hosts.iter().map(|h| h.name.clone()).collect();
        for host in hosts {
            self.check_host_update(&host);
        }
    }

    pub(crate) fn on_host_update(&mut self, host: &str, version: Option<String>) {
        let Some(h) = self.host_mut(host) else { return };
        h.update.unanswered_check = None;
        h.update.offer = match (version, h.update.offer.take()) {
            (Some(version), Some(offer)) if offer.version == version => Some(offer),
            (Some(version), _) => Some(Offer { version, stage: Stage::Offered }),
            (None, _) => None,
        };
        self.close_spent_update_menu();
    }

    pub(crate) fn on_host_update_reply(&mut self, host: &str, request: u64, result: Result<(), String>, cx: &mut Context<Self>) -> bool {
        let Some(h) = self.host_mut(host) else { return false };
        if h.update.unanswered_check == Some(request) {
            h.update.unanswered_check = None;
            if let Err(why) = result {
                info!(%host, "no update check: {why}");
            }
            return true;
        }
        let Some(offer) = h.update.offer.as_mut().filter(|o| o.stage == Stage::Installing { request }) else { return false };
        let message = match result {
            Ok(()) => format!("{host} restarts as Dirigent {}", offer.version),
            Err(why) => {
                offer.stage = Stage::Offered;
                format!("{host}: the update failed: {why}")
            }
        };
        self.toast(message, cx);
        true
    }

    pub fn update_host(&mut self, host: &str, cx: &mut Context<Self>) {
        self.menu = None;
        if self.host(host).is_some_and(|h| h.threads_a_restart_would_cut_off() == 0) {
            self.install_host_update(host);
        } else if let Some(offer) = self.host_mut(host).and_then(|h| h.update.offer.as_mut()) {
            offer.stage = Stage::WhenIdle { idle_since: None };
        }
        cx.notify();
    }

    pub fn cancel_host_update(&mut self, host: &str, cx: &mut Context<Self>) {
        self.menu = None;
        let offer = self.host_mut(host).and_then(|h| h.update.offer.as_mut());
        if let Some(offer) = offer.filter(|o| matches!(o.stage, Stage::WhenIdle { .. })) {
            offer.stage = Stage::Offered;
        }
        cx.notify();
    }

    fn install_host_update(&mut self, host: &str) {
        if self.host(host).is_none_or(|h| h.update.offer.is_none()) {
            return;
        }
        let id = self.request(host, |id| ClientMessage::InstallUpdate { id });
        if let Some(offer) = self.host_mut(host).and_then(|h| h.update.offer.as_mut()) {
            info!(%host, version = %offer.version, "telling the host to update");
            offer.stage = Stage::Installing { request: id };
        }
    }

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

    pub fn tick_update(&mut self, cx: &mut Context<Self>) {
        if self.updater.when_idle && settled(self.local_busy() > 0, &mut self.updater.idle_since) {
            self.updater.when_idle = false;
            self.apply_update(cx);
        }
        let idle = |h: &mut HostState| {
            let busy = h.status != HostStatus::Connected || h.threads_a_restart_would_cut_off() > 0;
            let Some(Offer { stage: Stage::WhenIdle { idle_since }, .. }) = &mut h.update.offer else { return None };
            settled(busy, idle_since).then(|| h.name.clone())
        };
        let hosts: Vec<String> = self.hosts.iter_mut().filter_map(idle).collect();
        for host in hosts {
            self.install_host_update(&host);
            cx.notify();
        }
    }

    // MUST KILL: type: apply_update takes updater.ready as its in-flight guard → an explicit Installing state
    fn apply_update(&mut self, cx: &mut Context<Self>) {
        let Some(ready) = self.updater.ready.take() else { return };
        info!(version = %ready.version, "restarting to update");
        cx.spawn(async move |this, cx| {
            let installer = ready.installer.clone();
            let started = cx.background_executor().spawn(async move { start_installer(&installer) }).await;
            match started {
                Ok(()) => {
                    info!("installer started, quitting");
                    cx.update(|cx| cx.quit())
                }
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
