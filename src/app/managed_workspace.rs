use super::*;
use crate::{
    model::{ManagedWorkspace, WorkspaceBackend, WorkspaceState},
    vcs::{self, RepositorySnapshot},
};

impl Dirigent {
    pub(crate) fn refresh_repository(&mut self, project_id: Id) {
        let Some(path) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.path.clone())
        else {
            return;
        };
        match vcs::probe_repository(&path) {
            Ok(Some(snapshot)) => {
                self.repository_snapshots.insert(project_id, snapshot);
            }
            Ok(None) => {
                self.repository_snapshots.remove(&project_id);
            }
            Err(error) => {
                self.repository_snapshots.remove(&project_id);
                self.banner = Some(error);
            }
        }
    }

    pub(crate) fn toggle_workspace_dropdown(&mut self) {
        if self.composer_dropdown == Some(ComposerDropdown::Workspace) {
            self.composer_dropdown = None;
            return;
        }
        if let Some(project_id) = self.selected_project {
            self.refresh_repository(project_id);
        }
        self.composer_dropdown = Some(ComposerDropdown::Workspace);
    }

    pub(crate) fn pending_workspace_source(&self) -> Option<&RepositorySnapshot> {
        if self.creating_harness {
            return self.draft_workspace_source.as_ref();
        }
        self.selected_harness
            .and_then(|id| self.pending_workspace_sources.get(&id))
    }

    pub(crate) fn selected_managed_workspace(&self) -> Option<&ManagedWorkspace> {
        let workspace_id = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))?
            .workspace_id
            .as_deref()?;
        self.workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
    }

    pub(crate) fn repository_snapshot_for_composer(&self) -> Option<&RepositorySnapshot> {
        self.selected_project
            .and_then(|project_id| self.repository_snapshots.get(&project_id))
    }

    pub(crate) fn choose_new_workspace(&mut self) {
        let Some(project_id) = self.selected_project else {
            return;
        };
        self.refresh_repository(project_id);
        let Some(snapshot) = self.repository_snapshots.get(&project_id).cloned() else {
            self.banner = Some("This project is not in a supported JJ or Git repository.".into());
            return;
        };
        if self.creating_harness {
            self.draft_workspace_source = Some(snapshot);
        } else if let Some(harness_id) = self.selected_harness
            && self
                .harnesses
                .iter()
                .find(|harness| harness.id == harness_id)
                .is_some_and(|harness| {
                    harness.workspace_id.is_none() && harness.status != HarnessStatus::Working
                })
        {
            self.pending_workspace_sources.insert(harness_id, snapshot);
        }
        self.composer_dropdown = None;
    }

    pub(crate) fn use_project_directory(&mut self) {
        if self.creating_harness {
            self.draft_workspace_source = None;
        } else if let Some(harness_id) = self.selected_harness {
            self.pending_workspace_sources.remove(&harness_id);
        }
        self.composer_dropdown = None;
    }

    pub(crate) fn workspace_for_harness(&self, harness_id: Id) -> Option<&ManagedWorkspace> {
        let workspace_id = self
            .harnesses
            .iter()
            .find(|harness| harness.id == harness_id)?
            .workspace_id
            .as_deref()?;
        self.workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
    }

    pub(crate) fn can_delete_workspace_for_harness(&self, harness_id: Id) -> bool {
        let Some(workspace) = self.workspace_for_harness(harness_id) else {
            return false;
        };
        workspace.state != WorkspaceState::Provisioning
            && self
                .harnesses
                .iter()
                .filter(|harness| harness.workspace_id.as_deref() == Some(workspace.id.as_str()))
                .count()
                == 1
            && !self.deleting_workspace_harnesses.contains(&harness_id)
    }

    pub(crate) fn begin_delete_thread_and_workspace(&mut self, harness_id: Id) {
        if !self.can_delete_workspace_for_harness(harness_id) {
            self.banner = Some(
                "This workspace cannot be deleted while it is being created or shared by another thread."
                    .into(),
            );
            self.sidebar_menu = None;
            return;
        }
        self.pending_workspace_deletion = Some(harness_id);
        self.sidebar_menu = None;
        self.composer_dropdown = None;
    }

    pub(crate) fn cancel_workspace_deletion(&mut self) {
        self.pending_workspace_deletion = None;
    }

    pub(crate) fn confirm_workspace_deletion(&mut self) {
        let Some(harness_id) = self.pending_workspace_deletion.take() else {
            return;
        };
        if !self.can_delete_workspace_for_harness(harness_id) {
            self.banner = Some(
                "This workspace is now being created, deleted, or used by another thread.".into(),
            );
            return;
        }
        let Some(workspace) = self.workspace_for_harness(harness_id).cloned() else {
            return;
        };
        if let Some(harness) = self
            .harnesses
            .iter_mut()
            .find(|harness| harness.id == harness_id)
        {
            if let Some(process) = harness.process.take() {
                process.stop();
            }
            harness.status = HarnessStatus::Stopped;
            harness.run_started_at = None;
        }
        self.deleting_workspace_harnesses.insert(harness_id);
        self.persist();

        let workspace_id = workspace.id.clone();
        let events = self.workspace_events.clone();
        if let Err(error) = std::thread::Builder::new()
            .name(format!("remove-workspace-{workspace_id}"))
            .spawn(move || {
                let event = match vcs::remove_workspace(&workspace) {
                    Ok(()) => WorkspaceEvent::Removed {
                        workspace_id: workspace.id,
                        harness_id,
                    },
                    Err(error) => WorkspaceEvent::RemoveFailed { harness_id, error },
                };
                let _ = events.send_blocking(event);
            })
        {
            self.deleting_workspace_harnesses.remove(&harness_id);
            self.banner = Some(format!("could not start workspace removal: {error}"));
        }
    }

    pub(crate) fn workspace_deletion_pending(&self, harness_id: Id) -> bool {
        self.deleting_workspace_harnesses.contains(&harness_id)
    }

    fn allocate_workspace_id(&self, parent: &std::path::Path) -> Result<String, String> {
        for _ in 0..128 {
            let id = (0..8)
                .map(|_| char::from(b'a' + fastrand::u8(0..26)))
                .collect::<String>();
            if !parent.join(&id).exists()
                && !self.workspaces.iter().any(|workspace| workspace.id == id)
            {
                return Ok(id);
            }
        }
        Err("could not allocate a unique workspace identifier".into())
    }

    fn workspace_parent(&self, project_id: Id) -> Result<PathBuf, String> {
        let project = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .ok_or_else(|| "The workspace project no longer exists.".to_string())?;
        if let Some(root) = project.workspace_root.as_ref() {
            return Ok(root.clone());
        }
        Ok(platform::workspace_root()?.join(vcs::project_slug(&project.name)))
    }

    fn make_managed_workspace(
        &self,
        project_id: Id,
        snapshot: RepositorySnapshot,
    ) -> Result<ManagedWorkspace, String> {
        let parent = self.workspace_parent(project_id)?;
        let id = self.allocate_workspace_id(&parent)?;
        let root = parent.join(&id);
        let working_directory = root.join(&snapshot.project_relative_path);
        let git_branch =
            (snapshot.backend == WorkspaceBackend::Git).then(|| format!("dirigent/{id}"));
        Ok(ManagedWorkspace {
            id,
            project_id,
            backend: snapshot.backend,
            root,
            working_directory,
            source_repository: snapshot.repository_root,
            source_id: snapshot.source_id,
            source_label: snapshot.source_label,
            source_revision: snapshot.source_revision,
            jj_parent_revisions: snapshot.jj_parent_revisions,
            git_branch,
            state: WorkspaceState::Provisioning,
        })
    }

    pub(super) fn provision_harness_workspace(
        &mut self,
        harness_id: Id,
        snapshot: RepositorySnapshot,
    ) -> bool {
        let Some(index) = self
            .harnesses
            .iter()
            .position(|harness| harness.id == harness_id)
        else {
            return false;
        };
        let project_id = self.harnesses[index].project_id;
        let workspace = match self.make_managed_workspace(project_id, snapshot) {
            Ok(workspace) => workspace,
            Err(error) => {
                self.fail_harness(index, error);
                return false;
            }
        };
        let workspace_id = workspace.id.clone();
        if let Some(process) = self.harnesses[index].process.take() {
            process.stop();
        }
        self.harnesses[index].workspace_id = Some(workspace_id.clone());
        self.harnesses[index].status = HarnessStatus::Starting;
        self.harnesses[index].run_started_at = None;
        self.pending_workspace_sources.remove(&harness_id);
        self.workspaces.push(workspace.clone());
        self.persist();

        let events = self.workspace_events.clone();
        std::thread::Builder::new()
            .name(format!("workspace-{workspace_id}"))
            .spawn(move || {
                let event = match vcs::create_workspace(&workspace) {
                    Ok(()) => WorkspaceEvent::Created(workspace.id),
                    Err(error) => WorkspaceEvent::Failed(workspace.id, error),
                };
                let _ = events.send_blocking(event);
            })
            .map(|_| true)
            .unwrap_or_else(|error| {
                if let Some(workspace) = self
                    .workspaces
                    .iter_mut()
                    .find(|workspace| workspace.id == workspace_id)
                {
                    workspace.state = WorkspaceState::Failed(error.to_string());
                }
                self.fail_harness(
                    index,
                    format!("could not start workspace creation: {error}"),
                );
                false
            })
    }

    pub(crate) fn handle_workspace_event(&mut self, event: WorkspaceEvent, cx: &mut Context<Self>) {
        match event {
            WorkspaceEvent::Created(workspace_id) => {
                let Some(workspace) = self
                    .workspaces
                    .iter_mut()
                    .find(|workspace| workspace.id == workspace_id)
                else {
                    return;
                };
                workspace.state = WorkspaceState::Ready;
                let working_directory = workspace.working_directory.clone();
                match start_fuzzy_index(
                    &working_directory,
                    true,
                    FuzzyIndexReady::Workspace(workspace_id.clone()),
                    self.fuzzy_index_events.clone(),
                ) {
                    Ok(picker) => {
                        self.workspace_file_pickers
                            .insert(workspace_id.clone(), picker);
                    }
                    Err(error) => self.banner = Some(error),
                }
                self.persist();
                let harness_ids = self
                    .harnesses
                    .iter()
                    .filter(|harness| harness.workspace_id.as_deref() == Some(&workspace_id))
                    .map(|harness| harness.id)
                    .collect::<Vec<_>>();
                for harness_id in harness_ids {
                    self.start_harness(harness_id, None);
                }
            }
            WorkspaceEvent::Failed(workspace_id, error) => {
                if let Some(workspace) = self
                    .workspaces
                    .iter_mut()
                    .find(|workspace| workspace.id == workspace_id)
                {
                    workspace.state = WorkspaceState::Failed(error.clone());
                }
                let indexes = self
                    .harnesses
                    .iter()
                    .enumerate()
                    .filter(|(_, harness)| harness.workspace_id.as_deref() == Some(&workspace_id))
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>();
                for index in indexes {
                    self.fail_harness(index, error.clone());
                }
                self.persist();
            }
            WorkspaceEvent::Removed {
                workspace_id,
                harness_id,
            } => {
                self.deleting_workspace_harnesses.remove(&harness_id);
                if let Some(picker) = self.workspace_file_pickers.remove(&workspace_id) {
                    picker.cancel();
                }
                self.workspaces
                    .retain(|workspace| workspace.id != workspace_id);
                self.delete_harness(harness_id);
                self.persist();
            }
            WorkspaceEvent::RemoveFailed { harness_id, error } => {
                self.deleting_workspace_harnesses.remove(&harness_id);
                self.banner = Some(error);
                self.persist();
            }
        }
        cx.notify();
    }

    pub(crate) fn working_directory_for_harness(&self, harness_id: Id) -> Result<PathBuf, String> {
        let harness = self
            .harnesses
            .iter()
            .find(|harness| harness.id == harness_id)
            .ok_or_else(|| "Thread no longer exists.".to_string())?;
        if let Some(workspace_id) = harness.workspace_id.as_deref() {
            let workspace = self
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .ok_or_else(|| "The thread's managed workspace is missing.".to_string())?;
            return match &workspace.state {
                WorkspaceState::Ready if workspace.working_directory.is_dir() => {
                    Ok(workspace.working_directory.clone())
                }
                WorkspaceState::Ready => Err(format!(
                    "The managed workspace is missing: {}",
                    workspace.working_directory.display()
                )),
                WorkspaceState::Provisioning => Err("The workspace is still being created.".into()),
                WorkspaceState::Failed(error) => Err(error.clone()),
            };
        }
        self.projects
            .iter()
            .find(|project| project.id == harness.project_id)
            .map(|project| project.path.clone())
            .ok_or_else(|| "The project for this thread no longer exists.".to_string())
    }

    pub(crate) fn harness_has_devshell(&self, harness_id: Id) -> bool {
        self.working_directory_for_harness(harness_id)
            .is_ok_and(|path| project_has_devshell(&path))
    }

    pub(crate) fn open_project_settings(&mut self, project_id: Id, cx: &mut Context<Self>) {
        self.refresh_repository(project_id);
        let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
        else {
            return;
        };
        let value = project
            .workspace_root
            .as_ref()
            .map(|path| path.display().to_string())
            .or_else(|| {
                self.default_workspace_path_for_project(project_id)
                    .map(|path| path.display().to_string())
            })
            .unwrap_or_default();
        self.workspace_settings_input
            .update(cx, |input, cx| input.set_text(value, cx));
        self.selected_project = Some(project_id);
        self.project_settings = Some(project_id);
        self.workspace_settings_editing = false;
        self.adding_project = false;
        self.creating_harness = false;
        self.sidebar_menu = None;
        self.composer_dropdown = None;
        self.path_completion = None;
        self.enter_input_mode(false);
    }

    pub(crate) fn close_project_settings(&mut self) {
        self.project_settings = None;
        self.workspace_settings_editing = false;
        if let Some(project_id) = self.selected_harness.and_then(|harness_id| {
            self.harnesses
                .iter()
                .find(|harness| harness.id == harness_id)
                .map(|harness| harness.project_id)
        }) {
            self.selected_project = Some(project_id);
            self.show_cached_models(project_id);
        }
        self.enter_normal_mode();
    }

    pub(crate) fn default_workspace_path_for_project(&self, project_id: Id) -> Option<PathBuf> {
        let project = self
            .projects
            .iter()
            .find(|project| project.id == project_id)?;
        platform::workspace_root()
            .ok()
            .map(|root| root.join(vcs::project_slug(&project.name)))
    }

    pub(crate) fn begin_workspace_root_edit(&mut self, cx: &mut Context<Self>) {
        let Some(project_id) = self.project_settings else {
            return;
        };
        let value = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .and_then(|project| project.workspace_root.as_ref())
            .map(|path| path.display().to_string())
            .or_else(|| {
                self.default_workspace_path_for_project(project_id)
                    .map(|path| path.display().to_string())
            })
            .unwrap_or_default();
        self.workspace_settings_input.update(cx, |input, cx| {
            input.set_text(value, cx);
            input.select_all(cx);
        });
        self.workspace_settings_editing = true;
        self.enter_input_mode(true);
    }

    pub(crate) fn cancel_workspace_root_edit(&mut self, cx: &mut Context<Self>) {
        self.workspace_settings_editing = false;
        self.enter_input_mode(false);
        let Some(project_id) = self.project_settings else {
            return;
        };
        let value = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .and_then(|project| project.workspace_root.as_ref())
            .map(|path| path.display().to_string())
            .or_else(|| {
                self.default_workspace_path_for_project(project_id)
                    .map(|path| path.display().to_string())
            })
            .unwrap_or_default();
        self.workspace_settings_input
            .update(cx, |input, cx| input.set_text(value, cx));
    }

    pub(crate) fn use_default_workspace_root(&mut self, cx: &mut Context<Self>) {
        let Some(project_id) = self.project_settings else {
            return;
        };
        if let Some(project) = self
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        {
            project.workspace_root = None;
            self.workspace_settings_input
                .update(cx, |input, cx| input.clear(cx));
            self.workspace_settings_editing = false;
            self.enter_input_mode(false);
            self.persist();
        }
    }

    pub(crate) fn save_custom_workspace_root(&mut self, cx: &mut Context<Self>) {
        let Some(project_id) = self.project_settings else {
            return;
        };
        let raw = self
            .workspace_settings_input
            .read(cx)
            .text()
            .trim()
            .to_string();
        if raw.is_empty() {
            self.use_default_workspace_root(cx);
            cx.notify();
            return;
        }
        let path = platform::home_dir()
            .and_then(|home| resolve_tilde_path(&raw, &home))
            .unwrap_or_else(|| PathBuf::from(&raw));
        if !path.is_absolute() {
            self.banner = Some("Workspace paths must be absolute or start with ~.".into());
            cx.notify();
            return;
        }
        if let Err(error) = fs::create_dir_all(&path) {
            self.banner = Some(format!("Could not create {}: {error}", path.display()));
            cx.notify();
            return;
        }
        let path = match fs::canonicalize(&path) {
            Ok(path) => path,
            Err(error) => {
                self.banner = Some(format!("Could not open {}: {error}", path.display()));
                cx.notify();
                return;
            }
        };
        let repository_root = self
            .repository_snapshots
            .get(&project_id)
            .map(|snapshot| snapshot.repository_root.as_path())
            .or_else(|| {
                self.projects
                    .iter()
                    .find(|project| project.id == project_id)
                    .map(|project| project.path.as_path())
            });
        if repository_root.is_some_and(|root| path.starts_with(root)) {
            self.banner =
                Some("The workspace directory cannot be inside the project repository.".into());
            cx.notify();
            return;
        }
        if let Some(project) = self
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        {
            project.workspace_root = Some(path.clone());
            self.workspace_settings_input.update(cx, |input, cx| {
                input.set_text(path.display().to_string(), cx)
            });
            self.banner = None;
            self.workspace_settings_editing = false;
            self.enter_input_mode(false);
            self.persist();
        }
        cx.notify();
    }
}
