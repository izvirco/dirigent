//! Handles projects, threads, navigation, and sidebar-backed workspace state.

use super::*;

impl Dirigent {
    pub(crate) fn resize_sidebar(&mut self, width: f32, cx: &mut Context<Self>) {
        let width = width.clamp(200.0, 520.0);
        if self.sidebar_width == width {
            return;
        }
        self.sidebar_width = width;
        self.schedule_sidebar_layout_persist(cx);
    }
    pub(crate) fn toggle_project_collapsed(&mut self, project_id: Id) {
        if !self.collapsed_projects.remove(&project_id) {
            self.collapsed_projects.insert(project_id);
        }
        self.sidebar_menu = None;
        self.persist();
    }
    pub(crate) fn reorder_project(&mut self, source: Id, target: Id) {
        if source == target {
            return;
        }
        let Some(source_index) = self
            .projects
            .iter()
            .position(|project| project.id == source)
        else {
            return;
        };
        let Some(target_index) = self
            .projects
            .iter()
            .position(|project| project.id == target)
        else {
            return;
        };
        let project = self.projects.remove(source_index);
        self.projects
            .insert(target_index.min(self.projects.len()), project);
        self.persist();
    }
    pub(crate) fn toggle_thread_menu(&mut self, harness_id: Id) {
        let menu = SidebarMenu::Thread(harness_id);
        self.sidebar_menu = (self.sidebar_menu != Some(menu)).then_some(menu);
    }
    pub(crate) fn toggle_project_menu(&mut self, project_id: Id) {
        let menu = SidebarMenu::Project(project_id);
        self.sidebar_menu = (self.sidebar_menu != Some(menu)).then_some(menu);
    }
    pub(crate) fn toggle_bottom_sidebar_menu(&mut self) {
        self.sidebar_menu =
            (self.sidebar_menu != Some(SidebarMenu::Bottom)).then_some(SidebarMenu::Bottom);
    }
    pub(crate) fn begin_renaming_harness(&mut self, harness_id: Id, cx: &mut Context<Self>) {
        let Some(title) = self
            .harnesses
            .iter()
            .find(|harness| harness.id == harness_id)
            .map(|harness| {
                harness
                    .title
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        else {
            return;
        };
        self.thread_rename_input.update(cx, |input, cx| {
            input.set_text(title, cx);
            input.select_all(cx);
        });
        self.renaming_harness = Some(harness_id);
        self.sidebar_menu = None;
        self.set_sidebar_hovered(true, cx);
        self.enter_input_mode(true);
    }
    pub(super) fn finish_renaming_harness(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.renaming_harness else {
            return;
        };
        let title = self.thread_rename_input.read(cx).text().trim().to_string();
        if title.is_empty() {
            self.banner = Some("Thread names cannot be empty.".into());
            cx.notify();
            return;
        }
        if let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) {
            self.harnesses[index].set_manual_title(title);
            self.title_processes.remove(&id);
            self.synchronize_harness_session_name(index);
            self.persist();
        }
        self.renaming_harness = None;
        self.enter_normal_mode();
        cx.notify();
    }
    fn synchronize_harness_session_name(&self, index: usize) {
        let Some(process) = self.harnesses[index].process.as_ref() else {
            return;
        };
        let _ = process.send(json!({
            "type": "set_session_name",
            "name": self.harnesses[index].title,
        }));
    }

    fn start_title_generation(&mut self, harness_id: Id, prompt: &str) {
        let Some(project_path) = self
            .harnesses
            .iter()
            .find(|harness| harness.id == harness_id)
            .and_then(|harness| {
                self.projects
                    .iter()
                    .find(|project| project.id == harness.project_id)
            })
            .map(|project| project.path.clone())
        else {
            return;
        };
        match TitleProcess::spawn(harness_id, &project_path, prompt, self.title_events.clone()) {
            Ok(process) => {
                self.title_processes.insert(harness_id, process);
            }
            Err(error) => {
                tracing::error!(error = %error, harness_id, "could not start title generation");
            }
        }
    }

    pub(super) fn handle_title_generation_event(&mut self, event: TitleGenerationEvent) {
        self.title_processes.remove(&event.harness_id);
        let title = match event.result {
            Ok(title) => title,
            Err(error) => {
                tracing::error!(error = %error, harness_id = event.harness_id, "title generation failed");
                return;
            }
        };
        let Some(index) = self
            .harnesses
            .iter()
            .position(|harness| harness.id == event.harness_id)
        else {
            return;
        };
        if !self.harnesses[index].apply_generated_title(title) {
            return;
        }
        self.synchronize_harness_session_name(index);
        self.persist();
    }

    pub(super) fn select_after_harness_hidden(&mut self, project_id: Id) {
        let replacement = self.harness_navigation_ids().into_iter().next();
        if let Some(id) = replacement {
            self.select_harness(id);
        } else {
            self.selected_harness = None;
            self.last_used_harness = None;
            self.selected_project = self
                .projects
                .iter()
                .find(|project| project.id == project_id)
                .or_else(|| self.projects.first())
                .map(|project| project.id);
            self.creating_harness = false;
        }
    }
    pub(crate) fn can_archive_harness(&self, id: Id) -> bool {
        self.harnesses.iter().any(|harness| {
            harness.id == id
                && !matches!(
                    harness.status,
                    HarnessStatus::Starting | HarnessStatus::Working
                )
        })
    }
    pub(crate) fn set_harness_archived(&mut self, id: Id, archived: bool) {
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        if archived && !self.can_archive_harness(id) {
            return;
        }
        let project_id = self.harnesses[index].project_id;
        let sidebar_order = self.allocate_sidebar_order();
        self.harnesses[index].archived = archived;
        self.harnesses[index].sidebar_order = sidebar_order;
        self.harnesses[index].attention_required = false;
        self.harnesses[index].run_started_at = None;
        if archived {
            self.stop_delegation(id);
            self.harnesses[index].has_unread_completion = false;
            self.harnesses[index].process.take();
            self.harnesses[index].process_state = PiProcessState::Stopped;
            self.harnesses[index].status = HarnessStatus::Stopped;
        }
        self.sidebar_menu = None;
        if archived && self.selected_harness == Some(id) {
            self.select_after_harness_hidden(project_id);
        }
        self.persist();
    }
    pub(crate) fn delete_harness(&mut self, id: Id) {
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        let project_id = self.harnesses[index].project_id;
        // Include the entire delegation tree, not just direct children.
        let mut removed = HashSet::new();
        let mut pending = vec![id];
        while let Some(id) = pending.pop() {
            if !removed.insert(id) {
                continue;
            }
            pending.extend(
                self.harnesses
                    .iter()
                    .filter(|harness| harness.delegation.parent == Some(id))
                    .map(|harness| harness.id),
            );
            self.stop_delegation(id);
        }
        self.harnesses
            .retain(|harness| !removed.contains(&harness.id));
        self.composer_inputs.retain(|id, _| !removed.contains(id));
        self.title_processes.retain(|id, _| !removed.contains(id));
        self.pending_workspace_sources
            .retain(|id, _| !removed.contains(id));
        self.deleting_workspace_harnesses
            .retain(|id| !removed.contains(id));
        if self
            .pending_workspace_deletion
            .is_some_and(|id| removed.contains(&id))
        {
            self.pending_workspace_deletion = None;
        }
        self.sidebar_menu = None;
        if self
            .renaming_harness
            .is_some_and(|id| removed.contains(&id))
        {
            self.renaming_harness = None;
        }
        if self
            .pending_dialog
            .as_ref()
            .is_some_and(|dialog| removed.contains(&dialog.harness_id))
        {
            self.pending_dialog = None;
        }
        if self
            .last_used_harness
            .is_some_and(|id| removed.contains(&id))
        {
            self.last_used_harness = None;
        }
        if self
            .selected_harness
            .is_some_and(|id| removed.contains(&id))
        {
            self.select_after_harness_hidden(project_id);
        }
        self.persist();
    }
    pub(crate) fn delete_project(&mut self, id: Id) {
        let Some(index) = self.projects.iter().position(|project| project.id == id) else {
            return;
        };
        let removed_harnesses = self
            .harnesses
            .iter()
            .filter(|harness| harness.project_id == id)
            .map(|harness| harness.id)
            .collect::<HashSet<_>>();
        let deleting_selection = self.selected_project == Some(id);
        let was_adding_project = self.adding_project;

        self.projects.remove(index);
        self.harnesses.retain(|harness| harness.project_id != id);
        self.composer_inputs
            .retain(|harness_id, _| !removed_harnesses.contains(harness_id));
        self.title_processes
            .retain(|harness_id, _| !removed_harnesses.contains(harness_id));
        self.collapsed_projects.remove(&id);
        self.expanded_archived_projects.remove(&id);
        self.project_file_pickers.remove(&id);
        self.repository_snapshots.remove(&id);
        self.available_models_by_project.remove(&id);
        self.available_thinking_levels
            .retain(|(project_id, _), _| *project_id != id);
        if self
            .project_probe
            .as_ref()
            .is_some_and(|(project_id, _)| *project_id == id)
        {
            self.project_probe.take();
        }
        if self
            .renaming_harness
            .is_some_and(|harness_id| removed_harnesses.contains(&harness_id))
        {
            self.renaming_harness = None;
        }
        if self
            .pending_dialog
            .as_ref()
            .is_some_and(|dialog| removed_harnesses.contains(&dialog.harness_id))
        {
            self.pending_dialog = None;
        }
        if self
            .last_used_harness
            .is_some_and(|harness_id| removed_harnesses.contains(&harness_id))
        {
            self.last_used_harness = None;
        }
        self.sidebar_menu = None;
        if self.project_settings == Some(id) {
            self.project_settings = None;
        }

        if deleting_selection {
            self.selected_harness = None;
            self.thread_text_selection = None;
            self.creating_harness = false;
            self.composer_dropdown = None;
            self.path_completion = None;
            self.draft_model = None;
            self.draft_thinking_level = None;
            self.draft_workspace_source = None;

            if !was_adding_project
                && let Some(harness_id) = self.harness_navigation_ids().into_iter().next()
            {
                self.select_harness(harness_id);
            } else {
                self.selected_project = self.projects.first().map(|project| project.id);
                self.adding_project = was_adding_project || self.projects.is_empty();
                if let Some(project_id) = self.selected_project {
                    self.show_cached_models(project_id);
                } else {
                    self.available_models.clear();
                }
            }
        }
        self.persist();
    }
    pub(crate) fn begin_adding_project(&mut self) {
        self.settings = None;
        self.about_open = false;
        self.project_probe.take();
        self.path_completion = None;
        self.adding_project = true;
        self.creating_harness = false;
        self.project_settings = None;
        self.sidebar_menu = None;
        self.banner = None;
    }
    pub(super) fn start_new_selected_project(&mut self) {
        if let Some(project_id) = self
            .selected_project
            .or_else(|| self.projects.first().map(|p| p.id))
        {
            self.start_new_harness(project_id);
        } else {
            self.begin_adding_project();
        }
        self.enter_input_mode(true);
    }
    /// Returns visible threads in keyboard-navigation order: inbox first, then active work.
    pub(super) fn harness_navigation_ids(&self) -> Vec<Id> {
        let mut inbox = self
            .harnesses
            .iter()
            .filter(|harness| harness.is_in_inbox())
            .collect::<Vec<_>>();
        let mut workpool = self
            .harnesses
            .iter()
            .filter(|harness| harness.is_in_workpool())
            .collect::<Vec<_>>();
        inbox.sort_by_key(|harness| std::cmp::Reverse(harness.sidebar_order));
        workpool.sort_by_key(|harness| std::cmp::Reverse(harness.sidebar_order));
        inbox
            .into_iter()
            .chain(workpool)
            .map(|harness| harness.id)
            .collect()
    }
    pub(super) fn select_relative_harness(&mut self, delta: isize) {
        let ids = self.harness_navigation_ids();
        if ids.is_empty() {
            return;
        }
        let current = self
            .selected_harness
            .and_then(|id| ids.iter().position(|candidate| *candidate == id))
            .unwrap_or(0) as isize;
        let target = (current + delta).rem_euclid(ids.len() as isize) as usize;
        self.select_harness(ids[target]);
    }
    pub(super) fn select_edge_harness(&mut self, newest: bool) {
        let ids = self.harness_navigation_ids();
        let target = if newest { ids.first() } else { ids.last() };
        if let Some(id) = target {
            self.select_harness(*id);
        }
    }
    pub(super) fn select_matching_harness(&mut self, matches: impl Fn(&Harness) -> bool) {
        let ids = self
            .harnesses
            .iter()
            .rev()
            .filter(|harness| !harness.archived && matches(harness))
            .map(|harness| harness.id)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return;
        }
        let target = self
            .selected_harness
            .and_then(|id| ids.iter().position(|candidate| *candidate == id))
            .map_or(0, |index| (index + 1) % ids.len());
        self.select_harness(ids[target]);
    }
    pub(super) fn open_project(&mut self, project_id: Id) {
        if let Some(id) = self
            .harnesses
            .iter()
            .filter(|harness| harness.project_id == project_id && harness.archived)
            .max_by_key(|harness| harness.sidebar_order)
            .map(|harness| harness.id)
        {
            self.select_harness(id);
        } else {
            self.start_new_harness(project_id);
        }
    }
    pub(super) fn select_relative_project(&mut self, delta: isize) {
        if self.projects.is_empty() {
            return;
        }
        let current = self
            .selected_project
            .and_then(|id| self.projects.iter().position(|project| project.id == id))
            .unwrap_or(0) as isize;
        let target = (current + delta).rem_euclid(self.projects.len() as isize) as usize;
        self.open_project(self.projects[target].id);
    }
    pub(super) fn select_edge_project(&mut self, first: bool) {
        let project_id = if first {
            self.projects.first().map(|project| project.id)
        } else {
            self.projects.last().map(|project| project.id)
        };
        if let Some(project_id) = project_id {
            self.open_project(project_id);
        }
    }
    pub(crate) fn add_project(&mut self, cx: &mut Context<Self>) {
        let raw_path = self.project_input.read(cx).text().trim().to_string();
        if raw_path.is_empty() {
            self.banner = Some("Enter the full path to a project.".into());
            cx.notify();
            return;
        }
        let path = platform::home_dir()
            .and_then(|home| resolve_tilde_path(&raw_path, &home))
            .unwrap_or_else(|| PathBuf::from(&raw_path));
        if !path.is_absolute() {
            self.banner = Some("Project paths must be absolute or start with ~.".into());
            cx.notify();
            return;
        }
        if let Err(error) = self.open_project_directory(path, cx) {
            self.banner = Some(error);
        }
        cx.notify();
    }

