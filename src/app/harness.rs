use super::*;

pub(super) fn conversation_list_splice(
    old_message_count: usize,
    old_working: bool,
    new_message_count: usize,
    new_working: bool,
) -> (Range<usize>, usize) {
    let retained_message_count = old_message_count.min(new_message_count);
    let old_item_count = old_message_count + usize::from(old_working);
    let new_item_count = new_message_count + usize::from(new_working);
    (
        retained_message_count..old_item_count,
        new_item_count - retained_message_count,
    )
}

pub(super) fn reasoning_options_for_model(
    project_id: Option<Id>,
    current_model: &str,
    available_models: &[AvailableModel],
    available_thinking_levels: &HashMap<(Id, String), Vec<String>>,
) -> Vec<String> {
    if let Some(project_id) = project_id
        && let Some(levels) =
            available_thinking_levels.get(&(project_id, current_model.to_string()))
    {
        return levels.clone();
    }
    let model = available_models
        .iter()
        .find(|model| current_model == format!("{}/{}", model.provider, model.id));
    if model.is_some_and(|model| !model.reasoning) {
        return vec!["off".into()];
    }
    let mut levels = vec![
        "off".into(),
        "minimal".into(),
        "low".into(),
        "medium".into(),
        "high".into(),
    ];
    if model.is_some_and(|model| model.supports_xhigh) {
        levels.push("xhigh".into());
    }
    if model.is_some_and(|model| model.supports_max) {
        levels.push("max".into());
    }
    levels
}

