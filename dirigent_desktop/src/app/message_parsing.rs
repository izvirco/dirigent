//! Converts Pi RPC and session data into conversation messages.

use super::*;

pub(super) fn parse_cached_draft_images(bytes: &[u8]) -> Result<Vec<AttachedImage>, String> {
    let images = serde_json::from_slice::<Vec<CachedDraftImage>>(bytes)
        .map_err(|error| format!("could not decode cached composer images: {error}"))?;
    images
        .into_iter()
        .map(|image| {
            let format = ImageFormat::from_mime_type(&image.mime_type).ok_or_else(|| {
                format!(
                    "cached composer image has unsupported type {}",
                    image.mime_type
                )
            })?;
            let bytes = BASE64
                .decode(image.data)
                .map_err(|error| format!("could not decode a cached composer image: {error}"))?;
            let source = format!("cached composer image {}", image.label);
            let normalized =
                normalize_for_harness(Arc::new(Image::from_bytes(format, bytes)), &source)?;
            Ok(AttachedImage {
                label: image.label,
                image: normalized,
            })
        })
        .collect()
}

pub(super) fn parse_available_model(value: &Value) -> Option<AvailableModel> {
    let thinking_map = value.get("thinkingLevelMap").and_then(Value::as_object);
    Some(AvailableModel {
        provider: value.get("provider")?.as_str()?.to_string(),
        id: value.get("id")?.as_str()?.to_string(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_else(|| value.get("id").and_then(Value::as_str).unwrap_or("model"))
            .to_string(),
        reasoning: value
            .get("reasoning")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        supports_xhigh: thinking_map.is_some_and(|map| map.contains_key("xhigh")),
        supports_max: thinking_map.is_some_and(|map| map.contains_key("max")),
    })
}

pub(super) fn parse_session_stats(value: &Value) -> Option<SessionStats> {
    let data = value.get("data")?;
    let tokens = data.get("tokens")?;
    let context_usage = data.get("contextUsage").and_then(|usage| {
        Some(ContextUsage {
            used_tokens: usage.get("tokens")?.as_u64()?,
            context_window: usage.get("contextWindow")?.as_u64()?,
        })
    });
    Some(SessionStats {
        input_tokens: tokens.get("input")?.as_u64()?,
        output_tokens: tokens.get("output")?.as_u64()?,
        cache_read_tokens: tokens.get("cacheRead")?.as_u64()?,
        cache_write_tokens: tokens.get("cacheWrite")?.as_u64()?,
        user_messages: data.get("userMessages")?.as_u64()?,
        assistant_messages: data.get("assistantMessages")?.as_u64()?,
        cost: data.get("cost")?.as_f64()?,
        context_usage,
    })
}

pub(super) fn parse_codex_usage(value: &Value) -> Option<CodexUsage> {
    const FIVE_HOURS_SECONDS: u64 = 5 * 60 * 60;
    const WEEK_SECONDS: u64 = 7 * 24 * 60 * 60;
    const DURATION_TOLERANCE_SECONDS: u64 = 60;

    let windows = value.get("windows")?.as_array()?;
    let mut usage = CodexUsage {
        five_hour: None,
        weekly: None,
        fetched_at: value.get("fetchedAt")?.as_u64()?,
    };
    for window in windows {
        let Some(duration) = window.get("durationSeconds").and_then(Value::as_u64) else {
            continue;
        };
        let Some(used_percent) = window.get("usedPercent").and_then(Value::as_f64) else {
            continue;
        };
        let parsed = CodexUsageWindow {
            used_percent,
            resets_at: window.get("resetsAt").and_then(Value::as_u64),
        };
        if duration.abs_diff(FIVE_HOURS_SECONDS) <= DURATION_TOLERANCE_SECONDS {
            usage.five_hour = Some(parsed);
        } else if duration.abs_diff(WEEK_SECONDS) <= DURATION_TOLERANCE_SECONDS {
            usage.weekly = Some(parsed);
        }
    }
    Some(usage)
}

pub(super) fn compact_json(value: &Value) -> String {
    truncate_output(&serde_json::to_string(value).unwrap_or_else(|_| "{}".into()))
}

pub(super) fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn tool_label(name: &str, args: &Value) -> String {
    let argument = match name {
        "bash" => args.get("command").and_then(Value::as_str),
        "read" | "write" | "edit" => args.get("path").and_then(Value::as_str),
        "find" | "grep" => args.get("pattern").and_then(Value::as_str),
        _ => None,
    };
    match argument {
        Some(argument) if name == "bash" => one_line(argument),
        Some(argument) => format!("{name} {}", one_line(argument)),
        _ if args.is_null() => name.to_string(),
        _ => format!("{name} {}", one_line(&compact_json(args))),
    }
}

pub(super) fn tool_expanded(_name: &str) -> bool {
    false
}

fn change_stats(diff: &str) -> (usize, usize) {
    diff.lines().fold((0, 0), |(additions, deletions), line| {
        match line.as_bytes().first() {
            Some(b'+') => (additions + 1, deletions),
            Some(b'-') => (additions, deletions + 1),
            _ => (additions, deletions),
        }
    })
}

fn tool_call_change_stats(name: &str, args: &Value) -> Option<(usize, usize)> {
    (name == "write").then(|| {
        (
            args.get("content")
                .and_then(Value::as_str)
                .map(|content| content.split_inclusive('\n').count())
                .unwrap_or_default(),
            0,
        )
    })
}

pub(super) fn add_tool_change_summary(message: &mut Message, name: &str, result: Option<&Value>) {
    let result_stats = (name == "edit")
        .then(|| {
            result?
                .pointer("/details/diff")
                .and_then(Value::as_str)
                .map(change_stats)
        })
        .flatten();
    message.tool_change_stats = result_stats.or(message.tool_change_stats);
    if let Some((additions, deletions)) = message.tool_change_stats {
        message.append_text(&format!(" +{additions} -{deletions}"));
    }
}

pub(super) fn write_detail(args: &Value) -> Option<String> {
    let content = args.get("content").and_then(Value::as_str)?;
    if content.is_empty() {
        return None;
    }
    let mut detail = String::with_capacity(content.len() + content.lines().count() * 2);
    for line in content.split_inclusive('\n') {
        detail.push_str("+ ");
        detail.push_str(line);
    }
    Some(truncate_output(&detail))
}

pub(super) fn normalize_diff_spacing(diff: &str) -> String {
    let mut detail = String::with_capacity(diff.len() + diff.lines().count());
    for line in diff.split_inclusive('\n') {
        if matches!(line.as_bytes().first(), Some(b'+' | b'-' | b' ')) {
            detail.push_str(&line[..1]);
            detail.push(' ');
            detail.push_str(&line[1..]);
        } else {
            detail.push_str(line);
        }
    }
    detail
}

fn message_timestamp_ms(value: &Value) -> Option<u64> {
    value.get("timestamp").and_then(Value::as_u64)
}

pub(super) fn tool_message(
    name: &str,
    args: &Value,
    tool_call_id: Option<String>,
    running: bool,
) -> Message {
    let mut message = Message::tool(
        tool_label(name, args),
        tool_call_id,
        running,
        tool_expanded(name),
    );
    message.tool_name = Some(name.to_string());
    if name == "write" {
        message.set_detail(write_detail(args));
    }
    message.tool_change_stats = tool_call_change_stats(name, args);
    message
}

pub(super) fn truncate_output(value: &str) -> String {
    const LIMIT: usize = 4_000;
    if value.len() <= LIMIT {
        value.to_string()
    } else {
        let mut end = LIMIT;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n… output truncated by Dirigent", &value[..end])
    }
}

/// Extracts concise display output, preferring edit diffs over generic result text.
pub(super) fn tool_result_detail(name: &str, result: &Value, is_error: bool) -> Option<String> {
    if name == "edit"
        && !is_error
        && let Some(diff) = result.pointer("/details/diff").and_then(Value::as_str)
    {
        return Some(truncate_output(&normalize_diff_spacing(diff)));
    }

    result
        .get("content")
        .map(content_text)
        .filter(|text| !text.is_empty())
        .map(|text| truncate_output(&text))
}

pub(super) fn content_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn content_images(value: &Value) -> Vec<Arc<Image>> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("image") {
                return None;
            }
            let data = block.get("data").and_then(Value::as_str)?;
            let format = ImageFormat::from_mime_type(
                block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("image/png"),
            )?;
            let bytes = BASE64.decode(data).ok()?;
            (!bytes.is_empty()).then(|| Arc::new(Image::from_bytes(format, bytes)))
        })
        .collect()
}

