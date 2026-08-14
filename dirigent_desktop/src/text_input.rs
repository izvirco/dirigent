//! Implements the editable GPUI text composer and image attachments.

use std::{ops::Range, path::PathBuf, sync::Arc};

use gpui::{
    App, AppContext as _, Bounds, ClipboardEntry, ClipboardItem, Context, CursorStyle, Element,
    ElementId, EventEmitter, FocusHandle, Focusable, GlobalElementId, HighlightStyle, Image,
    IntoElement, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Render, ScrollHandle, SharedString, StyledText, TextLayout, Window, div, fill,
    prelude::*, px, relative, rgba, size,
};

use crate::{
    image_attachment::{load_external_image, normalize_for_harness},
    theme::{accent, blue, border, faint, muted, rgb, surface, theme_text},
};

fn vertical_offset_to_reveal(
    current_offset: Pixels,
    max_offset: Pixels,
    viewport: Range<Pixels>,
    target: Range<Pixels>,
) -> Pixels {
    let offset = if target.start < viewport.start {
        current_offset + viewport.start - target.start
    } else if target.end > viewport.end {
        current_offset + viewport.end - target.end
    } else {
        current_offset
    };
    offset.clamp(-max_offset, px(0.0))
}

fn clipboard_path(value: &str) -> Option<PathBuf> {
    let path = if value.starts_with("file:") {
        url::Url::parse(value).ok()?.to_file_path().ok()?
    } else {
        PathBuf::from(value)
    };
    (path.is_absolute() && path.exists()).then_some(path)
}

/// Parses common `text/uri-list` and copied-file clipboard representations.
fn clipboard_text_paths(text: &str) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('#')
            || (paths.is_empty() && matches!(line, "copy" | "cut"))
        {
            continue;
        }
        paths.push(clipboard_path(line)?);
    }
    (!paths.is_empty()).then_some(paths)
}

fn native_clipboard_paths() -> Vec<PathBuf> {
    use clipboard_rs::{Clipboard as _, ClipboardContext};

    let clipboard = match ClipboardContext::new() {
        Ok(clipboard) => clipboard,
        Err(error) => {
            tracing::debug!(error = %error, "could not initialize native clipboard path fallback");
            return Vec::new();
        }
    };
    match clipboard.get_files() {
        Ok(files) => {
            let paths = files
                .iter()
                .filter_map(|value| clipboard_path(value))
                .collect::<Vec<_>>();
            tracing::debug!(
                reported_files = files.len(),
                valid_paths = paths.len(),
                "read native clipboard file list"
            );
            paths
        }
        Err(error) => {
            tracing::debug!(error = %error, "native clipboard did not contain a readable file list");
            Vec::new()
        }
    }
}

