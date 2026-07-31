use super::*;

pub(super) fn effective_settings_before_entry(
    entries: &[Value],
    target_id: &str,
    mut model: String,
    mut thinking: String,
) -> (String, String) {
    let by_id = entries
        .iter()
        .filter_map(|entry| Some((entry.get("id")?.as_str()?, entry)))
        .collect::<HashMap<_, _>>();
    let mut branch = Vec::new();
    let mut current = by_id
        .get(target_id)
        .and_then(|entry| entry.get("parentId").and_then(Value::as_str));
    let mut visited = HashSet::new();
    while let Some(id) = current {
        if !visited.insert(id) {
            break;
        }
        let Some(entry) = by_id.get(id).copied() else {
            break;
        };
        branch.push(entry);
        current = entry.get("parentId").and_then(Value::as_str);
    }
    branch.reverse();
    for entry in branch {
        match entry.get("type").and_then(Value::as_str) {
            Some("model_change") => {
                if let (Some(provider), Some(model_id)) = (
                    entry.get("provider").and_then(Value::as_str),
                    entry.get("modelId").and_then(Value::as_str),
                ) {
                    model = format!("{provider}/{model_id}");
                }
            }
            Some("thinking_level_change") => {
                if let Some(level) = entry.get("thinkingLevel").and_then(Value::as_str) {
                    thinking = level.to_string();
                }
            }
            _ => {}
        }
    }
    (model, thinking)
}

