//! Manages Pi harness startup, composer submission, and session-side caches.

use super::*;

const SLOW_MESSAGE_SEND_STAGE: Duration = Duration::from_millis(250);

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn warn_if_slow(harness_id: Id, operation: &'static str, stage: &'static str, elapsed: Duration) {
    if elapsed >= SLOW_MESSAGE_SEND_STAGE {
        tracing::warn!(
            harness_id,
            operation,
            stage,
            elapsed_ms = duration_ms(elapsed),
            "slow user message send stage"
        );
    }
}

#[derive(Default)]
struct ComposerSendTimings {
    read_input: Duration,
    unarchive: Duration,
    process_start: Duration,
    vcs_refresh: Duration,
    read_images: Duration,
    conversation_update: Duration,
    prompt_send: Duration,
    input_clear: Duration,
    draft_cache: Duration,
    workspace_provision: Duration,
    notify: Duration,
}

impl ComposerSendTimings {
    fn log(
        &self,
        harness: &Harness,
        outcome: &'static str,
        total: Duration,
        prompt_bytes: usize,
        image_count: usize,
        was_working: bool,
    ) {
        let harness_id = harness.id;
        let stages = [
            ("read_input", self.read_input),
            ("unarchive", self.unarchive),
            ("process_start", self.process_start),
            ("vcs_refresh", self.vcs_refresh),
            ("read_images", self.read_images),
            ("conversation_update", self.conversation_update),
            ("prompt_send", self.prompt_send),
            ("input_clear", self.input_clear),
            ("draft_cache", self.draft_cache),
            ("workspace_provision", self.workspace_provision),
            ("notify", self.notify),
        ];
        for (stage, elapsed) in stages {
            warn_if_slow(harness_id, "composer_send", stage, elapsed);
        }
        let measured = stages
            .iter()
            .fold(Duration::ZERO, |total, (_, elapsed)| total + *elapsed);
        let other = total.saturating_sub(measured);
        warn_if_slow(
            harness_id,
            "composer_send",
            "unmeasured_or_descheduled",
            other,
        );
        tracing::info!(
            harness_id,
            outcome,
            total_ms = duration_ms(total),
            read_input_ms = duration_ms(self.read_input),
            unarchive_ms = duration_ms(self.unarchive),
            process_start_ms = duration_ms(self.process_start),
            vcs_refresh_ms = duration_ms(self.vcs_refresh),
            read_images_ms = duration_ms(self.read_images),
            conversation_update_ms = duration_ms(self.conversation_update),
            prompt_send_ms = duration_ms(self.prompt_send),
            input_clear_ms = duration_ms(self.input_clear),
            draft_cache_ms = duration_ms(self.draft_cache),
            workspace_provision_ms = duration_ms(self.workspace_provision),
            notify_ms = duration_ms(self.notify),
            other_ms = duration_ms(other),
            prompt_bytes,
            image_count,
            message_count = harness.messages.len(),
            queued_message_count = harness.queued_messages.len(),
            was_working,
            "user message send timing"
        );
    }
}

