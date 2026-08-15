//! Connects the background updater to application state and shutdown.

use super::*;

impl Dirigent {
    pub(super) fn handle_update_event(
        &mut self,
        event: crate::update::UpdateEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::update::UpdateEvent::Checked(release) => {
                if matches!(
                    self.update_state,
                    crate::update::UpdateState::Downloading(_)
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
            crate::update::UpdateEvent::Downloaded { release, path } => {
                match crate::update::launch_updater(&path) {
                    Ok(lock) => {
                        self.update_shutdown_lock = Some(lock);
                        cx.quit();
                    }
                    Err(error) => {
                        tracing::error!(%error, "could not start update");
                        self.update_state = crate::update::UpdateState::Failed { release };
                    }
                }
            }
            crate::update::UpdateEvent::DownloadFailed { release, error } => {
                tracing::error!(%error, "update download failed");
                self.update_state = crate::update::UpdateState::Failed { release };
            }
        }
    }

    pub(crate) fn install_available_update(&mut self, cx: &mut Context<Self>) {
        let release = match &self.update_state {
            crate::update::UpdateState::Available(release)
            | crate::update::UpdateState::Failed { release, .. } => release.clone(),
            _ => return,
        };
        self.update_state = crate::update::UpdateState::Downloading(release.clone());
        crate::update::download(release, self.update_events.clone());
        cx.notify();
    }
}