/// Coalesces adjacent blocks of the same role and entry into one rendered message.
pub(super) fn push_assistant_block(
    messages: &mut Vec<Message>,
    role: MessageRole,
    text: &str,
    entry_id: Option<&str>,
) {
    if text.is_empty() {
        return;
    }
    if let Some(message) = messages
        .last_mut()
        .filter(|message| message.role == role && message.entry_id.as_deref() == entry_id)
    {
        if !message.text.is_empty() {
            message.append_text("\n");
        }
        message.append_text(text);
        return;
    }
    messages.push(Message::new(role, text).with_entry_id(entry_id));
}

/// Reconstructs the active root-to-leaf path from Pi's flat session-tree entries.
pub(super) fn entries_through_leaf(values: &[Value], leaf_id: Option<&str>) -> Vec<Value> {
    let by_id = values
        .iter()
        .filter_map(|entry| Some((entry.get("id")?.as_str()?, entry)))
        .collect::<HashMap<_, _>>();
    let mut entries = Vec::new();
    let mut current_id = leaf_id;
    let mut visited = HashSet::new();
    while let Some(id) = current_id {
        if !visited.insert(id) {
            break;
        }
        let Some(entry) = by_id.get(id).copied() else {
            break;
        };
        entries.push(entry.clone());
        current_id = entry.get("parentId").and_then(Value::as_str);
    }
    entries.reverse();
    entries
}

