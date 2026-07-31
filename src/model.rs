use std::{
    ops::Range,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{Image, ScrollHandle, SharedString};
use serde::{Deserialize, Serialize};

use crate::theme::{green, red};

use crate::{
    markdown::{MarkdownDocument, parse_markdown},
    rpc::PiProcess,
    text_input::AttachedImage,
};

pub(crate) type Id = u64;

pub(crate) struct Project {
    pub(crate) id: Id,
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) workspace_root: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum WorkspaceBackend {
    Jj,
    Git,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum WorkspaceState {
    Provisioning,
    Ready,
    Failed(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ManagedWorkspace {
    pub(crate) id: String,
    pub(crate) project_id: Id,
    pub(crate) backend: WorkspaceBackend,
    pub(crate) root: PathBuf,
    pub(crate) working_directory: PathBuf,
    pub(crate) source_repository: PathBuf,
    pub(crate) source_id: String,
    pub(crate) source_label: String,
    pub(crate) source_revision: String,
    pub(crate) jj_parent_revisions: Vec<String>,
    pub(crate) git_branch: Option<String>,
    pub(crate) state: WorkspaceState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HarnessStatus {
    Starting,
    Idle,
    Working,
    Failed,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MessageRole {
    User,
    Assistant,
    Thinking,
    Tool,
    Notice,
    Error,
}

pub(crate) struct Message {
    pub(crate) role: MessageRole,
    pub(crate) entry_id: Option<String>,
    pub(crate) text: String,
    pub(crate) queued: bool,
    pub(crate) display_text: SharedString,
    pub(crate) copy_text: SharedString,
    pub(crate) markdown: Option<MarkdownDocument>,
    pub(crate) detail: Option<String>,
    pub(crate) display_detail: Option<SharedString>,
    pub(crate) detail_colors: Vec<(Range<usize>, u32)>,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) running: bool,
    pub(crate) expanded: bool,
    pub(crate) images: Vec<Arc<Image>>,
    pub(crate) detail_scroll: ScrollHandle,
}

impl Message {
    pub(crate) fn new(role: MessageRole, text: impl Into<String>) -> Self {
        let text = text.into();
        let mut message = Self {
            role,
            entry_id: None,
            text,
            queued: false,
            display_text: SharedString::default(),
            copy_text: SharedString::default(),
            markdown: None,
            detail: None,
            display_detail: None,
            detail_colors: Vec::new(),
            tool_call_id: None,
            running: false,
            expanded: false,
            images: Vec::new(),
            detail_scroll: ScrollHandle::new(),
        };
        message.refresh_text_cache();
        message
    }

    pub(crate) fn tool(
        text: impl Into<String>,
        tool_call_id: Option<String>,
        running: bool,
        expanded: bool,
    ) -> Self {
        let mut message = Self::new(MessageRole::Tool, text);
        message.tool_call_id = tool_call_id;
        message.running = running;
        message.expanded = expanded;
        message
    }

    pub(crate) fn user_with_images(text: String, images: Vec<Arc<Image>>) -> Self {
        let mut message = Self::new(MessageRole::User, text);
        message.images = images;
        message
    }

    pub(crate) fn compaction(reason: Option<&str>, summary: Option<&str>, running: bool) -> Self {
        let text = reason.map_or_else(
            || "compact context".to_string(),
            |reason| format!("compact context · {reason}"),
        );
        let mut message = Self::tool(text, None, running, false);
        message.set_detail(summary.map(str::to_string));
        message
    }

    pub(crate) fn with_entry_id(mut self, entry_id: Option<&str>) -> Self {
        self.entry_id = entry_id.map(str::to_string);
        self
    }

    pub(crate) fn is_compaction(&self) -> bool {
        self.role == MessageRole::Tool
            && self
                .text
                .split_whitespace()
                .next()
                .is_some_and(|tool| tool == "compact")
    }

    pub(crate) fn notice(text: impl Into<String>) -> Self {
        Self::new(MessageRole::Notice, text)
    }

    pub(crate) fn error(text: impl Into<String>) -> Self {
        Self::new(MessageRole::Error, text)
    }

    pub(crate) fn append_text(&mut self, text: &str) {
        self.text.push_str(text);
        self.refresh_text_cache();
    }

    pub(crate) fn set_running(&mut self, running: bool) {
        self.running = running;
        if running {
            self.markdown = None;
        } else {
            self.refresh_markdown_cache();
        }
    }

    pub(crate) fn set_detail(&mut self, detail: Option<String>) {
        self.detail = detail;
        self.refresh_detail_cache();
        self.refresh_copy_cache();
    }

    pub(crate) fn refresh_text_cache(&mut self) {
        self.display_text = if self.role == MessageRole::Thinking {
            self.text
                .lines()
                .filter_map(|line| {
                    let line = line.trim();
                    if line.is_empty() {
                        return None;
                    }
                    Some(
                        line.strip_prefix("**")
                            .and_then(|line| line.strip_suffix("**"))
                            .unwrap_or(line),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
                .into()
        } else {
            self.text.clone().into()
        };
        self.refresh_markdown_cache();
        self.refresh_copy_cache();
    }

    fn refresh_markdown_cache(&mut self) {
        self.markdown = (self.role == MessageRole::Assistant && !self.running)
            .then(|| parse_markdown(&self.text));
    }

    pub(crate) fn refresh_theme_colors(&mut self) {
        self.refresh_detail_cache();
    }

    fn refresh_detail_cache(&mut self) {
        self.display_detail = self.detail.clone().map(Into::into);
        self.detail_colors.clear();
        let tool = self
            .text
            .split_once(' ')
            .map_or(self.text.as_str(), |(tool, _)| tool);
        if !matches!(tool, "edit" | "write") {
            return;
        }
        let Some(detail) = self.detail.as_deref() else {
            return;
        };
        let lines = detail
            .split_inclusive('\n')
            .scan(0, |offset, line| {
                let start = *offset;
                *offset += line.len();
                Some((start, line))
            })
            .collect::<Vec<_>>();
        let mut index = 0;
        while index < lines.len() {
            let (offset, line) = lines[index];
            let kind = line.as_bytes().first().copied();
            let is_single_line_edit = tool == "edit"
                && kind == Some(b'-')
                && lines
                    .get(index + 1)
                    .is_some_and(|(_, line)| line.starts_with('+'))
                && (index == 0 || !lines[index - 1].1.starts_with('-'))
                && lines
                    .get(index + 2)
                    .is_none_or(|(_, line)| !line.starts_with('+'));

            if is_single_line_edit {
                let (added_offset, added_line) = lines[index + 1];
                add_inline_diff_colors(
                    &mut self.detail_colors,
                    offset,
                    line,
                    added_offset,
                    added_line,
                );
                index += 2;
                continue;
            }

            let color = match kind {
                Some(b'+') => green(),
                Some(b'-') => red(),
                _ => crate::theme::detail_text(),
            };
            self.detail_colors
                .push((offset..offset + line.len(), color));
            index += 1;
        }
    }

    fn refresh_copy_cache(&mut self) {
        self.copy_text = match self.detail.as_deref() {
            Some(detail) if !detail.is_empty() => format!("{}\n{detail}", self.text).into(),
            _ => self.text.clone().into(),
        };
    }
}

fn add_inline_diff_colors(
    colors: &mut Vec<(Range<usize>, u32)>,
    removed_offset: usize,
    removed_line: &str,
    added_offset: usize,
    added_line: &str,
) {
    colors.push((removed_offset..removed_offset + 1, red()));
    colors.push((added_offset..added_offset + 1, green()));

    let removed_start = diff_source_start(removed_line);
    let added_start = diff_source_start(added_line);
    let removed = removed_line[removed_start..].trim_end_matches(['\r', '\n']);
    let added = added_line[added_start..].trim_end_matches(['\r', '\n']);
    let (removed_changes, added_changes) = token_diff_ranges(removed, added);

    colors.extend(removed_changes.into_iter().map(|range| {
        (
            removed_offset + removed_start + range.start
                ..removed_offset + removed_start + range.end,
            red(),
        )
    }));
    colors.extend(added_changes.into_iter().map(|range| {
        (
            added_offset + added_start + range.start..added_offset + added_start + range.end,
            green(),
        )
    }));
}

fn line_tokens(text: &str) -> Vec<Range<usize>> {
    let mut tokens = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let character = text[start..]
            .chars()
            .next()
            .expect("valid character offset");
        let word = character.is_alphanumeric() || character == '_';
        let whitespace = character.is_whitespace();
        let mut end = start + character.len_utf8();
        if word || whitespace {
            while end < text.len() {
                let next = text[end..].chars().next().expect("valid character offset");
                if (word && (next.is_alphanumeric() || next == '_'))
                    || (whitespace && next.is_whitespace())
                {
                    end += next.len_utf8();
                } else {
                    break;
                }
            }
        }
        tokens.push(start..end);
        start = end;
    }
    tokens
}

fn token_diff_ranges(removed: &str, added: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    const MAX_LCS_CELLS: usize = 65_536;

    let removed_tokens = line_tokens(removed);
    let added_tokens = line_tokens(added);
    let rows = removed_tokens.len() + 1;
    let columns = added_tokens.len() + 1;
    if rows.saturating_mul(columns) > MAX_LCS_CELLS {
        return token_edge_diff_ranges(removed, added, &removed_tokens, &added_tokens);
    }

    let mut lengths = vec![0_usize; rows * columns];
    for removed_index in (0..removed_tokens.len()).rev() {
        for added_index in (0..added_tokens.len()).rev() {
            let index = removed_index * columns + added_index;
            lengths[index] = if removed[removed_tokens[removed_index].clone()]
                == added[added_tokens[added_index].clone()]
            {
                1 + lengths[(removed_index + 1) * columns + added_index + 1]
            } else {
                lengths[(removed_index + 1) * columns + added_index]
                    .max(lengths[removed_index * columns + added_index + 1])
            };
        }
    }

    let mut removed_matches = vec![false; removed_tokens.len()];
    let mut added_matches = vec![false; added_tokens.len()];
    let (mut removed_index, mut added_index) = (0, 0);
    while removed_index < removed_tokens.len() && added_index < added_tokens.len() {
        if removed[removed_tokens[removed_index].clone()]
            == added[added_tokens[added_index].clone()]
        {
            removed_matches[removed_index] = true;
            added_matches[added_index] = true;
            removed_index += 1;
            added_index += 1;
        } else if lengths[(removed_index + 1) * columns + added_index]
            > lengths[removed_index * columns + added_index + 1]
        {
            removed_index += 1;
        } else {
            added_index += 1;
        }
    }

    (
        unmatched_token_ranges(removed, &removed_tokens, &removed_matches),
        unmatched_token_ranges(added, &added_tokens, &added_matches),
    )
}

fn unmatched_token_ranges(
    text: &str,
    tokens: &[Range<usize>],
    matches: &[bool],
) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        if matches[index] {
            index += 1;
            continue;
        }
        let start = tokens[index].start;
        while index + 1 < tokens.len() && !matches[index + 1] {
            index += 1;
        }
        if let Some(range) = trim_whitespace(text, start..tokens[index].end) {
            ranges.push(range);
        }
        index += 1;
    }
    ranges
}

fn token_edge_diff_ranges(
    removed: &str,
    added: &str,
    removed_tokens: &[Range<usize>],
    added_tokens: &[Range<usize>],
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let mut prefix = 0;
    while prefix < removed_tokens.len()
        && prefix < added_tokens.len()
        && removed[removed_tokens[prefix].clone()] == added[added_tokens[prefix].clone()]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < removed_tokens.len() - prefix
        && suffix < added_tokens.len() - prefix
        && removed[removed_tokens[removed_tokens.len() - suffix - 1].clone()]
            == added[added_tokens[added_tokens.len() - suffix - 1].clone()]
    {
        suffix += 1;
    }

    let changed_range = |text: &str, tokens: &[Range<usize>]| {
        if prefix + suffix == tokens.len() {
            return Vec::new();
        }
        let start = tokens[prefix].start;
        let end = tokens[tokens.len() - suffix - 1].end;
        trim_whitespace(text, start..end).into_iter().collect()
    };
    (
        changed_range(removed, removed_tokens),
        changed_range(added, added_tokens),
    )
}

fn trim_whitespace(text: &str, mut range: Range<usize>) -> Option<Range<usize>> {
    while range.start < range.end {
        let character = text[range.clone()].chars().next()?;
        if !character.is_whitespace() {
            break;
        }
        range.start += character.len_utf8();
    }
    while range.start < range.end {
        let character = text[range.clone()].chars().next_back()?;
        if !character.is_whitespace() {
            break;
        }
        range.end -= character.len_utf8();
    }
    (!range.is_empty()).then_some(range)
}

fn diff_source_start(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut offset = 1;
    if bytes.get(offset) == Some(&b' ') {
        offset += 1;
    }
    let number_start = offset;
    while bytes.get(offset).is_some_and(u8::is_ascii_digit) {
        offset += 1;
    }
    if offset > number_start && bytes.get(offset) == Some(&b' ') {
        offset += 1;
    }
    offset
}

#[derive(Clone, Debug)]
pub(crate) struct RetryStatus {
    pub(crate) attempt: u64,
    pub(crate) max_attempts: u64,
    pub(crate) delay_ms: u64,
    pub(crate) error_message: String,
    pub(crate) waiting: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContextUsage {
    pub(crate) used_tokens: u64,
    pub(crate) context_window: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CodexUsageWindow {
    pub(crate) used_percent: f64,
    pub(crate) resets_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CodexUsage {
    pub(crate) five_hour: Option<CodexUsageWindow>,
    pub(crate) weekly: Option<CodexUsageWindow>,
    pub(crate) fetched_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HarnessTitleState {
    Initial,
    Generated,
    Manual,
}

pub(crate) struct Harness {
    pub(crate) id: Id,
    pub(crate) project_id: Id,
    pub(crate) title: String,
    title_state: HarnessTitleState,
    pub(crate) session_file: Option<PathBuf>,
    pub(crate) status: HarnessStatus,
    pub(crate) has_unread_completion: bool,
    pub(crate) attention_required: bool,
    pub(crate) archived: bool,
    pub(crate) sidebar_order: u64,
    pub(crate) run_started_at: Option<Instant>,
    pub(crate) last_run_duration: Option<Duration>,
    pub(crate) messages: Vec<Message>,
    pub(crate) model: Option<String>,
    pub(crate) thinking_level: Option<String>,
    pub(crate) composer_draft: String,
    pub(crate) composer_draft_images: Vec<AttachedImage>,
    pub(crate) context_usage: Option<ContextUsage>,
    pub(crate) queued_messages: Vec<Message>,
    pub(crate) steering_queue: Vec<String>,
    pub(crate) follow_up_queue: Vec<String>,
    pub(crate) retry_status: Option<RetryStatus>,
    pub(crate) error: Option<String>,
    pub(crate) process: Option<PiProcess>,
    pub(crate) process_generation: u64,
    pub(crate) loaded_messages: bool,
    pub(crate) cached_entries: Option<Vec<serde_json::Value>>,
    pub(crate) cached_leaf_id: Option<String>,
    pub(crate) startup_settings_pending: bool,
    pub(crate) pending_initial_prompt: Option<(String, Vec<AttachedImage>)>,
    pub(crate) nix_enabled: bool,
    pub(crate) nix_restart_pending: bool,
    pub(crate) workspace_id: Option<String>,
}

impl Harness {
    pub(crate) fn new(id: Id, project_id: Id, title: String, sidebar_order: u64) -> Self {
        Self {
            id,
            project_id,
            title,
            title_state: HarnessTitleState::Initial,
            session_file: None,
            status: HarnessStatus::Starting,
            has_unread_completion: false,
            attention_required: false,
            archived: false,
            sidebar_order,
            run_started_at: None,
            last_run_duration: None,
            messages: Vec::new(),
            model: None,
            thinking_level: None,
            composer_draft: String::new(),
            composer_draft_images: Vec::new(),
            context_usage: None,
            error: None,
            process: None,
            process_generation: 0,
            loaded_messages: true,
            cached_entries: None,
            cached_leaf_id: None,
            startup_settings_pending: true,
            pending_initial_prompt: None,
            queued_messages: Vec::new(),
            steering_queue: Vec::new(),
            follow_up_queue: Vec::new(),
            retry_status: None,
            nix_enabled: false,
            nix_restart_pending: false,
            workspace_id: None,
        }
    }

    pub(crate) fn set_manual_title(&mut self, title: String) {
        self.title = title;
        self.title_state = HarnessTitleState::Manual;
    }

    pub(crate) fn set_derived_title(&mut self) {
        self.title_state = HarnessTitleState::Generated;
    }

    pub(crate) fn apply_generated_title(&mut self, title: String) -> bool {
        if self.title_state != HarnessTitleState::Initial {
            return false;
        }
        self.title = title;
        self.title_state = HarnessTitleState::Generated;
        true
    }

    pub(crate) fn is_in_workpool(&self) -> bool {
        !self.archived
            && !self.attention_required
            && !self.has_unread_completion
            && self.run_started_at.is_some()
    }

    pub(crate) fn is_in_inbox(&self) -> bool {
        !self.archived && !self.is_in_workpool()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn restored(
        id: Id,
        project_id: Id,
        title: String,
        session_file: Option<PathBuf>,
        nix_enabled: bool,
        workspace_id: Option<String>,
        archived: bool,
        sidebar_order: u64,
    ) -> Self {
        Self {
            id,
            project_id,
            title,
            title_state: HarnessTitleState::Manual,
            session_file,
            status: HarnessStatus::Idle,
            has_unread_completion: false,
            attention_required: false,
            archived,
            sidebar_order,
            run_started_at: None,
            last_run_duration: None,
            messages: Vec::new(),
            model: None,
            thinking_level: None,
            composer_draft: String::new(),
            composer_draft_images: Vec::new(),
            context_usage: None,
            error: None,
            process: None,
            process_generation: 0,
            loaded_messages: false,
            cached_entries: None,
            cached_leaf_id: None,
            startup_settings_pending: false,
            pending_initial_prompt: None,
            queued_messages: Vec::new(),
            steering_queue: Vec::new(),
            follow_up_queue: Vec::new(),
            retry_status: None,
            nix_enabled,
            nix_restart_pending: false,
            workspace_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Harness, Message, MessageRole};
    use crate::theme::{green, red};
    use std::time::Instant;

    #[test]
    fn caches_normalized_thinking_text() {
        let message = Message::new(
            MessageRole::Thinking,
            "\n**Planning**\n\nCheck the implementation.\n**Done**",
        );

        assert_eq!(
            message.display_text.as_ref(),
            "Planning\nCheck the implementation.\nDone"
        );
    }

    #[test]
    fn parses_assistant_markdown_only_after_streaming_finishes() {
        let mut message = Message::new(MessageRole::Assistant, "**partial");
        message.set_running(true);
        message.append_text(" answer**");

        assert!(message.markdown.is_none());
        message.set_running(false);

        let markdown = message
            .markdown
            .as_ref()
            .expect("markdown should be cached");
        let crate::markdown::MarkdownBlock::Paragraph(text) = &markdown.blocks[0] else {
            panic!("expected a paragraph")
        };
        assert_eq!(text.text, "partial answer");
        assert!(text.spans[0].style.strong);
        assert_eq!(message.copy_text.as_ref(), "**partial answer**");
    }

    #[test]
    fn refreshes_copy_text_and_diff_highlights() {
        let mut message = Message::tool("write src/main.rs", None, false, true);
        message.set_detail(Some(" context\n-old\n+new".into()));

        assert_eq!(
            message.copy_text.as_ref(),
            "write src/main.rs\n context\n-old\n+new"
        );
        assert_eq!(
            message.detail_colors,
            vec![
                (0..9, crate::theme::detail_text()),
                (9..14, red()),
                (14..18, green())
            ]
        );
    }

    #[test]
    fn highlights_only_the_changed_parts_of_a_single_line_edit() {
        let detail = "  9 before\n- 10 let café = old_name;\n+ 24 let café = new_name;\n  25 after";
        let mut message = Message::tool("edit src/main.rs", None, false, true);
        message.set_detail(Some(detail.into()));

        let highlighted = |color| {
            message
                .detail_colors
                .iter()
                .filter(|(_, range_color)| *range_color == color)
                .map(|(range, _)| &detail[range.clone()])
                .collect::<Vec<_>>()
        };
        assert_eq!(highlighted(red()), vec!["-", "old_name"]);
        assert_eq!(highlighted(green()), vec!["+", "new_name"]);
    }

    #[test]
    fn highlights_inserted_tokens_without_borrowing_from_the_next_identifier() {
        let detail = "- 8 theme::{ACCENT, BORDER, MUTED}\n+ 9 theme::{ACCENT, BLUE, BORDER, MUTED}";
        let mut message = Message::tool("edit src/theme.rs", None, false, true);
        message.set_detail(Some(detail.into()));

        let highlighted = |color| {
            message
                .detail_colors
                .iter()
                .filter(|(_, range_color)| *range_color == color)
                .map(|(range, _)| &detail[range.clone()])
                .collect::<Vec<_>>()
        };
        assert_eq!(highlighted(red()), vec!["-"]);
        assert_eq!(highlighted(green()), vec!["+", "BLUE,"]);
    }

    #[test]
    fn colors_whole_lines_for_multi_line_edits() {
        let detail = "-old one\n-old two\n+new one\n+new two";
        let mut message = Message::tool("edit src/main.rs", None, false, true);
        message.set_detail(Some(detail.into()));

        assert_eq!(
            message.detail_colors,
            vec![
                (0..9, red()),
                (9..18, red()),
                (18..27, green()),
                (27..35, green())
            ]
        );
    }

    #[test]
    fn generated_titles_do_not_overwrite_manual_edits() {
        let mut harness = Harness::new(1, 2, "original prompt".into(), 1);
        harness.set_manual_title("original prompt".into());

        assert!(!harness.apply_generated_title("Generated title".into()));
        assert_eq!(harness.title, "original prompt");
    }

    #[test]
    fn generated_titles_replace_untouched_initial_titles_once() {
        let mut harness = Harness::new(1, 2, "original prompt".into(), 1);

        assert!(harness.apply_generated_title("Generated title".into()));
        assert!(!harness.apply_generated_title("Later title".into()));
        assert_eq!(harness.title, "Generated title");
    }

    #[test]
    fn attention_moves_working_threads_to_the_inbox() {
        let mut harness = Harness::new(1, 2, "task".into(), 1);
        harness.run_started_at = Some(Instant::now());
        assert!(harness.is_in_workpool());

        harness.attention_required = true;
        assert!(harness.is_in_inbox());

        harness.attention_required = false;
        harness.archived = true;
        assert!(!harness.is_in_inbox());
        assert!(!harness.is_in_workpool());
    }
}