impl Dirigent {
    pub(crate) fn begin_message_edit(&mut self, message_index: usize, cx: &mut Context<Self>) {
        if self.pi_bridge_extension.is_none() {
            self.banner = Some("The bundled Pi session bridge is unavailable.".into());
            return;
        }
        let Some(harness_id) = self.selected_harness else {
            return;
        };
        let Some(index) = self
            .harnesses
            .iter()
            .position(|harness| harness.id == harness_id)
        else {
            return;
        };
        if self.harnesses[index].status == HarnessStatus::Working {
            self.banner = Some("Wait for Pi to finish before editing an earlier message.".into());
            return;
        }
        let Some(message) = self.harnesses[index].messages.get(message_index) else {
            return;
        };
        if message.role != MessageRole::User || message.queued {
            return;
        }
        let Some(entry_id) = message.entry_id.clone() else {
            self.banner = Some("This message is not saved in Pi yet.".into());
            return;
        };
        let text = message.text.clone();
        let images = message
            .images
            .iter()
            .cloned()
            .enumerate()
            .map(|(image_index, image)| AttachedImage {
                label: format!("image-{}", image_index + 1),
                image,
            })
            .collect::<Vec<_>>();
        let fallback_model = self.harnesses[index].model.clone().unwrap_or_default();
        let fallback_thinking = self.harnesses[index]
            .thinking_level
            .clone()
            .unwrap_or_else(|| "off".into());
        let (model, thinking) = effective_settings_before_entry(
            self.harnesses[index]
                .cached_entries
                .as_deref()
                .unwrap_or_default(),
            &entry_id,
            fallback_model,
            fallback_thinking,
        );
        let input = cx.new(move |cx| {
            let mut input = TextInput::new("Edit message…", cx).borderless().multiline();
            input.restore_draft(text, images, cx);
            input
        });
        cx.subscribe(&input, |this, _, event, cx| match event {
            InputEvent::Submit => this.submit_message_edit(cx),
            InputEvent::Escape => this.cancel_message_edit(cx),
            InputEvent::Focused => {
                this.enter_input_mode(false);
                cx.notify();
            }
            _ => {}
        })
        .detach();
        self.hovered_copy_message = None;
        self.hovered_action_message = None;
        self.hovered_tool_detail_message = None;
        self.editing_message = Some(MessageEdit {
            harness_id,
            message_index,
            entry_id,
            input,
            model,
            thinking,
            submitting: false,
        });
        self.composer_dropdown = None;
        self.focus_input = true;
        self.keyboard_mode = KeyboardMode::Input;
        self.conversation_list
            .remeasure_items(message_index..message_index + 1);
        cx.notify();
    }
    pub(crate) fn cancel_message_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.editing_message.take() else {
            return;
        };
        if edit.submitting {
            self.editing_message = Some(edit);
            return;
        }
        self.composer_dropdown = None;
        self.conversation_list
            .remeasure_items(edit.message_index..edit.message_index + 1);
        cx.notify();
    }
    pub(crate) fn select_edit_model(&mut self, model: String) {
        if let Some(edit) = self.editing_message.as_mut() {
            edit.model = model;
        }
        self.composer_dropdown = None;
    }
    pub(crate) fn select_edit_thinking(&mut self, thinking: String) {
        if let Some(edit) = self.editing_message.as_mut() {
            edit.thinking = thinking;
        }
        self.composer_dropdown = None;
    }
    pub(crate) fn edit_reasoning_options(&self, model: &str) -> Vec<String> {
        reasoning_options_for_model(
            self.selected_project,
            model,
            &self.available_models,
            &self.available_thinking_levels,
        )
    }
    pub(crate) fn submit_message_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.editing_message.as_ref() else {
            return;
        };
        if edit.submitting {
            return;
        }
        let text = edit.input.read(cx).text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let pending = PendingEditSubmit {
            harness_id: edit.harness_id,
            entry_id: edit.entry_id.clone(),
            text,
            images: edit.input.read(cx).images(),
            model: edit.model.clone(),
            thinking: edit.thinking.clone(),
        };
        let Some(index) = self
            .harnesses
            .iter()
            .position(|harness| harness.id == pending.harness_id)
        else {
            return;
        };
        self.start_harness(pending.harness_id, None);
        if self.harnesses[index].process.is_none() || self.harnesses[index].startup_settings_pending
        {
            self.banner = Some("Pi is still starting; try the edit again in a moment.".into());
            return;
        }
        if let Some(edit) = self.editing_message.as_mut() {
            edit.submitting = true;
        }
        let command = format!("/dirigent-navigate {}", pending.entry_id);
        self.pending_edit_submit = Some(pending);
        self.send_value(
            index,
            json!({"id":"dirigent-edit-navigate","type":"prompt","message":command}),
        );
        cx.notify();
    }
    pub(crate) fn begin_message_fork(&mut self, message_index: usize, cx: &mut Context<Self>) {
        if self.pi_bridge_extension.is_none() {
            self.banner = Some("The bundled Pi session bridge is unavailable.".into());
            return;
        }
        let Some(source_harness_id) = self.selected_harness else {
            return;
        };
        let Some(source_index) = self
            .harnesses
            .iter()
            .position(|harness| harness.id == source_harness_id)
        else {
            return;
        };
        if self.harnesses[source_index].status == HarnessStatus::Working {
            self.banner = Some("Wait for Pi to finish before forking this thread.".into());
            return;
        }
        let Some(message) = self.harnesses[source_index].messages.get(message_index) else {
            return;
        };
        let Some(entry_id) = message.entry_id.clone() else {
            self.banner = Some("This message is not saved in Pi yet.".into());
            return;
        };
        let position = match message.role {
            MessageRole::User if !message.queued => "before",
            MessageRole::Assistant => "at",
            _ => return,
        };
        let Some(source_session_file) = self.harnesses[source_index].session_file.clone() else {
            self.banner = Some("Pi has not persisted this thread yet.".into());
            return;
        };
        if !source_session_file.is_file() {
            self.banner = Some("Pi has not finished saving this thread yet.".into());
            return;
        }
        let prefill = (message.role == MessageRole::User).then(|| {
            let images = message
                .images
                .iter()
                .cloned()
                .enumerate()
                .map(|(image_index, image)| AttachedImage {
                    label: format!("image-{}", image_index + 1),
                    image,
                })
                .collect::<Vec<_>>();
            (message.text.clone(), images)
        });
        let root_user_fork = position == "before"
            && self.harnesses[source_index]
                .cached_entries
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|entry| entry.get("id").and_then(Value::as_str) == Some(entry_id.as_str()))
                .is_some_and(|entry| entry.get("parentId").is_none_or(Value::is_null));
        let project_id = self.harnesses[source_index].project_id;
        let title = format!("{} (fork)", self.harnesses[source_index].title);
        let nix_enabled = self.harnesses[source_index].nix_enabled;
        let workspace_id = self.harnesses[source_index].workspace_id.clone();
        let model = self.harnesses[source_index].model.clone();
        let thinking = self.harnesses[source_index].thinking_level.clone();
        let id = self.allocate_id();
        let sidebar_order = self.allocate_sidebar_order();
        let mut harness = if root_user_fork {
            Harness::new(id, project_id, title, sidebar_order)
        } else {
            Harness::restored(
                id,
                project_id,
                title,
                Some(source_session_file),
                nix_enabled,
                workspace_id.clone(),
                false,
                sidebar_order,
            )
        };
        harness.set_derived_title();
        harness.status = if root_user_fork {
            HarnessStatus::Stopped
        } else {
            HarnessStatus::Starting
        };
        harness.nix_enabled = nix_enabled;
        harness.workspace_id = workspace_id;
        harness.model = model;
        harness.thinking_level = thinking;
        self.harnesses.push(harness);
        self.add_composer_input(id, cx);
        if let Some((text, images)) = prefill
            && let Some(input) = self.composer_inputs.get(&id)
        {
            input.update(cx, |input, cx| input.restore_draft(text, images, cx));
        }
        if !root_user_fork {
            self.pending_forks.insert(
                id,
                PendingFork {
                    source_harness_id,
                    entry_id,
                    position,
                },
            );
        }
        self.selected_project = Some(project_id);
        self.selected_harness = Some(id);
        self.last_used_harness = Some(id);
        self.adding_project = false;
        self.creating_harness = false;
        self.editing_message = None;
        self.composer_dropdown = None;
        self.reset_conversation_list(self.harnesses.len() - 1);
        self.focus_input = true;
        self.keyboard_mode = KeyboardMode::Input;
        self.persist();
        if root_user_fork {
            cx.notify();
            return;
        }
        self.start_harness(source_harness_id, None);
        if self.harnesses[source_index].process.is_none()
            || self.harnesses[source_index].startup_settings_pending
        {
            self.fail_pending_fork(
                id,
                "Pi is still starting; try the fork again in a moment.".into(),
            );
            cx.notify();
            return;
        }
        let command = format!(
            "/dirigent-fork {position} {}",
            self.pending_forks[&id].entry_id
        );
        self.send_value(
            source_index,
            json!({"id":"dirigent-fork-command","type":"prompt","message":command}),
        );
        cx.notify();
    }
    pub(super) fn fail_pending_edit(&mut self, message: String) {
        self.banner = Some(message);
        self.pending_edit_submit = None;
        if let Some(edit) = self.editing_message.as_mut() {
            edit.submitting = false;
        }
    }
    pub(super) fn send_pending_edit_model(&mut self, index: usize) {
        let Some(pending) = self.pending_edit_submit.as_ref() else {
            return;
        };
        let Some((provider, model_id)) = pending.model.split_once('/') else {
            self.fail_pending_edit("The selected model has an invalid identifier.".into());
            return;
        };
        self.send_value(
            index,
            json!({
                "id":"dirigent-edit-model",
                "type":"set_model",
                "provider":provider,
                "modelId":model_id,
            }),
        );
    }
    pub(super) fn send_pending_edit_thinking(&mut self, index: usize) {
        let Some(thinking) = self
            .pending_edit_submit
            .as_ref()
            .map(|pending| pending.thinking.clone())
        else {
            return;
        };
        self.send_value(
            index,
            json!({
                "id":"dirigent-edit-thinking",
                "type":"set_thinking_level",
                "level":thinking,
            }),
        );
    }
    pub(super) fn finish_pending_edit(&mut self, index: usize) {
        let Some(pending) = self.pending_edit_submit.take() else {
            return;
        };
        if self.harnesses[index].id != pending.harness_id {
            return;
        }
        self.harnesses[index].model = Some(pending.model);
        self.harnesses[index].thinking_level = Some(pending.thinking);
        self.cache_harness_state(index);
        let image_previews = pending
            .images
            .iter()
            .map(|image| image.image.clone())
            .collect();
        self.harnesses[index]
            .messages
            .push(Message::user_with_images(
                pending.text.clone(),
                image_previews,
            ));
        self.editing_message = None;
        self.composer_dropdown = None;
        self.send_prompt_command(index, pending.text, pending.images, false);
        self.sync_conversation_list(index, None);
    }
    pub(super) fn fail_pending_fork(&mut self, harness_id: Id, message: String) {
        let source_harness_id = self
            .pending_forks
            .remove(&harness_id)
            .map(|pending| pending.source_harness_id);
        if let Some(index) = self
            .harnesses
            .iter()
            .position(|harness| harness.id == harness_id)
        {
            self.harnesses.remove(index);
        }
        self.composer_inputs.remove(&harness_id);
        self.banner = Some(message);
        if let Some(source_harness_id) = source_harness_id
            && let Some(source_index) = self
                .harnesses
                .iter()
                .position(|harness| harness.id == source_harness_id)
        {
            self.selected_project = Some(self.harnesses[source_index].project_id);
            self.selected_harness = Some(source_harness_id);
            self.last_used_harness = Some(source_harness_id);
            self.reset_conversation_list(source_index);
        }
        self.persist();
    }
    pub(super) fn handle_bridge_status(
        &mut self,
        index: usize,
        status_text: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Ok(result) = serde_json::from_str::<Value>(status_text) else {
            return false;
        };
        let operation = result
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let success = result.get("success").and_then(Value::as_bool) == Some(true);
        let error = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or(
                if result.get("cancelled").and_then(Value::as_bool) == Some(true) {
                    "The operation was cancelled by a Pi extension."
                } else {
                    "Pi could not complete the operation."
                },
            )
            .to_string();
        match operation {
            "codex_usage" => {
                if success
                    && let Some(usage) = parse_codex_usage(&result)
                    && self
                        .codex_usage
                        .is_none_or(|current| usage.fetched_at >= current.fetched_at)
                {
                    self.codex_usage = Some(usage);
                }
                true
            }
            "navigate" => {
                let Some(pending) = self.pending_edit_submit.as_ref() else {
                    return true;
                };
                if self.harnesses[index].id != pending.harness_id {
                    return true;
                }
                if !success {
                    self.fail_pending_edit(error);
                    return true;
                }
                let parent_id = self.harnesses[index]
                    .cached_entries
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .find(|entry| {
                        entry.get("id").and_then(Value::as_str) == Some(pending.entry_id.as_str())
                    })
                    .and_then(|entry| entry.get("parentId").and_then(Value::as_str))
                    .map(str::to_string);
                self.harnesses[index].cached_leaf_id = parent_id;
                self.harnesses[index].messages = parse_entries(
                    self.harnesses[index]
                        .cached_entries
                        .as_deref()
                        .unwrap_or_default(),
                    self.harnesses[index].cached_leaf_id.as_deref(),
                );
                self.request_entries(index);
                self.send_pending_edit_model(index);
                true
            }
            "fork" => {
                let source_harness_id = self.harnesses[index].id;
                let Some(target_harness_id) =
                    self.pending_forks.iter().find_map(|(target_id, pending)| {
                        (pending.source_harness_id == source_harness_id).then_some(*target_id)
                    })
                else {
                    return true;
                };
                if !success {
                    self.fail_pending_fork(target_harness_id, error);
                    return true;
                }
                let Some(session_file) = result
                    .get("sessionFile")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                else {
                    self.fail_pending_fork(
                        target_harness_id,
                        "Pi did not report the forked session file.".into(),
                    );
                    return true;
                };
                let pending = self
                    .pending_forks
                    .remove(&target_harness_id)
                    .expect("pending fork must exist");
                let leaf_id = if pending.position == "at" {
                    Some(pending.entry_id)
                } else {
                    self.harnesses[index]
                        .cached_entries
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .find(|entry| {
                            entry.get("id").and_then(Value::as_str)
                                == Some(pending.entry_id.as_str())
                        })
                        .and_then(|entry| entry.get("parentId").and_then(Value::as_str))
                        .map(str::to_string)
                };
                let entries = entries_through_leaf(
                    self.harnesses[index]
                        .cached_entries
                        .as_deref()
                        .unwrap_or_default(),
                    leaf_id.as_deref(),
                );
                self.harnesses[index].process.take();
                self.harnesses[index].status = HarnessStatus::Stopped;
                self.harnesses[index].run_started_at = None;
                let Some(target_index) = self
                    .harnesses
                    .iter()
                    .position(|harness| harness.id == target_harness_id)
                else {
                    return true;
                };
                self.harnesses[target_index].session_file = Some(session_file);
                self.harnesses[target_index].cached_entries = Some(entries);
                self.harnesses[target_index].cached_leaf_id = leaf_id;
                self.harnesses[target_index].messages = parse_entries(
                    self.harnesses[target_index]
                        .cached_entries
                        .as_deref()
                        .unwrap_or_default(),
                    self.harnesses[target_index].cached_leaf_id.as_deref(),
                );
                self.harnesses[target_index].loaded_messages = true;
                self.harnesses[target_index].status = HarnessStatus::Stopped;
                self.selected_project = Some(self.harnesses[target_index].project_id);
                self.selected_harness = Some(target_harness_id);
                self.last_used_harness = Some(target_harness_id);
                self.focus_input = true;
                self.keyboard_mode = KeyboardMode::Input;
                self.persist_composer_draft(target_harness_id, cx);
                self.cache_harness_entries(target_index);
                self.reset_conversation_list(target_index);
                self.persist();
                true
            }
            _ => false,
        }
    }
}