/// Parses only the active branch and annotates messages with settings effective at each entry.
pub(super) fn parse_entries(values: &[Value], leaf_id: Option<&str>) -> Vec<Message> {
    let entries_by_id = values
        .iter()
        .filter_map(|entry| Some((entry.get("id")?.as_str()?, entry)))
        .collect::<HashMap<_, _>>();
    let mut active_entries = Vec::new();
    let mut visited = HashSet::new();
    let mut current_id = leaf_id;
    while let Some(id) = current_id {
        if !visited.insert(id) {
            break;
        }
        let Some(entry) = entries_by_id.get(id).copied() else {
            break;
        };
        active_entries.push(entry);
        current_id = entry.get("parentId").and_then(Value::as_str);
    }
    active_entries.reverse();

    let mut messages = Vec::new();
    let mut current_model = None;
    let mut current_thinking_level = None;
    for entry in active_entries {
        match entry.get("type").and_then(Value::as_str) {
            Some("model_change") => {
                current_model = entry
                    .get("provider")
                    .and_then(Value::as_str)
                    .zip(entry.get("modelId").and_then(Value::as_str))
                    .map(|(provider, model)| format!("{provider}/{model}"));
            }
            Some("thinking_level_change") => {
                current_thinking_level = entry
                    .get("thinkingLevel")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            Some("message") => {
                if let Some(message) = entry.get("message") {
                    let first_new_message = messages.len();
                    push_parsed_message(
                        &mut messages,
                        message,
                        entry.get("id").and_then(Value::as_str),
                    );
                    for message in &mut messages[first_new_message..] {
                        if message.model.is_none() {
                            message.model = current_model.clone();
                        }
                        message.thinking_level = current_thinking_level.clone();
                    }
                }
            }
            Some("compaction") => {
                let summary = entry
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(truncate_output);
                let mut message = Message::compaction(None, summary.as_deref(), false)
                    .with_entry_id(entry.get("id").and_then(Value::as_str));
                message.set_turn_settings(current_model.clone(), current_thinking_level.clone());
                messages.push(message);
            }
            _ => {}
        }
    }
    messages
}

pub(super) fn parse_messages(values: &[Value]) -> Vec<Message> {
    let mut messages = Vec::new();
    for value in values {
        push_parsed_message(&mut messages, value, None);
    }
    messages
}

pub(super) fn rpc_string_array(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// Re-keys expansion state once an optimistic user message receives its persisted entry ID.
pub(super) fn reconcile_work_group_expansion(
    previous: &[Message],
    canonical: &[Message],
    expansion: &mut HashMap<String, bool>,
) -> bool {
    let previous_users = previous
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == MessageRole::User)
        .collect::<Vec<_>>();
    let canonical_users = canonical
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .collect::<Vec<_>>();
    let mut changed = false;
    for ((previous_index, previous), canonical) in previous_users.into_iter().zip(canonical_users) {
        let pending_id = format!("pending:{previous_index}");
        let Some(expanded) = expansion.remove(&pending_id) else {
            continue;
        };
        let Some(entry_id) = canonical
            .entry_id
            .as_deref()
            .filter(|_| canonical.text == previous.text)
        else {
            expansion.insert(pending_id, expanded);
            continue;
        };
        expansion.insert(format!("entry:{entry_id}"), expanded);
        changed = true;
    }
    changed
}

pub(super) fn reconcile_queued_messages(
    messages: &mut Vec<Message>,
    steering: &[String],
) -> Vec<Message> {
    // Match from the end so duplicate steering prompts retain the newest optimistic messages,
    // which is the ordering represented by Pi's queue.
    let mut pending = steering
        .iter()
        .fold(HashMap::new(), |mut pending, message| {
            *pending.entry(message.as_str()).or_insert(0_usize) += 1;
            pending
        });
    let mut retained = vec![false; messages.len()];
    for (index, message) in messages.iter().enumerate().rev() {
        let count = pending.entry(message.text.as_str()).or_default();
        if *count > 0 {
            *count -= 1;
            retained[index] = true;
        }
    }

    let mut still_queued = Vec::new();
    let mut accepted = Vec::new();
    for (index, mut message) in std::mem::take(messages).into_iter().enumerate() {
        if retained[index] {
            still_queued.push(message);
        } else {
            message.queued = false;
            accepted.push(message);
        }
    }
    *messages = still_queued;
    accepted
}

pub(super) fn assistant_failure(value: &Value) -> Option<String> {
    if value.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let error = value
        .get("errorMessage")
        .and_then(Value::as_str)
        .filter(|error| !error.trim().is_empty());
    match value.get("stopReason").and_then(Value::as_str) {
        Some("error") => Some(error.unwrap_or("Pi's model request failed.").to_string()),
        Some("length") => Some(
            "The model reached its maximum output token limit; the response may be incomplete."
                .into(),
        ),
        Some("aborted") => Some(
            error
                .filter(|error| *error != "Request was aborted")
                .unwrap_or("Operation aborted.")
                .to_string(),
        ),
        _ => None,
    }
}

/// Appends one protocol message, joining tool results to earlier calls by tool-call ID.
pub(super) fn push_parsed_message(
    messages: &mut Vec<Message>,
    value: &Value,
    entry_id: Option<&str>,
) {
    let first_new_message = messages.len();
    if value.get("role").and_then(Value::as_str) == Some("assistant") {
        if let Some(blocks) = value.get("content").and_then(Value::as_array) {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => push_assistant_block(
                        messages,
                        MessageRole::Assistant,
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        entry_id,
                    ),
                    Some("thinking") => push_assistant_block(
                        messages,
                        MessageRole::Thinking,
                        block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        entry_id,
                    ),
                    Some("toolCall") => {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let mut message = tool_message(
                            name,
                            block.get("arguments").unwrap_or(&Value::Null),
                            block.get("id").and_then(Value::as_str).map(str::to_string),
                            false,
                        )
                        .with_entry_id(entry_id);
                        message.set_tool_started_timestamp(message_timestamp_ms(value));
                        messages.push(message);
                    }
                    _ => {}
                }
            }
        } else if let Some(message) = parse_message(value) {
            messages.push(message.with_entry_id(entry_id));
        }
        if let Some(error) = assistant_failure(value) {
            messages.push(Message::error(error).with_entry_id(entry_id));
        }
    } else if value.get("role").and_then(Value::as_str) == Some("toolResult") {
        let tool_call_id = value.get("toolCallId").and_then(Value::as_str);
        if let Some(message) = messages
            .iter_mut()
            .rev()
            .find(|message| message.tool_call_id.as_deref() == tool_call_id)
        {
            let name = value
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let is_error = value.get("isError").and_then(Value::as_bool) == Some(true);
            let detail = tool_result_detail(name, value, is_error);
            if detail.is_some() && (name != "write" || is_error || message.detail.is_none()) {
                message.set_detail(detail);
            }
            if !is_error {
                add_tool_change_summary(message, name, Some(value));
            }
            message.finish_tool(is_error, message_timestamp_ms(value));
        } else if let Some(message) = parse_message(value) {
            messages.push(message.with_entry_id(entry_id));
        }
    } else if let Some(message) = parse_message(value) {
        messages.push(message.with_entry_id(entry_id));
    }

    if value.get("role").and_then(Value::as_str) == Some("assistant") {
        let model = value
            .get("provider")
            .and_then(Value::as_str)
            .zip(value.get("model").and_then(Value::as_str))
            .map(|(provider, model)| format!("{provider}/{model}"));
        if model.is_some() {
            for message in &mut messages[first_new_message..] {
                message.model = model.clone();
            }
        }
    }
}