pub(crate) enum InputEvent {
    Submit,
    Focused,
    Escape,
    Changed,
    CompletionPrevious,
    CompletionNext,
    CompletionAccepted,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharacterClass {
    Word,
    Punctuation,
}

fn character_class(character: char) -> CharacterClass {
    if character.is_alphanumeric() || character == '_' {
        CharacterClass::Word
    } else {
        CharacterClass::Punctuation
    }
}

fn previous_word_boundary(text: &str, mut offset: usize) -> usize {
    while let Some((index, character)) = text[..offset].char_indices().next_back() {
        if !character.is_whitespace() {
            break;
        }
        offset = index;
    }

    let Some((_, character)) = text[..offset].char_indices().next_back() else {
        return 0;
    };
    let class = character_class(character);
    while let Some((index, character)) = text[..offset].char_indices().next_back() {
        if character.is_whitespace() || character_class(character) != class {
            break;
        }
        offset = index;
    }
    offset
}

fn word_range_at(text: &str, index: usize) -> Range<usize> {
    if text.is_empty() {
        return 0..0;
    }

    let index = index.min(text.len());
    let character_start = if index == text.len() {
        text.char_indices()
            .next_back()
            .map_or(0, |(start, _)| start)
    } else if text.is_char_boundary(index) {
        index
    } else {
        (0..index)
            .rev()
            .find(|offset| text.is_char_boundary(*offset))
            .unwrap_or(0)
    };
    let character = text[character_start..]
        .chars()
        .next()
        .expect("non-empty text has a character at a valid boundary");
    if character_class(character) != CharacterClass::Word {
        return character_start..character_start + character.len_utf8();
    }

    let start = text[..character_start]
        .char_indices()
        .rev()
        .take_while(|(_, candidate)| character_class(*candidate) == CharacterClass::Word)
        .last()
        .map_or(character_start, |(start, _)| start);
    let end = character_start
        + text[character_start..]
            .char_indices()
            .take_while(|(_, candidate)| character_class(*candidate) == CharacterClass::Word)
            .map(|(_, candidate)| candidate.len_utf8())
            .sum::<usize>();
    start..end
}

fn next_word_boundary(text: &str, mut offset: usize) -> usize {
    let starts_in_whitespace = text[offset..]
        .chars()
        .next()
        .is_some_and(char::is_whitespace);
    if starts_in_whitespace {
        while let Some(character) = text[offset..].chars().next() {
            if !character.is_whitespace() {
                break;
            }
            offset += character.len_utf8();
        }
        return offset;
    }

    let Some(character) = text[offset..].chars().next() else {
        return text.len();
    };
    let class = character_class(character);
    while let Some(character) = text[offset..].chars().next() {
        if character.is_whitespace() || character_class(character) != class {
            break;
        }
        offset += character.len_utf8();
    }
    while let Some(character) = text[offset..].chars().next() {
        if !character.is_whitespace() {
            break;
        }
        offset += character.len_utf8();
    }
    offset
}

#[derive(Clone)]
pub(crate) struct AttachedImage {
    pub(crate) label: String,
    pub(crate) image: Arc<Image>,
}

#[derive(Clone)]
struct InputSnapshot {
    content: String,
    cursor: usize,
    selection: Range<usize>,
    selection_anchor: usize,
    images: Vec<AttachedImage>,
    next_image_number: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Typing,
    Backspace,
    Delete,
}

#[derive(Clone, Copy)]
struct CoalescingEdit {
    kind: EditKind,
    separator_seen: bool,
    cursor_after: usize,
}

const UNDO_LIMIT: usize = 100;

fn can_coalesce_edit(
    previous: Option<CoalescingEdit>,
    kind: EditKind,
    cursor: usize,
    affected_text: &str,
    can_continue_existing: bool,
) -> bool {
    let contains_word = affected_text
        .chars()
        .any(|character| !character.is_whitespace());
    can_continue_existing
        && previous.is_some_and(|edit| {
            edit.kind == kind
                && edit.cursor_after == cursor
                && !(edit.separator_seen && contains_word)
        })
}

pub(crate) struct TextInput {
    focus: FocusHandle,
    content: String,
    placeholder: SharedString,
    cursor: usize,
    selection: Range<usize>,
    selection_anchor: usize,
    selecting: bool,
    last_layout: Option<TextLayout>,
    preferred_cursor_x: Option<Pixels>,
    borderless: bool,
    multiline: bool,
    compact: bool,
    images: Vec<AttachedImage>,
    next_image_number: usize,
    scroll: ScrollHandle,
    completion_active: bool,
    autoscroll_cursor: bool,
    undo_stack: Vec<InputSnapshot>,
    redo_stack: Vec<InputSnapshot>,
    coalescing_edit: Option<CoalescingEdit>,
}

impl TextInput {
    pub(crate) fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            content: String::new(),
            placeholder: placeholder.into(),
            cursor: 0,
            selection: 0..0,
            selection_anchor: 0,
            selecting: false,
            last_layout: None,
            preferred_cursor_x: None,
            borderless: false,
            multiline: false,
            compact: false,
            images: Vec::new(),
            next_image_number: 1,
            scroll: ScrollHandle::new(),
            completion_active: false,
            autoscroll_cursor: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            coalescing_edit: None,
        }
    }

    pub(crate) fn borderless(mut self) -> Self {
        self.borderless = true;
        self
    }

    pub(crate) fn multiline(mut self) -> Self {
        self.multiline = true;
        self
    }

    pub(crate) fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    pub(crate) fn text(&self) -> &str {
        &self.content
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn position_for_offset(&self, offset: usize) -> Option<gpui::Point<Pixels>> {
        self.last_layout.as_ref()?.position_for_index(offset)
    }

    pub(crate) fn set_completion_active(&mut self, active: bool) {
        self.completion_active = active;
    }

    pub(crate) fn replace_range(
        &mut self,
        range: Range<usize>,
        value: &str,
        cx: &mut Context<Self>,
    ) {
        if range.start > range.end
            || range.end > self.content.len()
            || !self.content.is_char_boundary(range.start)
            || !self.content.is_char_boundary(range.end)
        {
            return;
        }
        let value = self.normalized(value);
        if self.content[range.clone()] == value {
            return;
        }
        self.begin_atomic_edit();
        self.selection = range;
        self.replace_selection(&value);
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    pub(crate) fn images(&self) -> Vec<AttachedImage> {
        self.images.clone()
    }

    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.images.clear();
        self.next_image_number = 1;
        self.set_text("", cx);
    }

    pub(crate) fn restore_draft(
        &mut self,
        text: String,
        images: Vec<AttachedImage>,
        cx: &mut Context<Self>,
    ) {
        self.images = images;
        self.next_image_number = self
            .images
            .iter()
            .filter_map(|image| image.label.strip_prefix("image-")?.parse::<usize>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        self.content = text;
        self.cursor = self.content.len();
        self.collapse_selection(self.cursor);
        self.prune_images();
        self.reset_history();
        cx.notify();
    }

    pub(crate) fn remove_image(&mut self, label: &str, cx: &mut Context<Self>) {
        let marker = format!("[{label}]");
        if !self.images.iter().any(|image| image.label == label) && !self.content.contains(&marker)
        {
            return;
        }
        self.begin_atomic_edit();
        self.images.retain(|image| image.label != label);
        self.content = self.content.replace(&marker, "");
        self.cursor = self.cursor.min(self.content.len());
        self.collapse_selection(self.cursor);
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    pub(crate) fn insert_at_cursor(&mut self, value: &str, cx: &mut Context<Self>) {
        let value = self.normalized(value);
        if value.is_empty() && self.selection.is_empty() {
            return;
        }
        self.begin_atomic_edit();
        self.replace_selection(&value);
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    pub(crate) fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.content = text.into();
        self.cursor = self.content.len();
        self.collapse_selection(self.cursor);
        self.prune_images();
        self.reset_history();
        cx.notify();
    }

    pub(crate) fn select_all(&mut self, cx: &mut Context<Self>) {
        self.break_edit_group();
        self.selection_anchor = 0;
        self.select_to(self.content.len());
        cx.notify();
    }

    fn snapshot(&self) -> InputSnapshot {
        InputSnapshot {
            content: self.content.clone(),
            cursor: self.cursor,
            selection: self.selection.clone(),
            selection_anchor: self.selection_anchor,
            images: self.images.clone(),
            next_image_number: self.next_image_number,
        }
    }

    fn restore_snapshot(&mut self, snapshot: InputSnapshot) {
        self.content = snapshot.content;
        self.cursor = snapshot.cursor;
        self.selection = snapshot.selection;
        self.selection_anchor = snapshot.selection_anchor;
        self.images = snapshot.images;
        self.next_image_number = snapshot.next_image_number;
        self.selecting = false;
        self.preferred_cursor_x = None;
        self.autoscroll_cursor = true;
    }

    fn push_snapshot(stack: &mut Vec<InputSnapshot>, snapshot: InputSnapshot) {
        if stack.len() == UNDO_LIMIT {
            stack.remove(0);
        }
        stack.push(snapshot);
    }

    fn begin_atomic_edit(&mut self) {
        self.break_edit_group();
        let snapshot = self.snapshot();
        Self::push_snapshot(&mut self.undo_stack, snapshot);
        self.redo_stack.clear();
    }

    /// Groups adjacent typing and deletion into word-sized undo transactions.
    fn begin_coalescing_edit(
        &mut self,
        kind: EditKind,
        affected_text: &str,
        can_continue_existing: bool,
    ) {
        let contains_separator = affected_text.chars().any(char::is_whitespace);
        let continue_existing = can_coalesce_edit(
            self.coalescing_edit,
            kind,
            self.cursor,
            affected_text,
            can_continue_existing,
        );

        let separator_seen = if continue_existing {
            self.coalescing_edit.is_some_and(|edit| edit.separator_seen) || contains_separator
        } else {
            self.begin_atomic_edit();
            contains_separator
        };
        self.coalescing_edit = Some(CoalescingEdit {
            kind,
            separator_seen,
            cursor_after: self.cursor,
        });
    }

    fn update_edit_group_cursor(&mut self) {
        if let Some(edit) = &mut self.coalescing_edit {
            edit.cursor_after = self.cursor;
        }
    }

    fn break_edit_group(&mut self) {
        self.coalescing_edit = None;
    }

    fn reset_history(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.break_edit_group();
    }

    fn undo(&mut self) {
        self.break_edit_group();
        let Some(snapshot) = self.undo_stack.pop() else {
            return;
        };
        let current = self.snapshot();
        Self::push_snapshot(&mut self.redo_stack, current);
        self.restore_snapshot(snapshot);
    }

    fn redo(&mut self) {
        self.break_edit_group();
        let Some(snapshot) = self.redo_stack.pop() else {
            return;
        };
        let current = self.snapshot();
        Self::push_snapshot(&mut self.undo_stack, current);
        self.restore_snapshot(snapshot);
    }

    fn collapse_selection(&mut self, offset: usize) {
        self.cursor = offset;
        self.selection = offset..offset;
        self.selection_anchor = offset;
        self.preferred_cursor_x = None;
        self.autoscroll_cursor = true;
    }

    fn select_to(&mut self, offset: usize) {
        self.cursor = offset;
        self.selection = self.selection_anchor.min(offset)..self.selection_anchor.max(offset);
        self.preferred_cursor_x = None;
        self.autoscroll_cursor = true;
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content[..offset]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content[offset..]
            .char_indices()
            .nth(1)
            .map(|(index, _)| offset + index)
            .unwrap_or(self.content.len())
    }

    fn previous_word_boundary(&self, offset: usize) -> usize {
        previous_word_boundary(&self.content, offset)
    }

    fn next_word_boundary(&self, offset: usize) -> usize {
        next_word_boundary(&self.content, offset)
    }

    fn vertical_boundary(&self, down: bool) -> Option<(usize, Pixels)> {
        let layout = self.last_layout.as_ref()?;
        let position = layout.position_for_index(self.cursor)?;
        let line_height = layout.line_height();
        let target_y = if down {
            position.y + line_height * 1.5
        } else {
            position.y - line_height * 0.5
        };
        let bounds = layout.bounds();
        if target_y < bounds.top() || target_y >= bounds.bottom() {
            return None;
        }
        let preferred_x = self.preferred_cursor_x.unwrap_or(position.x);
        let offset = layout
            .index_for_position(gpui::point(preferred_x, target_y))
            .unwrap_or_else(|nearest| nearest)
            .min(self.content.len());
        Some((offset, preferred_x))
    }

    fn move_vertically(&mut self, down: bool, shift: bool) {
        let Some((offset, preferred_x)) = self.vertical_boundary(down) else {
            return;
        };
        if shift {
            self.select_to(offset);
        } else {
            self.collapse_selection(offset);
        }
        self.preferred_cursor_x = Some(preferred_x);
    }

    fn normalized(&self, value: &str) -> String {
        if self.multiline {
            value.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            value.replace(['\n', '\r'], " ")
        }
    }

    fn replace_selection(&mut self, value: &str) {
        let value = self.normalized(value);
        let range = self.selection.clone();
        self.content.replace_range(range.clone(), &value);
        self.collapse_selection(range.start + value.len());
        self.prune_images();
    }

    fn prune_images(&mut self) {
        // Text markers are the source of truth: deleting `[image-N]` also removes its payload.
        self.images
            .retain(|image| self.content.contains(&format!("[{}]", image.label)));
    }

    /// Prefers image/file clipboard entries, falling back to plain text when none are attachable.
    fn paste(&mut self, cx: &mut Context<Self>) {
        let item = cx.read_from_clipboard();
        if item.is_none() {
            tracing::debug!(
                "clipboard did not contain a GPUI-readable entry; trying native file-list fallback"
            );
        }
        let fallback_text = item
            .as_ref()
            .into_iter()
            .flat_map(|item| &item.entries)
            .filter_map(|entry| match entry {
                ClipboardEntry::String(value) => Some(value.text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let mut clipboard_images = Vec::new();
        let mut external_paths = Vec::new();
        for entry in item.iter().flat_map(|item| &item.entries) {
            match entry {
                ClipboardEntry::Image(image) => clipboard_images.push(Arc::new(image.clone())),
                ClipboardEntry::ExternalPaths(paths) => {
                    external_paths.extend(paths.paths().iter().cloned())
                }
                ClipboardEntry::String(_) => {}
            }
        }

        // Some Linux file managers expose copied files as URI text rather than a GPUI
        // ExternalPaths entry. Try that before using a second, native clipboard reader.
        if clipboard_images.is_empty()
            && external_paths.is_empty()
            && let Some(paths) = clipboard_text_paths(&fallback_text)
        {
            external_paths = paths;
        }

        let probe_native_paths =
            clipboard_images.is_empty() && external_paths.is_empty() && fallback_text.is_empty();
        tracing::debug!(
            entries = item.as_ref().map_or(0, |item| item.entries.len()),
            clipboard_images = clipboard_images.len(),
            external_paths = external_paths.len(),
            has_text = !fallback_text.is_empty(),
            probe_native_paths,
            "processing clipboard paste"
        );

        if clipboard_images.is_empty() && external_paths.is_empty() && !probe_native_paths {
            if !fallback_text.is_empty() {
                self.begin_atomic_edit();
                self.replace_selection(&fallback_text);
            }
            return;
        }

        let task = cx.background_spawn(async move {
            if probe_native_paths {
                external_paths = native_clipboard_paths();
            }
            let mut images = Vec::new();
            for (index, image) in clipboard_images.into_iter().enumerate() {
                let source = format!("clipboard image {}", index + 1);
                match normalize_for_harness(image, &source) {
                    Ok(image) => images.push(image),
                    Err(error) => tracing::warn!(error = %error, source, "could not prepare clipboard image attachment"),
                }
            }

            let mut remaining_paths = Vec::new();
            for path in external_paths {
                match load_external_image(&path) {
                    Ok(Some(image)) => images.push(image),
                    Ok(None) => remaining_paths.push(path),
                    Err(error) => {
                        tracing::warn!(error = %error, path = %path.display(), "could not load clipboard path as an image");
                        remaining_paths.push(path);
                    }
                }
            }
            (images, remaining_paths, fallback_text)
        });

        cx.spawn(async move |this, cx| {
            let (images, remaining_paths, fallback_text) = task.await;
            let _ = this.update(cx, |this, cx| {
                let previous_content = this.content.clone();
                let attached_images = !images.is_empty();
                if attached_images || !remaining_paths.is_empty() || !fallback_text.is_empty() {
                    this.begin_atomic_edit();
                }
                for image in images {
                    let label = format!("image-{}", this.next_image_number);
                    this.next_image_number += 1;
                    let marker = format!("[{label}]");
                    this.replace_selection(&marker);
                    this.images.push(AttachedImage { label, image });
                }

                if !remaining_paths.is_empty() {
                    let separator = if attached_images && this.multiline {
                        "\n"
                    } else if attached_images {
                        " "
                    } else {
                        ""
                    };
                    let paths = remaining_paths
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(if this.multiline { "\n" } else { " " });
                    this.replace_selection(&format!("{separator}{paths}"));
                } else if !attached_images && !fallback_text.is_empty() {
                    this.replace_selection(&fallback_text);
                }

                if this.content != previous_content {
                    cx.emit(InputEvent::Changed);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn copy(&self, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selection.clone()].to_string(),
            ));
        }
    }

    fn index_for_position(&self, event_position: gpui::Point<gpui::Pixels>) -> usize {
        let Some(layout) = self.last_layout.as_ref() else {
            return self.cursor;
        };
        layout
            .index_for_position(event_position)
            .unwrap_or_else(|nearest| nearest)
            .min(self.content.len())
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        cx.emit(InputEvent::Focused);
        self.break_edit_group();
        let offset = self.index_for_position(event.position);
        self.preferred_cursor_x = None;
        self.selecting = true;
        if event.click_count >= 2 {
            let range = word_range_at(&self.content, offset);
            self.selection_anchor = range.start;
            self.select_to(range.end);
        } else if event.modifiers.shift {
            self.select_to(offset);
        } else {
            self.collapse_selection(offset);
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.select_to(self.index_for_position(event.position));
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let previous_content = self.content.clone();
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let command = modifiers.control || modifiers.platform;
        let shift = modifiers.shift;
        match key {
            "escape" => {
                self.break_edit_group();
                self.completion_active = false;
                cx.emit(InputEvent::Escape);
            }
            "enter" | "tab" if self.completion_active => {
                self.break_edit_group();
                self.completion_active = false;
                cx.emit(InputEvent::CompletionAccepted);
            }
            "up" if self.completion_active => cx.emit(InputEvent::CompletionPrevious),
            "down" if self.completion_active => cx.emit(InputEvent::CompletionNext),
            "z" if command && shift => self.redo(),
            "z" if command => self.undo(),
            "enter" if self.multiline && !command => {
                let can_continue = self.selection.is_empty();
                self.begin_coalescing_edit(EditKind::Typing, "\n", can_continue);
                self.replace_selection("\n");
                self.update_edit_group_cursor();
            }
            "enter" => {
                self.break_edit_group();
                cx.emit(InputEvent::Submit);
            }
            "backspace" => {
                let can_continue = self.selection.is_empty();
                let range = if can_continue && self.cursor > 0 {
                    let previous = if modifiers.control {
                        self.previous_word_boundary(self.cursor)
                    } else {
                        self.previous_boundary(self.cursor)
                    };
                    previous..self.cursor
                } else {
                    self.selection.clone()
                };
                if !range.is_empty() {
                    let removed = self.content[range.clone()].to_string();
                    self.begin_coalescing_edit(EditKind::Backspace, &removed, can_continue);
                    self.selection = range;
                    self.replace_selection("");
                    if can_continue {
                        self.update_edit_group_cursor();
                    } else {
                        self.break_edit_group();
                    }
                }
            }
            "delete" => {
                let can_continue = self.selection.is_empty();
                let range = if can_continue && self.cursor < self.content.len() {
                    self.cursor..self.next_boundary(self.cursor)
                } else {
                    self.selection.clone()
                };
                if !range.is_empty() {
                    let removed = self.content[range.clone()].to_string();
                    self.begin_coalescing_edit(EditKind::Delete, &removed, can_continue);
                    self.selection = range;
                    self.replace_selection("");
                    if can_continue {
                        self.update_edit_group_cursor();
                    } else {
                        self.break_edit_group();
                    }
                }
            }
            "left" => {
                self.break_edit_group();
                let offset = if !shift && !self.selection.is_empty() {
                    self.selection.start
                } else if modifiers.control {
                    self.previous_word_boundary(self.cursor)
                } else {
                    self.previous_boundary(self.cursor)
                };
                if shift {
                    self.select_to(offset);
                } else {
                    self.collapse_selection(offset);
                }
            }
            "right" => {
                self.break_edit_group();
                let offset = if !shift && !self.selection.is_empty() {
                    self.selection.end
                } else if modifiers.control {
                    self.next_word_boundary(self.cursor)
                } else {
                    self.next_boundary(self.cursor)
                };
                if shift {
                    self.select_to(offset);
                } else {
                    self.collapse_selection(offset);
                }
            }
            "up" if self.multiline => {
                self.break_edit_group();
                self.move_vertically(false, shift);
            }
            "down" if self.multiline => {
                self.break_edit_group();
                self.move_vertically(true, shift);
            }
            "home" => {
                self.break_edit_group();
                if shift {
                    self.select_to(0);
                } else {
                    self.collapse_selection(0);
                }
            }
            "end" => {
                self.break_edit_group();
                let end = self.content.len();
                if shift {
                    self.select_to(end);
                } else {
                    self.collapse_selection(end);
                }
            }
            "a" if command => {
                self.break_edit_group();
                self.selection_anchor = 0;
                self.select_to(self.content.len());
            }
            "c" if command => self.copy(cx),
            "x" if command => {
                self.copy(cx);
                if !self.selection.is_empty() {
                    self.begin_atomic_edit();
                    self.replace_selection("");
                }
            }
            "v" if command => self.paste(cx),
            _ if !command => {
                if let Some(character) = event.keystroke.key_char.as_deref() {
                    let character = self.normalized(character);
                    if !character.is_empty() || !self.selection.is_empty() {
                        let can_continue = self.selection.is_empty();
                        self.begin_coalescing_edit(EditKind::Typing, &character, can_continue);
                        self.replace_selection(&character);
                        self.update_edit_group_cursor();
                    }
                } else {
                    self.break_edit_group();
                }
            }
            _ => return,
        }
        if self.content != previous_content {
            cx.emit(InputEvent::Changed);
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl EventEmitter<InputEvent> for TextInput {}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

struct TextInputElement {
    text: StyledText,
    layout: TextLayout,
    cursor: Option<usize>,
    autoscroll_cursor: Option<usize>,
    scroll: ScrollHandle,
}

impl IntoElement for TextInputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextInputElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.text.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text
            .prepaint(id, inspector_id, bounds, state, window, cx);

        let Some(cursor) = self.autoscroll_cursor else {
            return;
        };
        let Some(position) = self.layout.position_for_index(cursor) else {
            return;
        };

        let viewport = self.scroll.bounds();
        let current_offset = self.scroll.offset();
        let next_y = vertical_offset_to_reveal(
            current_offset.y,
            self.scroll.max_offset().y,
            viewport.top()..viewport.bottom(),
            position.y..position.y + self.layout.line_height(),
        );
        if next_y == current_offset.y {
            return;
        }

        self.scroll
            .set_offset(gpui::point(current_offset.x, next_y));

        // The parent's scroll offset was already applied before this child was
        // prepainted. Move this frame's text as well, then redraw so the parent
        // and scrollbar use the new offset on the next frame.
        let translated_bounds = Bounds::new(
            gpui::point(bounds.origin.x, bounds.origin.y + next_y - current_offset.y),
            bounds.size,
        );
        self.text
            .prepaint(id, inspector_id, translated_bounds, state, window, cx);
        window.refresh();
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text
            .paint(id, inspector_id, bounds, state, prepaint, window, cx);
        if let Some(cursor) = self.cursor
            && let Some(position) = self.layout.position_for_index(cursor)
        {
            window.paint_quad(fill(
                Bounds::new(position, size(px(1.5), self.layout.line_height())),
                rgb(blue()),
            ));
        }
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus.is_focused(window);
        let show_placeholder = self.content.is_empty();
        let display = if show_placeholder {
            self.placeholder.to_string()
        } else {
            self.content.clone()
        };

        let mut boundaries = vec![0, display.len()];
        if !self.selection.is_empty() {
            boundaries.extend([self.selection.start, self.selection.end]);
        }
        for image in &self.images {
            if let Some(start) = self.content.find(&format!("[{}]", image.label)) {
                boundaries.extend([start, start + image.label.len() + 2]);
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        let highlights = boundaries
            .windows(2)
            .filter_map(|window| {
                let range = window[0]..window[1];
                if range.is_empty() {
                    return None;
                }
                let content_start = range.start;
                let selected = !self.selection.is_empty()
                    && content_start >= self.selection.start
                    && content_start < self.selection.end;
                let image_marker = self.images.iter().any(|image| {
                    self.content
                        .find(&format!("[{}]", image.label))
                        .is_some_and(|start| {
                            content_start >= start && content_start < start + image.label.len() + 2
                        })
                });
                (selected || image_marker).then_some((
                    range,
                    HighlightStyle {
                        color: image_marker.then_some(rgb(blue()).into()),
                        background_color: selected
                            .then_some(rgb(crate::theme::text_selection()).opacity(0.33).into()),
                        ..Default::default()
                    },
                ))
            })
            .collect::<Vec<_>>();
        let text = StyledText::new(display).with_highlights(highlights);
        let layout = text.layout().clone();
        self.last_layout = Some(layout.clone());
        let cursor = (focused && self.selection.is_empty()).then_some(self.cursor);
        let autoscroll_cursor =
            (focused && self.autoscroll_cursor && self.multiline).then_some(self.cursor);
        if focused {
            self.autoscroll_cursor = false;
        }
        let viewport = self.scroll.bounds().size.height.as_f32();
        let max_offset = self.scroll.max_offset().y.as_f32();
        let thumb_fraction = if viewport > 0.0 && max_offset > 0.0 {
            let min_fraction = (10.0 / viewport).clamp(0.08, 1.0);
            (viewport / (viewport + max_offset)).clamp(min_fraction, 1.0)
        } else {
            1.0
        };
        let scroll_fraction = if max_offset > 0.0 {
            (-self.scroll.offset().y.as_f32() / max_offset).clamp(0.0, 1.0)
        } else {
            0.0
        };

        div()
            .id(("text-input", cx.entity_id()))
            .relative()
            .w_full()
            .min_w(px(0.0))
            .flex()
            .overflow_hidden()
            .when(self.multiline, |element| {
                element
                    .min_h(px(42.0))
                    .max_h(window.viewport_size().height * 0.6)
                    .items_start()
                    .line_height(px(20.0))
            })
            .when(!self.multiline, |element| {
                element
                    .when(self.compact, |element| {
                        element.h_full().line_height(px(18.0))
                    })
                    .when(!self.compact, |element| element.h(px(42.0)))
                    .items_center()
            })
            .when(!self.borderless, |element| {
                element
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(if focused { accent() } else { border() }))
                    .bg(rgb(surface()))
            })
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .text_sm()
            .text_color(rgb(if show_placeholder {
                faint()
            } else {
                theme_text()
            }))
            .child(
                div()
                    .id(("text-input-scroll", cx.entity_id()))
                    .w_full()
                    .min_w(px(0.0))
                    .when(!self.compact, |element| element.px_3())
                    .when(self.multiline, |element| {
                        element
                            .max_h(window.viewport_size().height * 0.6)
                            .py_2()
                            .pr_4()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .whitespace_normal()
                    })
                    .when(!self.multiline, |element| element.whitespace_nowrap())
                    .child(TextInputElement {
                        text,
                        layout,
                        cursor,
                        autoscroll_cursor,
                        scroll: self.scroll.clone(),
                    }),
            )
            .when(self.multiline && max_offset > 0.0, |element| {
                element.child(
                    div()
                        .absolute()
                        .top(px(4.0))
                        .bottom(px(4.0))
                        .right(px(3.0))
                        .w(px(2.0))
                        .rounded_full()
                        .bg(rgba(0xffffff16))
                        .child(
                            div()
                                .absolute()
                                .top(relative((1.0 - thumb_fraction) * scroll_fraction))
                                .h(relative(thumb_fraction))
                                .min_h(px(10.0))
                                .w_full()
                                .rounded_full()
                                .bg(rgb(muted())),
                        ),
                )
            })
    }
}