impl Dirigent {
    pub(super) fn add_composer_input(&mut self, harness_id: Id, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            TextInput::new("Make no mistakes, or else...", cx)
                .borderless()
                .multiline()
        });
        cx.subscribe(&input, move |this, _, event, cx| match event {
            InputEvent::Submit if this.selected_harness == Some(harness_id) => {
                this.send_composer(cx)
            }
            InputEvent::Focused if this.selected_harness == Some(harness_id) => {
                this.enter_input_mode(false);
                this.refresh_composer_path_completion(
                    PathCompletionTarget::Harness(harness_id),
                    cx,
                );
                cx.notify();
            }
            InputEvent::Escape if this.selected_harness == Some(harness_id) => {
                if this.path_completion.is_some() {
                    this.path_completion = None;
                } else {
                    this.enter_normal_mode();
                }
                cx.notify();
            }
            InputEvent::Changed => {
                this.persist_composer_draft(harness_id, cx);
                this.refresh_composer_path_completion(
                    PathCompletionTarget::Harness(harness_id),
                    cx,
                );
            }
            InputEvent::CompletionPrevious => this.move_path_completion(-1, cx),
            InputEvent::CompletionNext => this.move_path_completion(1, cx),
            InputEvent::CompletionAccepted => this.accept_path_completion(cx),
            _ => {}
        })
        .detach();
        self.composer_inputs.insert(harness_id, input);
    }
    pub(super) fn persist_composer_draft(&mut self, harness_id: Id, cx: &mut Context<Self>) {
        let Some(input) = self.composer_inputs.get(&harness_id) else {
            return;
        };
        let draft = input.read(cx).text().to_string();
        let images = input.read(cx).images();
        let Some(index) = self
            .harnesses
            .iter()
            .position(|harness| harness.id == harness_id)
        else {
            return;
        };
        let text_changed = self.harnesses[index].composer_draft != draft;
        let images_changed = self.harnesses[index].composer_draft_images.len() != images.len()
            || self.harnesses[index]
                .composer_draft_images
                .iter()
                .zip(&images)
                .any(|(cached, current)| {
                    cached.label != current.label || !Arc::ptr_eq(&cached.image, &current.image)
                });
        if !text_changed && !images_changed {
            return;
        }
        self.harnesses[index].composer_draft = draft;
        self.harnesses[index].composer_draft_images = images;
        self.cache_harness_draft(index, images_changed);
    }
    pub(super) fn show_cached_models(&mut self, project_id: Id) {
        self.available_models = self
            .available_models_by_project
            .get(&project_id)
            .cloned()
            .unwrap_or_default();
    }
    pub(super) fn cache_harness_entries(&mut self, index: usize) {
        let Some(cache) = self.session_cache.as_ref() else {
            return;
        };
        let harness = &self.harnesses[index];
        let (Some(session_file), Some(entries)) = (
            harness.session_file.as_deref(),
            harness.cached_entries.as_deref(),
        ) else {
            return;
        };
        if let Err(error) =
            cache.save_entries(session_file, entries, harness.cached_leaf_id.as_deref())
        {
            self.report_cache_error(error);
        }
    }
    pub(super) fn cache_harness_state(&mut self, index: usize) {
        let Some(cache) = self.session_cache.as_ref() else {
            return;
        };
        let harness = &self.harnesses[index];
        let Some(session_file) = harness.session_file.as_deref() else {
            return;
        };
        if let Err(error) = cache.save_session_state(
            session_file,
            harness.model.as_deref(),
            harness.thinking_level.as_deref(),
        ) {
            self.report_cache_error(error);
        }
    }
    pub(super) fn cache_harness_draft(&mut self, index: usize, include_images: bool) {
        let Some(cache) = self.session_cache.as_ref() else {
            return;
        };
        let harness = &self.harnesses[index];
        let Some(session_file) = harness.session_file.as_deref() else {
            return;
        };
        let result = if include_images {
            let images = harness
                .composer_draft_images
                .iter()
                .map(|attachment| CachedDraftImage {
                    label: attachment.label.clone(),
                    mime_type: attachment.image.format.mime_type().to_string(),
                    data: BASE64.encode(&attachment.image.bytes),
                })
                .collect::<Vec<_>>();
            serde_json::to_vec(&images)
                .map_err(|error| format!("could not encode composer images for caching: {error}"))
                .and_then(|images_json| {
                    cache.save_composer_draft(session_file, &harness.composer_draft, &images_json)
                })
        } else {
            cache.save_composer_text(session_file, &harness.composer_draft)
        };
        if let Err(error) = result {
            self.report_cache_error(error);
        }
    }
    pub(super) fn cache_project_models(&mut self, project_id: Id) {
        let Some(cache) = self.session_cache.as_ref() else {
            return;
        };
        let Some(project_path) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.path.clone())
        else {
            return;
        };
        let Some(models) = self.available_models_by_project.get(&project_id) else {
            return;
        };
        let result = serde_json::to_vec(models)
            .map_err(|error| format!("could not encode models for caching: {error}"))
            .and_then(|bytes| cache.save_models(&project_path, &bytes));
        if let Err(error) = result {
            self.report_cache_error(error);
        }
    }
    pub(super) fn cache_model_thinking_levels(&mut self, project_id: Id, model: &str) {
        let Some(cache) = self.session_cache.as_ref() else {
            return;
        };
        let Some(project_path) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.path.clone())
        else {
            return;
        };
        let Some(levels) = self
            .available_thinking_levels
            .get(&(project_id, model.to_string()))
        else {
            return;
        };
        if let Err(error) = cache.save_thinking_levels(&project_path, model, levels) {
            self.report_cache_error(error);
        }
    }
    pub(super) fn reset_conversation_list(&mut self, harness_index: usize) {
        let harness = &self.harnesses[harness_index];
        self.conversation_list_message_count = harness.messages.len();
        self.conversation_list_queued_count = harness.queued_messages.len();
        self.conversation_list_working = harness.status == HarnessStatus::Working;
        let item_count = self.conversation_list_message_count
            + self.conversation_list_queued_count
            + usize::from(self.conversation_list_working);
        self.conversation_list
            .reset_with_uniform_height(item_count, px(48.0));
        self.conversation_list.set_follow_mode(FollowMode::Tail);
    }
    pub(super) fn sync_conversation_list(
        &mut self,
        harness_index: usize,
        changed_message: Option<usize>,
    ) {
        if self.selected_harness != Some(self.harnesses[harness_index].id) {
            return;
        }
        let harness = &self.harnesses[harness_index];
        let new_message_count = harness.messages.len();
        let new_queued_count = harness.queued_messages.len();
        let new_working = harness.status == HarnessStatus::Working;
        if (self.conversation_list_queued_count > 0 || new_queued_count > 0)
            && (self.conversation_list_message_count != new_message_count
                || self.conversation_list_queued_count != new_queued_count
                || self.conversation_list_working != new_working)
        {
            let old_item_count = self.conversation_list_message_count
                + self.conversation_list_queued_count
                + usize::from(self.conversation_list_working);
            let new_item_count = new_message_count + new_queued_count + usize::from(new_working);
            self.conversation_list
                .splice(0..old_item_count, new_item_count);
            self.conversation_list_message_count = new_message_count;
            self.conversation_list_queued_count = new_queued_count;
            self.conversation_list_working = new_working;
        } else if self.conversation_list_message_count != new_message_count
            || self.conversation_list_working != new_working
        {
            let (old_range, new_item_count) = conversation_list_splice(
                self.conversation_list_message_count,
                self.conversation_list_working,
                new_message_count,
                new_working,
            );
            self.conversation_list.splice(old_range, new_item_count);
            if new_message_count < self.conversation_list_message_count {
                // A completed run is replaced with canonical session entries, which can
                // contain fewer, differently grouped messages than the streamed view.
                self.conversation_list.remeasure_items(0..new_message_count);
            }
            self.conversation_list_message_count = new_message_count;
            self.conversation_list_queued_count = new_queued_count;
            self.conversation_list_working = new_working;
        }
        if let Some(index) = changed_message.filter(|index| *index < new_message_count) {
            self.conversation_list.remeasure_items(index..index + 1);
        }
    }
    pub(crate) fn scroll_conversation_to_fraction(&mut self, fraction: f32) {
        let max_offset = self.conversation_list.max_offset_for_scrollbar().y;
        self.conversation_list
            .set_offset_from_scrollbar(point(px(0.0), -max_offset * fraction.clamp(0.0, 1.0)));
    }
    pub(super) fn scroll_conversation_by_fraction(&mut self, delta: f32) {
        let max_offset = self.conversation_list.max_offset_for_scrollbar().y.as_f32();
        let current = if max_offset > 0.0 {
            (-self
                .conversation_list
                .scroll_px_offset_for_scrollbar()
                .y
                .as_f32()
                / max_offset)
                .clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.scroll_conversation_to_fraction(current + delta);
    }
    pub(super) fn start_project_probe(&mut self, project_id: Id) {
        if self
            .project_probe
            .as_ref()
            .is_some_and(|(active_project_id, _)| *active_project_id == project_id)
        {
            return;
        }
        self.project_probe.take();
        let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
        else {
            return;
        };
        let project_path = project.path.clone();
        let session_name = format!("{} setup", project.name);
        let nix_enabled = self.draft_nix_enabled && project_has_devshell(&project_path);
        match PiProcess::spawn(
            RuntimeTarget::Project(project_id),
            &project_path,
            None,
            &session_name,
            self.pi_bridge_extension.as_deref(),
            nix_enabled,
            true,
            self.runtime_events.clone(),
        ) {
            Ok(process) => {
                self.project_probe = Some((project_id, process));
                self.send_project_value(
                    project_id,
                    json!({"id":"dirigent-project-state","type":"get_state"}),
                );
                self.send_project_value(project_id, json!({"type":"get_available_models"}));
            }
            Err(error) => {
                tracing::error!(error = %error, project_id, "could not start Pi project probe");
                self.banner = Some(error);
            }
        }
    }
    pub(super) fn send_project_value(&mut self, project_id: Id, value: Value) -> bool {
        let result = self
            .project_probe
            .as_ref()
            .filter(|(active_project_id, _)| *active_project_id == project_id)
            .ok_or_else(|| "pi project setup process is not running".to_string())
            .and_then(|(_, process)| process.send(value));
        if let Err(error) = result {
            tracing::error!(error = %error, project_id, "could not send Pi project command");
            self.banner = Some(error);
            false
        } else {
            true
        }
    }
    pub(super) fn mark_harness_working(&mut self, index: usize) {
        let started_run = self.harnesses[index].run_started_at.is_none();
        self.harnesses[index].status = HarnessStatus::Working;
        self.harnesses[index].attention_required = false;
        if started_run {
            self.harnesses[index].run_started_at = Some(Instant::now());
            self.harnesses[index].last_run_duration = None;
            self.refresh_harness_order(index);
            self.persist();
        }
        self.sync_conversation_list(index, None);
    }
    pub(super) fn start_harness(
        &mut self,
        id: Id,
        initial_prompt: Option<(String, Vec<AttachedImage>)>,
    ) {
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        if let Some(initial_prompt) = initial_prompt {
            self.harnesses[index].pending_initial_prompt = Some(initial_prompt);
        }
        if self.harnesses[index].pending_initial_prompt.is_some() {
            self.mark_harness_working(index);
        }
        if self.harnesses[index].process.is_some() {
            if !self.harnesses[index].startup_settings_pending
                && let Some((prompt, images)) = self.harnesses[index].pending_initial_prompt.take()
            {
                self.send_prompt_command(index, prompt, images, false);
            }
            return;
        }
        let project_path = match self.working_directory_for_harness(id) {
            Ok(path) => path,
            Err(error) => {
                self.fail_harness(index, error);
                return;
            }
        };
        let session_file = self.harnesses[index].session_file.clone();
        let title = self.harnesses[index].title.clone();
        self.harnesses[index].error = None;
        self.harnesses[index].process_generation += 1;
        let process_generation = self.harnesses[index].process_generation;
        self.sync_conversation_list(index, None);
        match PiProcess::spawn(
            RuntimeTarget::Harness(id, process_generation),
            &project_path,
            session_file.as_deref(),
            &title,
            self.pi_bridge_extension.as_deref(),
            self.harnesses[index].nix_enabled && project_has_devshell(&project_path),
            false,
            self.runtime_events.clone(),
        ) {
            Ok(process) => {
                self.harnesses[index].process = Some(process);
                if self.harnesses[index].startup_settings_pending {
                    self.send_startup_model(index);
                } else {
                    self.finish_harness_startup(index);
                }
            }
            Err(error) => self.fail_harness(index, error),
        }
    }

    // Pi dispatches RPC input lines concurrently, so startup settings must be
    // chained from their responses before the initial prompt is sent.
    pub(super) fn send_startup_model(&mut self, index: usize) {
        let model = self.harnesses[index].model.clone().and_then(|model| {
            let (provider, model_id) = model.split_once('/')?;
            Some((provider.to_string(), model_id.to_string()))
        });
        if let Some((provider, model_id)) = model {
            self.send_value(
                index,
                json!({
                    "id": STARTUP_MODEL_REQUEST_ID,
                    "type":"set_model",
                    "provider":provider,
                    "modelId":model_id
                }),
            );
        } else {
            self.send_startup_thinking_level(index);
        }
    }
    pub(super) fn send_startup_thinking_level(&mut self, index: usize) {
        if let Some(level) = self.harnesses[index].thinking_level.clone() {
            self.send_value(
                index,
                json!({
                    "id": STARTUP_THINKING_REQUEST_ID,
                    "type":"set_thinking_level",
                    "level":level
                }),
            );
        } else {
            self.finish_harness_startup(index);
        }
    }
    pub(super) fn finish_harness_startup(&mut self, index: usize) {
        self.harnesses[index].startup_settings_pending = false;
        self.send_value(index, json!({"id":"dirigent-state","type":"get_state"}));
        self.request_context_usage(index);
        if !self.harnesses[index].loaded_messages {
            self.request_entries(index);
        }
        if let Some((prompt, images)) = self.harnesses[index].pending_initial_prompt.take() {
            self.send_prompt_command(index, prompt, images, false);
        }
    }
    pub(super) fn send_value(&mut self, index: usize, value: Value) -> bool {
        let result = self.harnesses[index]
            .process
            .as_ref()
            .ok_or_else(|| "pi is not running".to_string())
            .and_then(|process| process.send(value));
        if let Err(error) = result {
            self.fail_harness(index, error);
            false
        } else {
            true
        }
    }
    pub(super) fn request_context_usage(&mut self, index: usize) {
        self.send_value(index, json!({"type":"get_session_stats"}));
    }
    pub(super) fn request_entries(&mut self, index: usize) {
        let cursor = self.harnesses[index]
            .cached_entries
            .as_ref()
            .and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find_map(|entry| entry.get("id")?.as_str())
            })
            .map(str::to_string);
        if let Some(cursor) = cursor {
            self.send_value(
                index,
                json!({
                    "id":"dirigent-entries-incremental",
                    "type":"get_entries",
                    "since":cursor,
                }),
            );
        } else {
            self.send_value(
                index,
                json!({"id":"dirigent-entries-full","type":"get_entries"}),
            );
        }
    }
    pub(super) fn request_thinking_levels(&mut self, index: usize) {
        if self.harnesses[index].model.is_some() {
            self.send_value(index, json!({"type":"get_available_thinking_levels"}));
        }
    }
    pub(super) fn send_prompt_command(
        &mut self,
        index: usize,
        prompt: String,
        images: Vec<AttachedImage>,
        steer_if_working: bool,
    ) {
        self.begin_turn_diff(index, &prompt);
        let images = images
            .into_iter()
            .map(|attachment| {
                json!({
                    "type":"image",
                    "data":BASE64.encode(&attachment.image.bytes),
                    "mimeType":attachment.image.format.mime_type(),
                })
            })
            .collect::<Vec<_>>();
        let mut command = json!({"type":"prompt","message":prompt});
        if !images.is_empty() {
            command["images"] = Value::Array(images);
        }
        if steer_if_working && self.harnesses[index].status == HarnessStatus::Working {
            command["streamingBehavior"] = Value::String("steer".into());
        }
        if self.send_value(index, command) {
            self.mark_harness_working(index);
        }
    }
    pub(crate) fn send_composer(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_harness else {
            return;
        };
        let Some(input) = self.composer_inputs.get(&id).cloned() else {
            return;
        };
        if let Some(workspace) = self.selected_managed_workspace()
            && workspace.state != WorkspaceState::Ready
        {
            self.banner = Some(match &workspace.state {
                WorkspaceState::Provisioning => "The workspace is still being created.".into(),
                WorkspaceState::Failed(error) => error.clone(),
                WorkspaceState::Ready => unreachable!(),
            });
            cx.notify();
            return;
        }
        if self.pending_forks.contains_key(&id) {
            self.banner = Some("The fork is still being created.".into());
            cx.notify();
            return;
        }
        let message = input.read(cx).text().trim().to_string();
        if message.is_empty() {
            return;
        }
        if self
            .harnesses
            .iter()
            .find(|harness| harness.id == id)
            .is_some_and(|harness| harness.archived)
        {
            self.set_harness_archived(id, false);
        }
        if let Some(source) = self.pending_workspace_sources.remove(&id) {
            let images = input.read(cx).images();
            let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
                return;
            };
            self.harnesses[index].pending_initial_prompt = Some((message.clone(), images.clone()));
            self.harnesses[index]
                .messages
                .push(Message::user_with_images(
                    message,
                    images.iter().map(|image| image.image.clone()).collect(),
                ));
            self.sync_conversation_list(index, None);
            input.update(cx, |input, cx| input.clear(cx));
            self.harnesses[index].composer_draft.clear();
            self.harnesses[index].composer_draft_images.clear();
            self.cache_harness_draft(index, true);
            self.composer_dropdown = None;
            self.provision_harness_workspace(id, source);
            cx.notify();
            return;
        }
        self.start_harness(id, None);
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        if self.harnesses[index].process.is_none() {
            return;
        }
        let images = input.read(cx).images();
        if self.harnesses[index].startup_settings_pending {
            self.harnesses[index].pending_initial_prompt = Some((message.clone(), images.clone()));
            self.harnesses[index]
                .messages
                .push(Message::user_with_images(
                    message,
                    images.iter().map(|image| image.image.clone()).collect(),
                ));
            self.mark_harness_working(index);
            input.update(cx, |input, cx| input.clear(cx));
            self.harnesses[index].composer_draft.clear();
            self.harnesses[index].composer_draft_images.clear();
            self.cache_harness_draft(index, true);
            cx.notify();
            return;
        }
        let steering = self.harnesses[index].status == HarnessStatus::Working;
        let mut user_message = Message::user_with_images(
            message.clone(),
            images.iter().map(|image| image.image.clone()).collect(),
        );
        user_message.queued = steering;
        if steering {
            self.harnesses[index].queued_messages.push(user_message);
        } else {
            self.harnesses[index].messages.push(user_message);
        }
        self.sync_conversation_list(index, None);
        self.conversation_list.scroll_to_end();
        self.composer_dropdown = None;
        self.send_prompt_command(index, message, images, true);
        input.update(cx, |input, cx| input.clear(cx));
        self.harnesses[index].composer_draft.clear();
        self.harnesses[index].composer_draft_images.clear();
        self.cache_harness_draft(index, true);
        cx.notify();
    }
    pub(crate) fn abort_selected(&mut self) {
        if let Some(index) = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().position(|harness| harness.id == id))
            && self.send_value(index, json!({"type":"abort"}))
        {
            self.mark_turn_diff_status(index, TurnDiffStatus::Aborted);
            self.harnesses[index].startup_settings_pending = false;
            self.harnesses[index].pending_initial_prompt = None;
            self.harnesses[index].status = HarnessStatus::Idle;
            self.harnesses[index].run_started_at = None;
            self.harnesses[index].attention_required = false;
            self.refresh_harness_order(index);
            let mut changed_message = None;
            for (message_index, message) in self.harnesses[index].messages.iter_mut().enumerate() {
                if message.running {
                    if message.role == MessageRole::Assistant {
                        changed_message = Some(message_index);
                    }
                    message.set_running(false);
                }
            }
            self.persist();
            self.sync_conversation_list(index, changed_message);
        }
    }
    pub(super) fn restart_selected(&mut self) {
        if let Some(id) = self.selected_harness {
            self.restart_harness(id);
        }
    }
    pub(super) fn composer_harness_id(&self) -> Option<Id> {
        if let Some(id) = self.selected_harness {
            return Some(id);
        }
        if !self.creating_harness {
            return None;
        }
        let project_id = self.selected_project?;
        self.harnesses
            .iter()
            .find(|harness| harness.project_id == project_id)
            .map(|harness| harness.id)
    }
    pub(crate) fn toggle_composer_dropdown(&mut self, dropdown: ComposerDropdown) {
        if self.composer_dropdown == Some(dropdown) {
            self.composer_dropdown = None;
            return;
        }
        self.composer_dropdown = Some(dropdown);
        if matches!(
            dropdown,
            ComposerDropdown::Project
                | ComposerDropdown::EditModel
                | ComposerDropdown::EditReasoning
                | ComposerDropdown::Workspace
        ) {
            return;
        }
        if self.creating_harness
            && let Some(project_id) = self.selected_project
            && self
                .project_probe
                .as_ref()
                .is_some_and(|(active_project_id, _)| *active_project_id == project_id)
        {
            match dropdown {
                ComposerDropdown::Model => {
                    self.send_project_value(project_id, json!({"type":"get_available_models"}));
                }
                ComposerDropdown::Reasoning => {
                    self.send_project_value(
                        project_id,
                        json!({"type":"get_available_thinking_levels"}),
                    );
                }
                ComposerDropdown::Project
                | ComposerDropdown::EditModel
                | ComposerDropdown::EditReasoning
                | ComposerDropdown::Workspace => return,
            }
            return;
        }
        let Some(id) = self.composer_harness_id() else {
            return;
        };
        self.start_harness(id, None);
        if let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) {
            match dropdown {
                ComposerDropdown::Model => {
                    self.send_value(index, json!({"type":"get_available_models"}));
                }
                ComposerDropdown::Reasoning => self.request_thinking_levels(index),
                ComposerDropdown::Project
                | ComposerDropdown::EditModel
                | ComposerDropdown::EditReasoning
                | ComposerDropdown::Workspace => {}
            }
        }
    }
    pub(crate) fn select_model(&mut self, provider: String, model_id: String) {
        self.composer_dropdown = None;
        if self.creating_harness {
            self.draft_model = Some(format!("{provider}/{model_id}"));
            if let Some(project_id) = self.selected_project
                && self
                    .project_probe
                    .as_ref()
                    .is_some_and(|(active_project_id, _)| *active_project_id == project_id)
            {
                self.send_project_value(
                    project_id,
                    json!({"type":"set_model", "provider":provider, "modelId":model_id}),
                );
            }
            return;
        }
        let Some(id) = self.selected_harness else {
            return;
        };
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        self.harnesses[index].model = Some(format!("{provider}/{model_id}"));
        if self.harnesses[index].process.is_none() {
            self.start_harness(id, None);
        } else if self.harnesses[index].startup_settings_pending {
            self.send_startup_model(index);
        } else {
            self.send_value(
                index,
                json!({"type":"set_model", "provider":provider, "modelId":model_id}),
            );
        }
    }
    pub(crate) fn select_thinking(&mut self, level: String) {
        self.composer_dropdown = None;
        if self.creating_harness {
            self.draft_thinking_level = Some(level);
            return;
        }
        let Some(id) = self.selected_harness else {
            return;
        };
        self.start_harness(id, None);
        if let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) {
            self.harnesses[index].thinking_level = Some(level.clone());
            self.send_value(index, json!({"type":"set_thinking_level", "level":level}));
        }
    }
    pub(crate) fn reasoning_options(&self) -> Vec<String> {
        let current_model = if self.creating_harness {
            self.draft_model.as_deref()
        } else {
            self.selected_harness.and_then(|id| {
                self.harnesses
                    .iter()
                    .find(|harness| harness.id == id)
                    .and_then(|harness| harness.model.as_deref())
            })
        };
        reasoning_options_for_model(
            self.selected_project,
            current_model.unwrap_or_default(),
            &self.available_models,
            &self.available_thinking_levels,
        )
    }
    pub(crate) fn toggle_nix(&mut self) {
        if self.creating_harness {
            let Some(project_id) = self.selected_project else {
                return;
            };
            if self.project_has_devshell(project_id) {
                self.draft_nix_enabled = !self.draft_nix_enabled;
                if self
                    .project_probe
                    .as_ref()
                    .is_some_and(|(active_project_id, _)| *active_project_id == project_id)
                {
                    self.project_probe.take();
                    self.start_project_probe(project_id);
                }
            }
            return;
        }

        let Some(id) = self.selected_harness else {
            return;
        };
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        if !self.harness_has_devshell(id) {
            return;
        }
        self.harnesses[index].nix_enabled = !self.harnesses[index].nix_enabled;
        self.persist();
        if self.harnesses[index].status == HarnessStatus::Working {
            self.harnesses[index].nix_restart_pending = true;
        } else {
            self.restart_harness(id);
        }
    }
    pub(super) fn restart_harness(&mut self, id: Id) {
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        if let Some(process) = self.harnesses[index].process.take() {
            process.stop();
        }
        if self.harnesses[index].run_started_at.take().is_some() {
            self.harnesses[index].attention_required = false;
            self.refresh_harness_order(index);
            self.persist();
        }
        self.start_harness(id, None);
    }
    pub(crate) fn project_has_devshell(&self, project_id: Id) -> bool {
        self.projects
            .iter()
            .find(|project| project.id == project_id)
            .is_some_and(|project| project_has_devshell(&project.path))
    }
}