pub(super) fn parse_message(value: &Value) -> Option<Message> {
    match value.get("role")?.as_str()? {
        "user" => {
            let content = value.get("content")?;
            Some(Message::user_with_images(
                content_text(content),
                content_images(content),
            ))
        }
        "assistant" => {
            let text = content_text(value.get("content")?);
            (!text.is_empty()).then(|| {
                let mut message = Message::new(MessageRole::Assistant, text);
                message.model = value
                    .get("provider")
                    .and_then(Value::as_str)
                    .zip(value.get("model").and_then(Value::as_str))
                    .map(|(provider, model)| format!("{provider}/{model}"));
                message
            })
        }
        "toolResult" => {
            let name = value
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let is_error = value.get("isError").and_then(Value::as_bool) == Some(true);
            let mut message = Message::tool(
                name,
                value
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                false,
                tool_expanded(name),
            );
            message.tool_name = Some(name.to_string());
            message.set_detail(tool_result_detail(name, value, is_error));
            if !is_error {
                add_tool_change_summary(&mut message, name, Some(value));
            }
            message.finish_tool(is_error, message_timestamp_ms(value));
            Some(message)
        }
        "bashExecution" => {
            let mut message = Message::tool(
                value
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("bash"),
                None,
                false,
                false,
            );
            message.tool_name = Some("bash".into());
            message.set_detail(
                value
                    .get("output")
                    .and_then(Value::as_str)
                    .map(truncate_output),
            );
            Some(message)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_stats() {
        let value = json!({
            "data": {
                "userMessages": 5,
                "assistantMessages": 12,
                "tokens": {
                    "input": 10_000,
                    "output": 2_000,
                    "cacheRead": 30_000,
                    "cacheWrite": 5_000
                },
                "cost": 123.12,
                "contextUsage": {
                    "tokens": 39_000,
                    "contextWindow": 272_000,
                    "percent": 14.3
                }
            }
        });

        assert_eq!(
            parse_session_stats(&value),
            Some(SessionStats {
                input_tokens: 10_000,
                output_tokens: 2_000,
                cache_read_tokens: 30_000,
                cache_write_tokens: 5_000,
                user_messages: 5,
                assistant_messages: 12,
                cost: 123.12,
                context_usage: Some(ContextUsage {
                    used_tokens: 39_000,
                    context_window: 272_000,
                }),
            })
        );
    }

    #[test]
    fn parses_stats_when_context_usage_is_unknown() {
        let value = json!({
            "data": {
                "userMessages": 1,
                "assistantMessages": 2,
                "tokens": {
                    "input": 100,
                    "output": 20,
                    "cacheRead": 0,
                    "cacheWrite": 0
                },
                "cost": 0.01,
                "contextUsage": {
                    "tokens": null,
                    "contextWindow": 200_000,
                    "percent": null
                }
            }
        });

        assert_eq!(
            parse_session_stats(&value)
                .expect("stats should parse")
                .context_usage,
            None
        );
    }
}