/// Uses Pi-reported levels when available, otherwise derives the provider's conventional set.
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
            TextInput::new(COMPOSER_PLACEHOLDER, cx)
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
            harness.cached_entries.as_ref(),
        ) else {
            return;
        };
        if let Err(error) = cache.save_entries(
            session_file,
            Arc::clone(entries),
            harness.cached_leaf_id.as_deref(),
        ) {
            self.report_cache_error(error);
        }
    }
    pub(super) fn cache_harness_entries_incremental(&mut self, index: usize, entries: Vec<Value>) {
        let Some(cache) = self.session_cache.as_ref() else {
            return;
        };
        let harness = &self.harnesses[index];
        let Some(session_file) = harness.session_file.as_deref() else {
            return;
        };
        if let Err(error) =
            cache.append_entries(session_file, entries, harness.cached_leaf_id.as_deref())
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
    /// Queues a draft write; unchanged image payloads can be omitted from frequent text updates.
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
    /// Captures a semantic message anchor, with a scrollbar fraction as a fallback.
    pub(super) fn conversation_scroll_anchor(
        &self,
        harness_index: usize,
    ) -> Option<ConversationScrollAnchor> {
        if self.selected_harness != Some(self.harnesses[harness_index].id)
            || self.conversation_list.is_following_tail()
        {
            return None;
        }
        let scroll_top = self.conversation_list.logical_scroll_top();
        let max_offset = self.conversation_list.max_offset_for_scrollbar().y.as_f32();
        let fallback_fraction = if max_offset > 0.0 {
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
        Some(ConversationScrollAnchor {
            identity: self.conversation_render_cache.scroll_anchor_identity(
                &self.harnesses[harness_index].messages,
                scroll_top.item_ix,
            ),
            offset_in_item: scroll_top.offset_in_item.as_f32(),
            fallback_fraction,
        })
    }

    pub(super) fn reset_conversation_list(&mut self, harness_index: usize) {
        let harness = &self.harnesses[harness_index];
        self.conversation_list_message_count = harness.messages.len();
        self.conversation_list_queued_count = harness.queued_messages.len();
        self.conversation_list_working = harness.status == HarnessStatus::Working;
        self.reset_conversation_render_cache();
    }

    pub(super) fn reset_conversation_list_preserving_scroll(
        &mut self,
        harness_index: usize,
        anchor: Option<ConversationScrollAnchor>,
    ) {
        self.reset_conversation_list(harness_index);
        let Some(anchor) = anchor else {
            return;
        };
        let render_item_index = anchor.identity.as_ref().and_then(|identity| {
            self.conversation_render_cache
                .render_item_index_for_scroll_anchor(
                    &self.harnesses[harness_index].messages,
                    identity,
                )
        });
        if let Some(render_item_index) = render_item_index {
            self.conversation_list.scroll_to(ListOffset {
                item_ix: render_item_index,
                offset_in_item: px(anchor.offset_in_item.max(0.0)),
            });
        } else {
            self.scroll_conversation_to_fraction(anchor.fallback_fraction);
        }
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
        let old_message_count = self.conversation_list_message_count;
        let content_only_update = changed_message.is_some()
            && new_message_count == old_message_count
            && new_queued_count == self.conversation_list_queued_count
            && new_working == self.conversation_list_working;
        if content_only_update {
            // Streaming text and tool-output updates do not change render-item identities or
            // work-group structure. Rebuilding the full conversation cache for every token made
            // long sessions progressively more expensive.
            if let Some(render_item) = changed_message.and_then(|message_index| {
                self.conversation_render_cache
                    .message_render_item_index(message_index)
            }) {
                self.conversation_list
                    .remeasure_items(render_item..render_item + 1);
                self.conversation_render_cache.invalidate_ruler_layout();
            }
        } else {
            let rebuild_from_message = if new_message_count < old_message_count {
                0
            } else {
                changed_message.unwrap_or(old_message_count.min(new_message_count))
            };
            self.sync_conversation_render_cache(rebuild_from_message);
        }
        self.conversation_list_message_count = new_message_count;
        self.conversation_list_queued_count = new_queued_count;
        self.conversation_list_working = new_working;
    }
    pub(super) fn sync_replaced_conversation_tail(
        &mut self,
        harness_index: usize,
        rebuild_from_message: usize,
    ) {
        if self.selected_harness != Some(self.harnesses[harness_index].id) {
            return;
        }
        self.sync_conversation_render_cache(rebuild_from_message);
        let harness = &self.harnesses[harness_index];
        self.conversation_list_message_count = harness.messages.len();
        self.conversation_list_queued_count = harness.queued_messages.len();
        self.conversation_list_working = harness.status == HarnessStatus::Working;
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
    /// Runs an ephemeral Pi process to resolve project-local defaults before a thread exists.
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
    /// Starts Pi if needed and defers any initial prompt until startup settings are acknowledged.
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
        self.harnesses[index].process_state = PiProcessState::Initializing;
        self.harnesses[index].cancellation_pending = false;
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
        if self.harnesses[index].startup_settings_pending
            && self.harnesses[index].delegation.active_run_mut().is_some()
        {
            self.send_value(
                index,
                json!({"id": "dirigent-agent-verify", "type": "get_state"}),
            );
            return;
        }
        self.complete_harness_startup(index);
    }
    pub(super) fn complete_harness_startup(&mut self, index: usize) {
        self.harnesses[index].startup_settings_pending = false;
        self.send_value(index, json!({"id":"dirigent-state","type":"get_state"}));
        self.request_session_stats(index);
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
    pub(super) fn request_session_stats(&mut self, index: usize) {
        self.send_value(index, json!({"type":"get_session_stats"}));
    }
    pub(super) fn request_entries(&mut self, index: usize) {
        // Pi's session tree is append-only during normal operation, so request only entries after
        // the newest cached ID. The response handler falls back to a full read if Pi rejects it.
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
    /// Encodes a prompt and holds it behind asynchronous diff-baseline capture when necessary.
    pub(super) fn send_prompt_command(
        &mut self,
        index: usize,
        prompt: String,
        images: Vec<AttachedImage>,
        steer_if_working: bool,
    ) {
        let started = Instant::now();
        let harness_id = self.harnesses[index].id;
        let prompt_bytes = prompt.len();
        let requested_image_count = images.len();
        let message_count = self.harnesses[index].messages.len();
        let queued_message_count = self.harnesses[index].queued_messages.len();
        let turn_diff_capture_attempted = self.harnesses[index].active_turn_diff.is_none();

        let image_started = Instant::now();
        let mut image_payloads = Vec::with_capacity(images.len());
        for attachment in images {
            let source = format!("composer attachment {}", attachment.label);
            let image = match normalize_for_harness(attachment.image, &source) {
                Ok(image) => image,
                Err(error) => {
                    let image_prepare = image_started.elapsed();
                    let total = started.elapsed();
                    tracing::error!(
                        error = %error,
                        harness_id,
                        label = attachment.label,
                        "could not prepare image attachment for prompt"
                    );
                    warn_if_slow(harness_id, "prompt_command", "image_prepare", image_prepare);
                    tracing::info!(
                        harness_id,
                        outcome = "image_error",
                        total_ms = duration_ms(total),
                        image_prepare_ms = duration_ms(image_prepare),
                        turn_diff_capture_ms = 0_u64,
                        command_build_ms = 0_u64,
                        process_send_ms = 0_u64,
                        prompt_bytes,
                        requested_image_count,
                        encoded_image_count = image_payloads.len(),
                        message_count,
                        queued_message_count,
                        turn_diff_capture_attempted,
                        "prompt command timing"
                    );
                    self.banner = Some(format!("Could not attach {}: {error}", attachment.label));
                    return;
                }
            };
            tracing::debug!(
                harness_id,
                label = attachment.label,
                mime_type = image.format.mime_type(),
                bytes = image.bytes.len(),
                "adding image to prompt command"
            );
            image_payloads.push(json!({
                "type":"image",
                "data":BASE64.encode(&image.bytes),
                "mimeType":image.format.mime_type(),
            }));
        }
        let image_prepare = image_started.elapsed();

        let baseline_already_pending = self.pending_diff_prompts.contains_key(&harness_id);
        let turn_diff_started = Instant::now();
        let baseline_job = if self.harnesses[index].active_turn_diff.is_none()
            && !self.harnesses[index].turn_diff_unavailable
            && !baseline_already_pending
        {
            self.queue_turn_diff_baseline(index, &prompt, true)
        } else {
            None
        };
        let turn_diff_capture = turn_diff_started.elapsed();

        let command_started = Instant::now();
        let mut command = json!({"type":"prompt","message":prompt});
        if !image_payloads.is_empty() {
            command["images"] = Value::Array(image_payloads);
        }
        if steer_if_working && self.harnesses[index].status == HarnessStatus::Working {
            command["streamingBehavior"] = Value::String("steer".into());
        }
        let command_build = command_started.elapsed();

        let process_send_started = Instant::now();
        // No prompt may reach Pi before its baseline. Additional prompts arriving during capture
        // join the same pending command list and preserve their local order.
        let (sent, queued_for_baseline) = if let Some(job_id) = baseline_job {
            self.pending_diff_prompts.insert(
                harness_id,
                PendingDiffPrompt {
                    job_id,
                    process_generation: self.harnesses[index].process_generation,
                    commands: vec![command],
                    started_at: started,
                },
            );
            (true, true)
        } else if let Some(pending) = self.pending_diff_prompts.get_mut(&harness_id) {
            pending.commands.push(command);
            (true, true)
        } else {
            (self.send_value(index, command), false)
        };
        let process_send = process_send_started.elapsed();
        if sent {
            self.mark_harness_working(index);
        }
        let total = started.elapsed();

        let stages = [
            ("image_prepare", image_prepare),
            ("turn_diff_capture", turn_diff_capture),
            ("command_build", command_build),
            ("process_send", process_send),
        ];
        for &(stage, elapsed) in &stages {
            warn_if_slow(harness_id, "prompt_command", stage, elapsed);
        }
        let measured = stages
            .iter()
            .fold(Duration::ZERO, |total, (_, elapsed)| total + *elapsed);
        let other = total.saturating_sub(measured);
        warn_if_slow(
            harness_id,
            "prompt_command",
            "unmeasured_or_descheduled",
            other,
        );
        tracing::info!(
            harness_id,
            outcome = if queued_for_baseline {
                "queued_for_baseline"
            } else if sent {
                "sent"
            } else {
                "send_error"
            },
            total_ms = duration_ms(total),
            image_prepare_ms = duration_ms(image_prepare),
            turn_diff_capture_ms = duration_ms(turn_diff_capture),
            command_build_ms = duration_ms(command_build),
            process_send_ms = duration_ms(process_send),
            other_ms = duration_ms(other),
            prompt_bytes,
            requested_image_count,
            encoded_image_count = requested_image_count,
            message_count,
            queued_message_count,
            turn_diff_capture_attempted,
            "prompt command timing"
        );
    }
    pub(crate) fn send_composer(&mut self, cx: &mut Context<Self>) {
        let started = Instant::now();
        let mut timings = ComposerSendTimings::default();
        let Some(id) = self.selected_harness else {
            return;
        };
        let Some(input) = self.composer_inputs.get(&id).cloned() else {
            return;
        };
        if self
            .harnesses
            .iter()
            .any(|h| h.id == id && h.cancellation_pending)
        {
            self.banner = Some(
                "Wait for the current cancellation to finish before sending another prompt.".into(),
            );
            cx.notify();
            return;
        }
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
        let stage_started = Instant::now();
        let message = input.read(cx).text().trim().to_string();
        timings.read_input = stage_started.elapsed();
        if message.is_empty() {
            return;
        }
        let prompt_bytes = message.len();
        let was_working = self
            .harnesses
            .iter()
            .find(|harness| harness.id == id)
            .is_some_and(|harness| harness.status == HarnessStatus::Working);
        if self
            .harnesses
            .iter()
            .find(|harness| harness.id == id)
            .is_some_and(|harness| harness.archived)
        {
            let stage_started = Instant::now();
            self.set_harness_archived(id, false);
            timings.unarchive = stage_started.elapsed();
        }
        if let Some(source) = self.pending_workspace_sources.remove(&id) {
            // Workspace choice is intentionally deferred until the first prompt. Keep that prompt
            // visible locally while provisioning, then startup will submit it in the new checkout.
            let stage_started = Instant::now();
            let images = input.read(cx).images();
            timings.read_images = stage_started.elapsed();
            let image_count = images.len();
            let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
                return;
            };

            let stage_started = Instant::now();
            self.harnesses[index].pending_initial_prompt = Some((message.clone(), images.clone()));
            self.harnesses[index]
                .messages
                .push(Message::user_with_images(
                    message,
                    images.iter().map(|image| image.image.clone()).collect(),
                ));
            self.sync_conversation_list(index, None);
            timings.conversation_update = stage_started.elapsed();

            let stage_started = Instant::now();
            input.update(cx, |input, cx| input.clear(cx));
            timings.input_clear = stage_started.elapsed();
            self.harnesses[index].composer_draft.clear();
            self.harnesses[index].composer_draft_images.clear();
            let stage_started = Instant::now();
            self.cache_harness_draft(index, true);
            timings.draft_cache = stage_started.elapsed();
            self.composer_dropdown = None;
            let stage_started = Instant::now();
            self.provision_harness_workspace(id, source);
            timings.workspace_provision = stage_started.elapsed();
            let stage_started = Instant::now();
            cx.notify();
            timings.notify = stage_started.elapsed();
            timings.log(
                &self.harnesses[index],
                "workspace_provisioning",
                started.elapsed(),
                prompt_bytes,
                image_count,
                was_working,
            );
            return;
        }

        let stage_started = Instant::now();
        self.start_harness(id, None);
        timings.process_start = stage_started.elapsed();
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        if self.harnesses[index].process.is_none() {
            timings.log(
                &self.harnesses[index],
                "process_unavailable",
                started.elapsed(),
                prompt_bytes,
                0,
                was_working,
            );
            return;
        }

        let stage_started = Instant::now();
        if self.refresh_harness_vcs_label(index) {
            self.persist();
        }
        timings.vcs_refresh = stage_started.elapsed();
        let stage_started = Instant::now();
        let images = input.read(cx).images();
        timings.read_images = stage_started.elapsed();
        let image_count = images.len();
        if self.harnesses[index].startup_settings_pending {
            // The process exists but cannot safely accept a prompt yet. Mirror it immediately in
            // the conversation and let finish_harness_startup send the pending initial prompt.
            let stage_started = Instant::now();
            self.harnesses[index].pending_initial_prompt = Some((message.clone(), images.clone()));
            self.harnesses[index]
                .messages
                .push(Message::user_with_images(
                    message,
                    images.iter().map(|image| image.image.clone()).collect(),
                ));
            self.mark_harness_working(index);
            timings.conversation_update = stage_started.elapsed();

            let stage_started = Instant::now();
            input.update(cx, |input, cx| input.clear(cx));
            timings.input_clear = stage_started.elapsed();
            self.harnesses[index].composer_draft.clear();
            self.harnesses[index].composer_draft_images.clear();
            let stage_started = Instant::now();
            self.cache_harness_draft(index, true);
            timings.draft_cache = stage_started.elapsed();
            let stage_started = Instant::now();
            cx.notify();
            timings.notify = stage_started.elapsed();
            timings.log(
                &self.harnesses[index],
                "queued_during_startup",
                started.elapsed(),
                prompt_bytes,
                image_count,
                was_working,
            );
            return;
        }

        // Pi treats prompts sent during a turn as steering input. They stay in a separate local
        // queue until a queue update confirms which messages Pi accepted.
        let steering = self.harnesses[index].status == HarnessStatus::Working;
        let stage_started = Instant::now();
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
        timings.conversation_update = stage_started.elapsed();

        let stage_started = Instant::now();
        self.send_prompt_command(index, message, images, true);
        timings.prompt_send = stage_started.elapsed();
        let stage_started = Instant::now();
        input.update(cx, |input, cx| input.clear(cx));
        timings.input_clear = stage_started.elapsed();
        self.harnesses[index].composer_draft.clear();
        self.harnesses[index].composer_draft_images.clear();
        let stage_started = Instant::now();
        self.cache_harness_draft(index, true);
        timings.draft_cache = stage_started.elapsed();
        let stage_started = Instant::now();
        cx.notify();
        timings.notify = stage_started.elapsed();
        timings.log(
            &self.harnesses[index],
            if steering { "steered" } else { "sent" },
            started.elapsed(),
            prompt_bytes,
            image_count,
            was_working,
        );
    }
    pub(crate) fn abort_selected(&mut self) {
        if let Some(id) = self.selected_harness {
            self.abort_harness(id);
        }
    }
    pub(crate) fn abort_harness(&mut self, id: Id) {
        self.stop_delegation(id);
        let Some(index) = self.harnesses.iter().position(|h| h.id == id) else {
            return;
        };
        self.finish_delegated_run(index, crate::delegation::WorkStatus::Cancelled);
        self.harnesses[index].pending_initial_prompt = None;
        self.pending_diff_prompts.remove(&id);
        self.harnesses[index].cancellation_pending = self.harnesses[index].process.is_some();
        if let Some(process) = self.harnesses[index].process.as_ref() {
            // Chain abort from the clear acknowledgement: RPC dispatches commands concurrently.
            let _ = process.send(json!({"id": "dirigent-cancel-clear", "type":"clear_queue"}));
        }
        self.mark_turn_diff_status(index, TurnDiffStatus::Aborted);
        // A cancelled child's startup may still finish configuring its explicitly selected model.
        // Removing the pending prompt is sufficient to prevent it from executing the assignment.
        if self.harnesses[index].delegation.parent.is_none() {
            self.harnesses[index].startup_settings_pending = false;
        }
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
        self.stop_delegation(id);
        self.finish_delegated_run(index, crate::delegation::WorkStatus::Interrupted);
        self.harnesses[index].process_state = PiProcessState::Stopped;
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