    /// A directory launch selects its composer; a plain relaunch only activates the window.
    /// Returns whether startup should skip restoring the previously selected thread.
    pub(crate) fn handle_launch(
        &mut self,
        project_directory: Result<Option<PathBuf>, String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let result = match project_directory {
            Ok(Some(path)) => self.open_project_directory(path, cx),
            Ok(None) => return false,
            Err(error) => Err(error),
        };
        let opened = match result {
            Ok(()) => {
                self.enter_input_mode(true);
                true
            }
            Err(error) => {
                self.banner = Some(error);
                false
            }
        };
        cx.notify();
        opened
    }

    /// Shared by the project input and Explorer launches; canonical paths prevent duplicates.
    pub(super) fn open_project_directory(
        &mut self,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let path = fs::canonicalize(&path)
            .map_err(|error| format!("Cannot open {}: {error}", path.display()))?;
        if !path.is_dir() {
            return Err(format!("{} is not a directory.", path.display()));
        }
        if let Some(project_id) = self
            .projects
            .iter()
            .find(|project| project.path == path)
            .map(|project| project.id)
        {
            self.start_new_harness(project_id);
            return Ok(());
        }

        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("project")
            .to_string();
        let id = self.allocate_id();
        self.projects.push(Project {
            id,
            name,
            path: path.clone(),
            workspace_root: None,
            keep_active_threads_in_project: false,
        });
        match start_fuzzy_index(
            &path,
            true,
            FuzzyIndexReady::Project(id),
            self.fuzzy_index_events.clone(),
        ) {
            Ok(picker) => {
                self.project_file_pickers.insert(id, picker);
            }
            Err(error) => {
                tracing::error!(error = %error, project_id = id, "could not start project file index");
                self.banner = Some(error);
            }
        }
        self.collapsed_projects.insert(id);
        self.project_input.update(cx, |input, cx| input.clear(cx));
        self.persist();
        self.start_new_harness(id);
        Ok(())
    }
    /// Creates the local thread immediately, then starts Pi or provisions its chosen workspace.
    pub(crate) fn create_harness(&mut self, cx: &mut Context<Self>) {
        let prompt = self.harness_input.read(cx).text().trim().to_string();
        let images = self.harness_input.read(cx).images();
        let Some(project_id) = self.selected_project else {
            self.banner = Some("Connect a project before starting a harness.".into());
            cx.notify();
            return;
        };
        if prompt.is_empty() {
            self.banner = Some("Describe a task before starting pi.".into());
            cx.notify();
            return;
        }
        let title = prompt.chars().take(54).collect::<String>();
        self.project_probe.take();
        let workspace_source = self.draft_workspace_source.take();
        let id = self.allocate_id();
        let sidebar_order = self.allocate_sidebar_order();
        let mut harness = Harness::new(id, project_id, title, sidebar_order);
        harness.nix_enabled = self.draft_nix_enabled && self.project_has_devshell(project_id);
        harness.model = self.draft_model.take();
        harness.thinking_level = self.draft_thinking_level.take();
        if workspace_source.is_some() {
            harness.status = HarnessStatus::Starting;
            harness.pending_initial_prompt = Some((prompt.clone(), images.clone()));
        } else {
            harness.status = HarnessStatus::Working;
            harness.run_started_at = Some(Instant::now());
        }
        harness.messages.push(Message::user_with_images(
            prompt.clone(),
            images.iter().map(|image| image.image.clone()).collect(),
        ));
        self.harnesses.push(harness);
        self.refresh_harness_vcs_label(self.harnesses.len() - 1);
        self.selected_harness = Some(id);
        self.last_used_harness = Some(id);
        self.add_composer_input(id, cx);
        self.reset_conversation_list(self.harnesses.len() - 1);
        self.composer_dropdown = None;
        self.path_completion = None;
        self.keyboard_mode = KeyboardMode::Input;
        self.focus_input = true;
        self.creating_harness = false;
        self.project_settings = None;
        self.banner = None;
        self.harness_input.update(cx, |input, cx| input.clear(cx));
        self.persist();
        self.start_title_generation(id, &prompt);
        if let Some(source) = workspace_source {
            self.provision_harness_workspace(id, source);
        } else {
            self.start_harness(id, Some((prompt, images)));
        }
        cx.notify();
    }
    /// Switches the complete conversation context and lazily starts the selected Pi process.
    pub(crate) fn select_harness(&mut self, id: Id) {
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        self.settings = None;
        self.about_open = false;
        self.project_probe.take();
        self.harnesses[index].has_unread_completion = false;
        self.selected_project = Some(self.harnesses[index].project_id);
        self.show_cached_models(self.harnesses[index].project_id);
        self.selected_harness = Some(id);
        self.last_used_harness = Some(id);
        self.thread_text_selection = None;
        self.selected_diff_turn = None;
        self.hovered_copy_message = None;
        self.hovered_action_message = None;
        self.hovered_tool_detail_message = None;
        self.editing_message = None;
        self.pending_edit_submit = None;
        self.adding_project = false;
        self.creating_harness = false;
        self.project_settings = None;
        self.composer_dropdown = None;
        self.path_completion = None;
        self.draft_model = None;
        self.draft_thinking_level = None;
        self.request_cached_session_rebuild(index);
        self.request_delegated_session_rebuilds(id);
        self.reset_conversation_list(index);
        self.persist_last_used_harness();
        self.refresh_repository(self.harnesses[index].project_id);
        self.start_harness(id, None);
    }
    pub(crate) fn start_new_harness(&mut self, project_id: Id) {
        self.settings = None;
        self.about_open = false;
        // New threads inherit model settings from a visible sibling, avoiding an ephemeral probe
        // when a project Pi process has already resolved its local configuration.
        let source_id = self
            .selected_harness
            .filter(|id| {
                self.harnesses.iter().any(|harness| {
                    harness.id == *id && harness.project_id == project_id && !harness.archived
                })
            })
            .or_else(|| {
                self.harnesses
                    .iter()
                    .find(|harness| harness.project_id == project_id && !harness.archived)
                    .map(|harness| harness.id)
            });
        let (model, thinking_level) = source_id
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
            .map(|harness| (harness.model.clone(), harness.thinking_level.clone()))
            .unwrap_or_default();

        self.selected_project = Some(project_id);
        self.show_cached_models(project_id);
        self.selected_harness = None;
        self.selected_diff_turn = None;
        self.adding_project = false;
        self.creating_harness = true;
        self.project_settings = None;
        self.editing_message = None;
        self.pending_edit_submit = None;
        self.composer_dropdown = None;
        self.path_completion = None;
        self.draft_model = model;
        self.draft_thinking_level = thinking_level;
        self.draft_nix_enabled = true;
        self.draft_workspace_source = None;
        self.banner = None;
        self.project_probe.take();
        self.refresh_repository(project_id);

        if let Some(id) = source_id {
            if self.draft_model.is_none()
                || self.draft_thinking_level.is_none()
                || self.available_models.is_empty()
            {
                self.start_harness(id, None);
                if let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) {
                    if self.draft_model.is_none() || self.draft_thinking_level.is_none() {
                        self.send_value(index, json!({"id":"dirigent-state","type":"get_state"}));
                    }
                    if self.available_models.is_empty() {
                        self.send_value(index, json!({"type":"get_available_models"}));
                    }
                }
            }
        } else {
            self.start_project_probe(project_id);
        }
    }
}
