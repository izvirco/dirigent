//! Connects the background updater to application state and shutdown.

use super::*;

impl Dirigent {
    pub(crate) fn check_for_updates(&self) {
        crate::update::check_now(self.update_events.clone());
    }

    pub(super) fn handle_update_event(
        &mut self,
        event: crate::update::UpdateEvent,
        _cx: &mut Context<Self>,
    ) {
        match event {
            crate::update::UpdateEvent::Checked(release) => {
                if matches!(
                    self.update_state,
                    crate::update::UpdateState::Downloading { .. }
                        | crate::update::UpdateState::Ready { .. }
                ) {
                    return;
                }
                self.update_state = release.map_or(
                    crate::update::UpdateState::Current,
                    crate::update::UpdateState::Available,
                );
            }
            crate::update::UpdateEvent::CheckFailed(error) => {
                tracing::warn!(%error, "update check failed");
                if matches!(self.update_state, crate::update::UpdateState::Checking) {
                    self.update_state = crate::update::UpdateState::Current;
                }
            }
            crate::update::UpdateEvent::DownloadProgress { downloaded, total } => {
                if let crate::update::UpdateState::Downloading {
                    downloaded: current,
                    total: expected,
                    ..
                } = &mut self.update_state
                {
                    *current = downloaded;
                    *expected = total;
                }
            }
            crate::update::UpdateEvent::Downloaded { release, path } => {
                self.update_state = crate::update::UpdateState::Ready { release, path };
            }
            crate::update::UpdateEvent::DownloadFailed { release, error } => {
                tracing::error!(%error, "update download failed");
                self.update_state = crate::update::UpdateState::Failed { release };
            }
        }
    }

    pub(crate) fn activate_update(&mut self, cx: &mut Context<Self>) {
        if let crate::update::UpdateState::Ready { release, path } = &self.update_state {
            let release = release.clone();
            let path = path.clone();
            if crate::update::dry_run_enabled() {
                tracing::info!(
                    version = %release.version,
                    path = %path.display(),
                    "update dry run completed; skipping installation and restart"
                );
                let _ = std::fs::remove_file(path);
                self.update_state = crate::update::UpdateState::Available(release);
                cx.notify();
                return;
            }
            match crate::update::launch_updater(&path) {
                Ok(()) => cx.quit(),
                Err(error) => {
                    tracing::error!(%error, "could not start update");
                    let _ = std::fs::remove_file(path);
                    self.update_state = crate::update::UpdateState::Failed { release };
                    cx.notify();
                }
            }
            return;
        }

        let release = match &self.update_state {
            crate::update::UpdateState::Available(release)
            | crate::update::UpdateState::Failed { release, .. } => release.clone(),
            _ => return,
        };
        let total = crate::update::artifact_size(&release);
        self.update_state = crate::update::UpdateState::Downloading {
            release: release.clone(),
            downloaded: 0,
            total,
        };
        crate::update::download(release, self.update_events.clone());
        cx.notify();
    }
}
