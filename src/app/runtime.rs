use super::*;

impl Dirigent {
    pub(super) fn fail_harness(&mut self, index: usize, error: String) {
        self.finish_turn_diff(index, TurnDiffStatus::Failed);
        tracing::error!(
            error = %error,
            harness_id = self.harnesses[index].id,
            generation = self.harnesses[index].process_generation,
            "harness failed"
        );
        self.harnesses[index].status = HarnessStatus::Failed;
        self.harnesses[index].run_started_at = None;
        self.harnesses[index].attention_required = false;
        self.harnesses[index].error = Some(error.clone());
        self.harnesses[index].messages.push(Message::error(error));
        self.refresh_harness_order(index);
        self.persist();
        self.sync_conversation_list(index, None);
    }
    pub(super) fn handle_runtime_error(&mut self, target: RuntimeTarget, message: String) {
        tracing::error!(error = %message, ?target, "Pi runtime error");
        match target {
            RuntimeTarget::Harness(harness_id, generation) => {
                if let Some(index) = self.harnesses.iter().position(|harness| {
                    harness.id == harness_id && harness.process_generation == generation
                }) {
                    self.harnesses[index].error = Some(message.clone());
                    self.harnesses[index].messages.push(Message::error(message));
                    if self.harnesses[index].status == HarnessStatus::Starting {
                        self.harnesses[index].status = HarnessStatus::Failed;
                        self.harnesses[index].run_started_at = None;
                        self.refresh_harness_order(index);
                        self.persist();
                    }
                    self.sync_conversation_list(index, None);
                }
            }
            RuntimeTarget::Project(project_id)
                if self
                    .project_probe
                    .as_ref()
                    .is_some_and(|(active_project_id, _)| *active_project_id == project_id) =>
            {
                self.banner = Some(message);
            }
            RuntimeTarget::Project(_) => {}
        }
    }
    pub(super) fn handle_runtime_exit(&mut self, target: RuntimeTarget) {
        let RuntimeTarget::Harness(harness_id, generation) = target else {
            return;
        };
        let Some(index) = self.harnesses.iter().position(|harness| {
            harness.id == harness_id && harness.process_generation == generation
        }) else {
            return;
        };
        if self
            .pending_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.harness_id == harness_id)
        {
            self.pending_dialog = None;
        }
        self.finish_turn_diff(index, TurnDiffStatus::Interrupted);
        self.harnesses[index].process.take();
        self.harnesses[index].retry_status = None;
        self.harnesses[index].steering_queue.clear();
        self.harnesses[index].follow_up_queue.clear();
        for mut message in std::mem::take(&mut self.harnesses[index].queued_messages) {
            message.queued = false;
            self.harnesses[index].messages.push(message);
        }
        let transitioned = self.harnesses[index].status != HarnessStatus::Stopped
            || self.harnesses[index].run_started_at.is_some()
            || self.harnesses[index].attention_required;
        if self.harnesses[index].status != HarnessStatus::Failed {
            self.harnesses[index].status = HarnessStatus::Stopped;
        }
        self.harnesses[index].run_started_at = None;
        self.harnesses[index].attention_required = false;
        if transitioned {
            self.refresh_harness_order(index);
            self.persist();
        }
        self.sync_conversation_list(index, None);
    }
    pub(super) fn handle_runtime_event(&mut self, event: RuntimeEvent, cx: &mut Context<Self>) {
        let (target, value) = match event {
            RuntimeEvent::Json { target, value } => (target, value),
            RuntimeEvent::Error { target, message } => {
                self.handle_runtime_error(target, message);
                return;
            }
            RuntimeEvent::Diagnostic { target, message } => {
                tracing::warn!(diagnostic = %message, ?target, "Pi diagnostic");
                return;
            }
            RuntimeEvent::Exited { target } => {
                self.handle_runtime_exit(target);
                return;
            }
        };
        let (harness_id, generation) = match target {
            RuntimeTarget::Harness(harness_id, generation) => (harness_id, generation),
            RuntimeTarget::Project(project_id) => {
                if value.get("type").and_then(Value::as_str) == Some("response") {
                    self.handle_project_response(project_id, &value);
                }
                return;
            }
        };
        let Some(index) = self.harnesses.iter().position(|harness| {
            harness.id == harness_id && harness.process_generation == generation
        }) else {
            return;
        };
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let replaces_messages = event_type == "response"
            && value.get("success").and_then(Value::as_bool) != Some(false)
            && matches!(
                value.get("command").and_then(Value::as_str),
                Some("get_entries" | "get_messages")
            )
            && !self.harnesses[index].loaded_messages;
        let changed_message = match event_type {
            "agent_start" => {
                self.begin_turn_diff(index, "Agent turn");
                self.harnesses[index].status = HarnessStatus::Working;
                if let Some(retry) = self.harnesses[index].retry_status.as_mut() {
                    retry.waiting = false;
                    self.remeasure_working_indicator(index);
                }
                if self.harnesses[index].run_started_at.is_none() {
                    self.harnesses[index].run_started_at = Some(Instant::now());
                    self.harnesses[index].last_run_duration = None;
                    self.refresh_harness_order(index);
                    self.persist();
                }
                None
            }
            "agent_settled" => {
                let changed_message = self.settle_harness(index);
                self.request_context_usage(index);
                self.request_entries(index);
                changed_message
            }
            "compaction_start" => {
                self.handle_compaction_start(index, &value);
                None
            }
            "compaction_end" => {
                let changed_message = self.handle_compaction_end(index, &value);
                self.request_context_usage(index);
                changed_message
            }
            "message_end" => {
                let changed_message = self.handle_message_end(index, &value);
                self.request_context_usage(index);
                changed_message
            }
            "message_update" => self.handle_message_update(index, &value),
            "tool_execution_start" => {
                self.handle_tool_start(index, &value);
                None
            }
            "tool_execution_update" => self.handle_tool_update(index, &value),
            "tool_execution_end" => self.handle_tool_end(index, &value),
            "queue_update" => {
                self.handle_queue_update(index, &value);
                None
            }
            "auto_retry_start" => {
                self.handle_auto_retry_start(index, &value);
                None
            }
            "auto_retry_end" => self.handle_auto_retry_end(index, &value),
            "response" => {
                self.handle_response(index, &value);
                None
            }
            "extension_error" => {
                let error = value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("A pi extension failed.");
                tracing::error!(error, harness_id, "Pi extension failed");
                self.harnesses[index].messages.push(Message::error(error));
                None
            }
            "extension_ui_request" => {
                self.handle_extension_ui(index, &value, cx);
                None
            }
            _ => None,
        };
        if replaces_messages && self.selected_harness == Some(self.harnesses[index].id) {
            self.reset_conversation_list(index);
        } else {
            self.sync_conversation_list(index, changed_message);
        }
    }
    pub(super) fn handle_compaction_start(&mut self, index: usize, value: &Value) {
        let reason = value.get("reason").and_then(Value::as_str);
        self.harnesses[index]
            .messages
            .push(Message::compaction(reason, None, true));
    }
    pub(super) fn handle_compaction_end(&mut self, index: usize, value: &Value) -> Option<usize> {
        let reason = value.get("reason").and_then(Value::as_str);
        let summary = value
            .pointer("/result/summary")
            .and_then(Value::as_str)
            .map(truncate_output);
        let message_index = self.harnesses[index]
            .messages
            .iter()
            .rposition(|message| message.is_compaction() && message.running)
            .unwrap_or_else(|| {
                self.harnesses[index]
                    .messages
                    .push(Message::compaction(reason, None, true));
                self.harnesses[index].messages.len() - 1
            });
        let message = &mut self.harnesses[index].messages[message_index];
        message.set_running(false);
        if let Some(summary) = summary {
            message.set_detail(Some(summary));
        } else if value.get("aborted").and_then(Value::as_bool) == Some(true) {
            message.append_text(" · aborted");
        } else {
            message.append_text(" · failed");
            if let Some(error) = value.get("errorMessage").and_then(Value::as_str) {
                message.set_detail(Some(truncate_output(error)));
            }
        }
        Some(message_index)
    }
    pub(super) fn remeasure_working_indicator(&mut self, index: usize) {
        if self.selected_harness == Some(self.harnesses[index].id)
            && self.harnesses[index].status == HarnessStatus::Working
        {
            let indicator = self.harnesses[index].messages.len();
            self.conversation_list
                .remeasure_items(indicator..indicator + 1);
        }
    }
    pub(super) fn handle_queue_update(&mut self, index: usize, value: &Value) {
        let steering = rpc_string_array(value, "steering");
        let follow_up = rpc_string_array(value, "followUp");
        let harness = &mut self.harnesses[index];
        let accepted = reconcile_queued_messages(&mut harness.queued_messages, &steering);
        harness.messages.extend(accepted);
        harness.steering_queue = steering;
        harness.follow_up_queue = follow_up;
    }
    pub(super) fn handle_auto_retry_start(&mut self, index: usize, value: &Value) {
        self.harnesses[index].retry_status = Some(RetryStatus {
            attempt: value.get("attempt").and_then(Value::as_u64).unwrap_or(1),
            max_attempts: value
                .get("maxAttempts")
                .and_then(Value::as_u64)
                .unwrap_or(1),
            delay_ms: value.get("delayMs").and_then(Value::as_u64).unwrap_or(0),
            error_message: value
                .get("errorMessage")
                .and_then(Value::as_str)
                .unwrap_or("Transient provider error")
                .to_string(),
            waiting: true,
        });
        self.remeasure_working_indicator(index);
    }
    pub(super) fn handle_auto_retry_end(&mut self, index: usize, value: &Value) -> Option<usize> {
        let previous = self.harnesses[index].retry_status.take();
        self.remeasure_working_indicator(index);
        if value.get("success").and_then(Value::as_bool) != Some(false) {
            return None;
        }
        let attempt = value
            .get("attempt")
            .and_then(Value::as_u64)
            .or_else(|| previous.as_ref().map(|retry| retry.attempt))
            .unwrap_or(1);
        let error = value
            .get("finalError")
            .and_then(Value::as_str)
            .unwrap_or("Unknown error");
        let message = format!("Automatic retry failed after {attempt} attempt(s): {error}");
        tracing::error!(
            error,
            attempt,
            harness_id = self.harnesses[index].id,
            "automatic retry failed"
        );
        self.mark_turn_diff_status(index, TurnDiffStatus::Failed);
        self.harnesses[index].status = HarnessStatus::Failed;
        self.harnesses[index].error = Some(message.clone());
        self.harnesses[index].messages.push(Message::error(message));
        self.harnesses[index].messages.len().checked_sub(1)
    }
    pub(super) fn handle_message_end(&mut self, index: usize, value: &Value) -> Option<usize> {
        let message = value.get("message").unwrap_or(&Value::Null);
        let error = assistant_failure(message)?;
        if self.harnesses[index]
            .messages
            .last()
            .is_some_and(|last| last.role == MessageRole::Error && last.text == error)
        {
            return self.harnesses[index].messages.len().checked_sub(1);
        }
        tracing::error!(error = %error, harness_id = self.harnesses[index].id, "assistant request failed");
        self.mark_turn_diff_status(index, TurnDiffStatus::Failed);
        self.harnesses[index].error = Some(error.clone());
        self.harnesses[index].messages.push(Message::error(error));
        self.harnesses[index].messages.len().checked_sub(1)
    }
    pub(super) fn handle_message_update(&mut self, index: usize, value: &Value) -> Option<usize> {
        let update = value.get("assistantMessageEvent").unwrap_or(&Value::Null);
        match update.get("type").and_then(Value::as_str) {
            Some("text_delta") | Some("thinking_delta") => {
                let role = if update.get("type").and_then(Value::as_str) == Some("thinking_delta") {
                    MessageRole::Thinking
                } else {
                    MessageRole::Assistant
                };
                let delta = update
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(message) = self.harnesses[index]
                    .messages
                    .last_mut()
                    .filter(|message| message.role == role && message.running)
                {
                    message.append_text(delta);
                } else {
                    let mut message = Message::new(role, delta);
                    message.set_running(true);
                    self.harnesses[index].messages.push(message);
                }
                return self.harnesses[index].messages.len().checked_sub(1);
            }
            Some("text_end") | Some("thinking_end") => {
                let role = if update.get("type").and_then(Value::as_str) == Some("thinking_end") {
                    MessageRole::Thinking
                } else {
                    MessageRole::Assistant
                };
                if let Some(message_index) = self.harnesses[index]
                    .messages
                    .iter()
                    .rposition(|message| message.role == role && message.running)
                {
                    self.harnesses[index].messages[message_index].set_running(false);
                    return Some(message_index);
                }
            }
            Some("done") | Some("error") => {
                let mut changed_message = None;
                for (message_index, message) in self.harnesses[index]
                    .messages
                    .iter_mut()
                    .enumerate()
                    .filter(|(_, message)| {
                        matches!(message.role, MessageRole::Assistant | MessageRole::Thinking)
                            && message.running
                    })
                {
                    message.set_running(false);
                    changed_message = Some(message_index);
                }
                return changed_message;
            }
            _ => {}
        }
        None
    }
    pub(super) fn settle_harness(&mut self, index: usize) -> Option<usize> {
        self.finish_turn_diff(index, TurnDiffStatus::Completed);
        if self.harnesses[index].status != HarnessStatus::Failed {
            self.harnesses[index].status = HarnessStatus::Idle;
        }
        self.harnesses[index].retry_status = None;
        self.harnesses[index].steering_queue.clear();
        self.harnesses[index].follow_up_queue.clear();
        for mut message in std::mem::take(&mut self.harnesses[index].queued_messages) {
            message.queued = false;
            self.harnesses[index].messages.push(message);
        }
        let run_duration = self.harnesses[index]
            .run_started_at
            .take()
            .map(|started_at| started_at.elapsed());
        self.harnesses[index].last_run_duration = run_duration;
        self.harnesses[index].attention_required = false;
        if self.selected_harness != Some(self.harnesses[index].id) {
            self.harnesses[index].has_unread_completion = true;
        }
        self.refresh_harness_order(index);
        self.persist();
        let mut changed_message = None;
        for (message_index, message) in self.harnesses[index].messages.iter_mut().enumerate() {
            if message.running {
                if message.role == MessageRole::Assistant {
                    changed_message = Some(message_index);
                }
                message.set_running(false);
            }
        }
        if self.harnesses[index].nix_restart_pending {
            self.harnesses[index].nix_restart_pending = false;
            let id = self.harnesses[index].id;
            self.restart_harness(id);
        }
        changed_message
    }
    pub(super) fn handle_tool_start(&mut self, index: usize, value: &Value) {
        let name = value
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let args = value.get("args").unwrap_or(&Value::Null);
        self.harnesses[index].messages.push(tool_message(
            name,
            args,
            value
                .get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_string),
            true,
        ));
    }
    pub(super) fn handle_tool_update(&mut self, index: usize, value: &Value) -> Option<usize> {
        let id = value.get("toolCallId").and_then(Value::as_str);
        let detail = value
            .pointer("/partialResult/content/0/text")
            .and_then(Value::as_str)
            .map(truncate_output)?;
        let message_index = self.harnesses[index]
            .messages
            .iter()
            .rposition(|message| message.tool_call_id.as_deref() == id)?;
        self.harnesses[index].messages[message_index].set_detail(Some(detail));
        Some(message_index)
    }
    pub(super) fn handle_tool_end(&mut self, index: usize, value: &Value) -> Option<usize> {
        let id = value.get("toolCallId").and_then(Value::as_str);
        let name = value
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let is_error = value.get("isError").and_then(Value::as_bool) == Some(true);
        let detail = value
            .get("result")
            .and_then(|result| tool_result_detail(name, result, is_error));
        if is_error {
            // A failed tool call is an expected agent outcome and is already shown in the
            // conversation. Keep it available only for opt-in diagnostics.
            tracing::debug!(
                harness_id = self.harnesses[index].id,
                tool = name,
                error = detail.as_deref().unwrap_or("tool execution failed"),
                "Pi tool execution failed"
            );
        }
        let message_index = self.harnesses[index]
            .messages
            .iter()
            .rposition(|message| message.tool_call_id.as_deref() == id)?;
        let message = &mut self.harnesses[index].messages[message_index];
        message.finish_tool(is_error, None);
        if detail.is_some() && (name != "write" || is_error || message.detail.is_none()) {
            message.set_detail(detail);
        }
        Some(message_index)
    }
    pub(super) fn handle_project_response(&mut self, project_id: Id, value: &Value) {
        if !self
            .project_probe
            .as_ref()
            .is_some_and(|(active_project_id, _)| *active_project_id == project_id)
        {
            return;
        }
        if value.get("success").and_then(Value::as_bool) == Some(false) {
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Pi could not load project settings.")
                .to_string();
            tracing::error!(error = %error, project_id, "Pi project request failed");
            self.banner = Some(error);
            return;
        }
        match value.get("command").and_then(Value::as_str) {
            Some("get_state") => {
                let data = value.get("data").unwrap_or(&Value::Null);
                let thinking_level = data
                    .get("thinkingLevel")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let model = data.get("model").and_then(|model| {
                    let provider = model.get("provider")?.as_str()?;
                    let id = model.get("id")?.as_str()?;
                    Some(format!("{provider}/{id}"))
                });
                if self.creating_harness && self.selected_project == Some(project_id) {
                    if self.draft_model.is_none() {
                        self.draft_model = model;
                    }
                    if self.draft_thinking_level.is_none() {
                        self.draft_thinking_level = thinking_level;
                    }
                }
                if self.draft_model.is_some() {
                    self.send_project_value(
                        project_id,
                        json!({"type":"get_available_thinking_levels"}),
                    );
                }
            }
            Some("get_available_models") => {
                let models = value
                    .pointer("/data/models")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(parse_available_model)
                    .collect::<Vec<_>>();
                self.available_models_by_project
                    .insert(project_id, models.clone());
                if self.selected_project == Some(project_id) {
                    self.available_models = models;
                }
                self.cache_project_models(project_id);
            }
            Some("get_available_thinking_levels") => {
                let Some(model) = self.draft_model.clone() else {
                    return;
                };
                let levels = value
                    .pointer("/data/levels")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                self.available_thinking_levels
                    .insert((project_id, model.clone()), levels);
                self.cache_model_thinking_levels(project_id, &model);
            }
            Some("set_model") => {
                self.send_project_value(
                    project_id,
                    json!({"id":"dirigent-project-state","type":"get_state"}),
                );
            }
            _ => {}
        }
    }
    pub(super) fn handle_response(&mut self, index: usize, value: &Value) {
        let command = value.get("command").and_then(Value::as_str);
        let request_id = value.get("id").and_then(Value::as_str);
        let startup_request = self.harnesses[index].startup_settings_pending
            && matches!(
                (request_id, command),
                (Some(STARTUP_MODEL_REQUEST_ID), Some("set_model"))
                    | (
                        Some(STARTUP_THINKING_REQUEST_ID),
                        Some("set_thinking_level")
                    )
            );
        if value.get("success").and_then(Value::as_bool) == Some(false) {
            if startup_request {
                self.harnesses[index].process.take();
                let error = value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Pi rejected a startup setting.")
                    .to_string();
                self.fail_harness(index, error);
                return;
            }
            if matches!(
                request_id,
                Some("dirigent-edit-navigate" | "dirigent-edit-model" | "dirigent-edit-thinking")
            ) {
                let error = value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Pi rejected the edited branch operation.")
                    .to_string();
                self.fail_pending_edit(error);
                return;
            }
            if request_id == Some("dirigent-fork-command") {
                let source_harness_id = self.harnesses[index].id;
                let target_harness_id =
                    self.pending_forks.iter().find_map(|(target_id, pending)| {
                        (pending.source_harness_id == source_harness_id).then_some(*target_id)
                    });
                let error = value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Pi rejected the fork operation.")
                    .to_string();
                if let Some(target_harness_id) = target_harness_id {
                    self.fail_pending_fork(target_harness_id, error);
                }
                return;
            }
            if command == Some("prompt") {
                self.harnesses[index].queued_messages.pop();
            }
            if command == Some("get_entries")
                && value.get("id").and_then(Value::as_str) == Some("dirigent-entries-incremental")
            {
                self.send_value(
                    index,
                    json!({"id":"dirigent-entries-full","type":"get_entries"}),
                );
                return;
            }
            if matches!(command, Some("set_model" | "set_thinking_level")) {
                self.send_value(index, json!({"id":"dirigent-state","type":"get_state"}));
            }
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Pi rejected a command.");
            tracing::error!(
                error,
                harness_id = self.harnesses[index].id,
                command = command.unwrap_or("unknown"),
                request_id = request_id.unwrap_or("unknown"),
                "Pi command failed"
            );
            self.harnesses[index].messages.push(Message::error(error));
            return;
        }
        if command == Some("prompt") {
            let steering = self.harnesses[index].steering_queue.clone();
            let accepted =
                reconcile_queued_messages(&mut self.harnesses[index].queued_messages, &steering);
            self.harnesses[index].messages.extend(accepted);
        }
        if startup_request {
            match command {
                Some("set_model") => self.send_startup_thinking_level(index),
                Some("set_thinking_level") => self.finish_harness_startup(index),
                _ => {}
            }
            return;
        }
        if request_id == Some("dirigent-edit-model") {
            self.send_pending_edit_thinking(index);
            return;
        }
        if request_id == Some("dirigent-edit-thinking") {
            self.finish_pending_edit(index);
            return;
        }
        match command {
            Some("get_state") => {
                let data = value.get("data").unwrap_or(&Value::Null);
                self.harnesses[index].session_file = data
                    .get("sessionFile")
                    .and_then(Value::as_str)
                    .map(PathBuf::from);
                let thinking_level = data
                    .get("thinkingLevel")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let model = data.get("model").and_then(|model| {
                    let provider = model.get("provider")?.as_str()?;
                    let id = model.get("id")?.as_str()?;
                    Some(format!("{provider}/{id}"))
                });
                self.harnesses[index].thinking_level = thinking_level.clone();
                self.harnesses[index].model = model.clone();
                if self.creating_harness
                    && self.selected_project == Some(self.harnesses[index].project_id)
                {
                    if self.draft_thinking_level.is_none() {
                        self.draft_thinking_level = thinking_level;
                    }
                    if self.draft_model.is_none() {
                        self.draft_model = model;
                    }
                }
                let reported_status = if data
                    .get("isStreaming")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    HarnessStatus::Working
                } else {
                    HarnessStatus::Idle
                };
                if self.harnesses[index].status != HarnessStatus::Working
                    || reported_status == HarnessStatus::Working
                {
                    self.harnesses[index].status = reported_status;
                }
                if reported_status == HarnessStatus::Working
                    && self.harnesses[index].run_started_at.is_none()
                {
                    self.harnesses[index].run_started_at = Some(Instant::now());
                    self.harnesses[index].last_run_duration = None;
                    self.refresh_harness_order(index);
                }
                self.persist();
                self.cache_harness_state(index);
                self.cache_harness_draft(index, true);
                self.request_thinking_levels(index);
            }
            Some("get_entries") => {
                let incoming = value
                    .pointer("/data/entries")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let incremental =
                    value.get("id").and_then(Value::as_str) == Some("dirigent-entries-incremental");
                if incremental {
                    let entries = self.harnesses[index]
                        .cached_entries
                        .get_or_insert_with(Vec::new);
                    let mut known_ids = entries
                        .iter()
                        .filter_map(|entry| entry.get("id")?.as_str().map(str::to_string))
                        .collect::<HashSet<_>>();
                    entries.extend(incoming.into_iter().filter(|entry| {
                        entry
                            .get("id")
                            .and_then(Value::as_str)
                            .is_none_or(|id| known_ids.insert(id.to_string()))
                    }));
                } else {
                    self.harnesses[index].cached_entries = Some(incoming);
                }
                self.harnesses[index].cached_leaf_id = value
                    .pointer("/data/leafId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.prune_turn_diffs_to_active_branch(index);
                self.harnesses[index].messages = parse_entries(
                    self.harnesses[index]
                        .cached_entries
                        .as_deref()
                        .unwrap_or_default(),
                    self.harnesses[index].cached_leaf_id.as_deref(),
                );
                self.harnesses[index].loaded_messages = true;
                self.cache_harness_entries(index);
            }
            Some("get_messages") if !self.harnesses[index].loaded_messages => {
                if let Some(messages) = value.pointer("/data/messages").and_then(Value::as_array) {
                    self.harnesses[index].messages = parse_messages(messages);
                }
                self.harnesses[index].loaded_messages = true;
            }
            Some("get_available_models") => {
                let models = value
                    .pointer("/data/models")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(parse_available_model)
                    .collect::<Vec<_>>();
                let project_id = self.harnesses[index].project_id;
                self.available_models_by_project
                    .insert(project_id, models.clone());
                if self.selected_project == Some(project_id) {
                    self.available_models = models;
                }
                self.cache_project_models(project_id);
            }
            Some("get_available_thinking_levels") => {
                let Some(model) = self.harnesses[index].model.clone() else {
                    return;
                };
                let levels = value
                    .pointer("/data/levels")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                let project_id = self.harnesses[index].project_id;
                self.available_thinking_levels
                    .insert((project_id, model.clone()), levels);
                self.cache_model_thinking_levels(project_id, &model);
            }
            Some("get_session_stats") => {
                self.harnesses[index].context_usage = parse_context_usage(value);
            }
            Some("set_model") => {
                let model = value.get("data").unwrap_or(&Value::Null);
                if let (Some(provider), Some(id)) = (
                    model.get("provider").and_then(Value::as_str),
                    model.get("id").and_then(Value::as_str),
                ) {
                    self.harnesses[index].model = Some(format!("{provider}/{id}"));
                }
                self.cache_harness_state(index);
                self.request_context_usage(index);
                self.request_thinking_levels(index);
            }
            Some("set_thinking_level") => {
                self.cache_harness_state(index);
            }
            Some("cycle_model") => {
                if let Some(model) = value.pointer("/data/model") {
                    let provider = model.get("provider").and_then(Value::as_str);
                    let id = model.get("id").and_then(Value::as_str);
                    if let (Some(provider), Some(id)) = (provider, id) {
                        self.harnesses[index].model = Some(format!("{provider}/{id}"));
                    }
                }
                self.cache_harness_state(index);
                self.request_thinking_levels(index);
            }
            Some("cycle_thinking_level") => {
                self.harnesses[index].thinking_level = value
                    .pointer("/data/level")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.cache_harness_state(index);
            }
            _ => {}
        }
    }
    pub(super) fn clear_harness_attention(&mut self, index: usize) {
        if !self.harnesses[index].attention_required {
            return;
        }
        self.harnesses[index].attention_required = false;
        self.refresh_harness_order(index);
        self.persist();
    }
    pub(crate) fn respond_extension_value(&mut self, value: String) {
        let Some(dialog) = self.pending_dialog.take() else {
            return;
        };
        let Some(index) = self
            .harnesses
            .iter()
            .position(|harness| harness.id == dialog.harness_id)
        else {
            return;
        };
        self.send_value(
            index,
            json!({
                "type":"extension_ui_response",
                "id":dialog.request_id,
                "value":value
            }),
        );
        self.clear_harness_attention(index);
    }
    pub(crate) fn respond_extension_confirmation(&mut self, confirmed: bool) {
        let Some(dialog) = self.pending_dialog.take() else {
            return;
        };
        let Some(index) = self
            .harnesses
            .iter()
            .position(|harness| harness.id == dialog.harness_id)
        else {
            return;
        };
        self.send_value(
            index,
            json!({
                "type":"extension_ui_response",
                "id":dialog.request_id,
                "confirmed":confirmed
            }),
        );
        self.clear_harness_attention(index);
    }
    pub(crate) fn cancel_extension_dialog(&mut self) {
        let Some(dialog) = self.pending_dialog.take() else {
            return;
        };
        let Some(index) = self
            .harnesses
            .iter()
            .position(|harness| harness.id == dialog.harness_id)
        else {
            return;
        };
        self.send_value(
            index,
            json!({
                "type":"extension_ui_response",
                "id":dialog.request_id,
                "cancelled":true
            }),
        );
        self.clear_harness_attention(index);
    }
    pub(crate) fn submit_extension_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(kind) = self.pending_dialog.as_ref().map(|dialog| dialog.kind) else {
            return;
        };
        if matches!(kind, DialogKind::Input | DialogKind::Editor) {
            let value = self.extension_input.read(cx).text().to_string();
            self.respond_extension_value(value);
            self.extension_input.update(cx, |input, cx| input.clear(cx));
            cx.notify();
        }
    }
    pub(super) fn handle_extension_ui(
        &mut self,
        index: usize,
        value: &Value,
        cx: &mut Context<Self>,
    ) {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match method {
            "notify" => {
                if let Some(message) = value.get("message").and_then(Value::as_str) {
                    self.harnesses[index]
                        .messages
                        .push(Message::notice(message));
                    self.harnesses[index].has_unread_completion = true;
                    self.refresh_harness_order(index);
                    self.persist();
                }
            }
            "setStatus" => {
                if value.get("statusKey").and_then(Value::as_str) == Some("__dirigent_bridge__")
                    && let Some(message) = value.get("statusText").and_then(Value::as_str)
                    && self.handle_bridge_status(index, message, cx)
                {
                    return;
                }
                if let Some(message) = value.get("statusText").and_then(Value::as_str) {
                    self.harnesses[index]
                        .messages
                        .push(Message::notice(message));
                }
            }
            "setTitle" => {
                if let Some(title) = value.get("title").and_then(Value::as_str) {
                    self.harnesses[index]
                        .messages
                        .push(Message::notice(format!("Pi title: {title}")));
                }
            }
            "set_editor_text" => {
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    self.harnesses[index].has_unread_completion = false;
                    self.harnesses[index].attention_required = true;
                    self.refresh_harness_order(index);
                    self.selected_project = Some(self.harnesses[index].project_id);
                    self.show_cached_models(self.harnesses[index].project_id);
                    self.selected_harness = Some(self.harnesses[index].id);
                    self.last_used_harness = Some(self.harnesses[index].id);
                    self.adding_project = false;
                    self.creating_harness = false;
                    self.persist();
                    if let Some(input) = self.composer_inputs.get(&self.harnesses[index].id) {
                        input.update(cx, |input, cx| input.set_text(text, cx));
                    }
                    self.persist_composer_draft(self.harnesses[index].id, cx);
                }
            }
            "setWidget" => {
                if let Some(lines) = value.get("widgetLines").and_then(Value::as_array) {
                    let text = lines
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        self.harnesses[index].messages.push(Message::notice(text));
                    }
                }
            }
            "select" | "confirm" | "input" | "editor" => {
                let kind = match method {
                    "select" => DialogKind::Select,
                    "confirm" => DialogKind::Confirm,
                    "input" => DialogKind::Input,
                    _ => DialogKind::Editor,
                };
                if self.pending_dialog.is_some() {
                    let request_id = value.get("id").and_then(Value::as_str).unwrap_or_default();
                    self.send_value(
                        index,
                        json!({
                            "type":"extension_ui_response",
                            "id":request_id,
                            "cancelled":true
                        }),
                    );
                    self.harnesses[index].messages.push(Message::notice(
                        "An extension dialog was cancelled because another harness already needs input.",
                    ));
                    return;
                }
                self.harnesses[index].has_unread_completion = false;
                self.harnesses[index].attention_required = true;
                self.refresh_harness_order(index);
                self.selected_project = Some(self.harnesses[index].project_id);
                self.show_cached_models(self.harnesses[index].project_id);
                self.selected_harness = Some(self.harnesses[index].id);
                self.last_used_harness = Some(self.harnesses[index].id);
                self.adding_project = false;
                self.creating_harness = false;
                self.persist();
                let prefill = value
                    .get("prefill")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.extension_input
                    .update(cx, |input, cx| input.set_text(prefill, cx));
                self.pending_dialog = Some(PendingDialog {
                    harness_id: self.harnesses[index].id,
                    request_id: value
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    kind,
                    title: value
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("Pi needs input")
                        .to_string(),
                    message: value
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    options: value
                        .get("options")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                });
            }
            _ => {}
        }
    }
}
