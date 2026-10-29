//! Hand-rolled multiline text input: gpui has no widget, so this implements
//! `EntityInputHandler` (IME, keyboard) plus a custom `Element` that shapes the
//! text with wrapping, grows with content up to a share of the window, then scrolls
//! (wheel, and automatically to keep the cursor in view) and paints cursor/selection.
//! Mouse selection (double click = word, triple = all) keeps following the mouse once it
//! leaves the input, and every edit is undoable (typing a word is one step).
//! Adapted from zed's `gpui/examples/input.rs`, extended to multiple lines.

use crate::select::{self, word_at};
use crate::theme::u;
use gpui::{
    App, Bounds, ClipboardEntry, ClipboardItem, ContentMask, Context, Corners, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, InspectorElementId, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollEvent, SharedString, Style, TextRun, TextStyle, UTF16Selection,
    Window, WrappedLine, actions, div, fill, point, prelude::*, px, quad, relative, rgba, size,
};
use std::ops::Range;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

actions!(
    input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
        Undo,
        Redo,
        Newline,
        Submit,
        SubmitQueued
    ]
);

pub const KEY_CONTEXT: &str = "Input";
/// A multiline input grows with its text up to this share of the window height, then scrolls.
const MAX_HEIGHT_FRACTION: f32 = 0.7;
/// Room (design pixels, `theme::u`) kept for the composer's padding and chip row so the whole card stays within that share.
const CARD_CHROME: f32 = 90.;
/// Right inset (design pixels) reserved for the scrollbar so text never runs under it.
const BAR_GUTTER: f32 = 12.;
/// Edits of one kind closer together than this join the same undo step.
const UNDO_GROUP: Duration = Duration::from_secs(1);
const UNDO_DEPTH: usize = 200;

/// Emitted on ctrl+Enter, or shift+Enter with `queued` (Enter alone inserts a newline;
/// single-line inputs submit on Enter too, with `ctrl` false). What `queued` means is up to
/// the subscriber.
pub struct Submitted {
    pub queued: bool,
    pub ctrl: bool,
}

/// Up / Down in a single-line input (a multiline one moves its cursor instead): for a list
/// under the input to move its selection. `down` is +1, up -1.
pub struct Arrow(pub i32);

/// Emitted when the clipboard held an image on paste (nothing is inserted; the
/// subscriber decides what to do with it, if anything).
pub struct ImagePasted(pub gpui::Image);

/// Emitted after every edit the user makes (typing, paste, delete, undo, IME commit);
/// not after `set_text`, so a subscriber filling the input doesn't hear its own write.
pub struct Changed;

/// Attachment token the composer colors: `[image-N]`.
pub fn image_token(n: u32) -> String {
    format!("[image-{n}]")
}

/// The distinct `N`s of the `[image-N]` tokens in `text`, ascending.
pub fn image_numbers(text: &str) -> Vec<u32> {
    let mut ns: Vec<u32> =
        image_tokens(text).into_iter().filter_map(|r| text[r].strip_prefix("[image-")?.strip_suffix(']')?.parse().ok()).collect();
    ns.sort_unstable();
    ns.dedup();
    ns
}

/// Byte ranges of every `[image-N]` token in `text`.
fn image_tokens(text: &str) -> Vec<Range<usize>> {
    let mut out = vec![];
    let mut from = 0;
    while let Some(i) = text[from..].find("[image-") {
        let start = from + i;
        let digits = start + "[image-".len();
        let n = text[digits..].bytes().take_while(u8::is_ascii_digit).count();
        from = digits;
        if n > 0 && text.as_bytes().get(digits + n) == Some(&b']') {
            out.push(start..digits + n + 1);
            from = digits + n + 1;
        }
    }
    out
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    single_line: bool,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    /// Layout from the last paint, used for mouse hit testing and IME bounds.
    last_layout: Option<Layout>,
    last_bounds: Option<Bounds<Pixels>>,
    /// What the mouse press selected (a word after a double click) while the button is
    /// down; dragging extends the selection from it.
    drag_anchor: Option<Range<usize>>,
    /// Vertical scroll of the text inside its (clamped) bounds.
    scroll_y: Pixels,
    /// Set by every edit / cursor move: the next paint scrolls the cursor into view.
    follow_cursor: bool,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The last edit, to join the next one of the same kind into its undo step.
    last_edit: Option<(Edit, Instant)>,
}

/// Text with its selection: what a draft keeps per thread (`Dirigent::save_draft`), and
/// what `set_content` swaps into the input when the user moves between threads.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct Content {
    pub text: String,
    pub selection: Range<usize>,
    pub reversed: bool,
}

/// Content and selection before an edit.
struct Snapshot {
    content: SharedString,
    selected_range: Range<usize>,
    reversed: bool,
}

/// Kinds of edit for undo grouping: typing a word joins the spaces that end it, deleting
/// joins deleting; anything else (paste, newline, IME) is a step of its own.
#[derive(Clone, Copy, PartialEq)]
enum Edit {
    Word,
    Space,
    Delete,
    Other,
}

impl Edit {
    fn of(range: &Range<usize>, new_text: &str) -> Self {
        if new_text.is_empty() {
            Self::Delete
        } else if range.is_empty() && new_text.graphemes(true).count() == 1 {
            if new_text == " " { Self::Space } else { Self::Word }
        } else {
            Self::Other
        }
    }

    fn joins(self, next: Self) -> bool {
        matches!((self, next), (Self::Word, Self::Word | Self::Space) | (Self::Space, Self::Space) | (Self::Delete, Self::Delete))
    }
}

/// Shaped text: one `WrappedLine` per `\n`-separated line, plus their byte offsets.
struct Layout {
    lines: Vec<WrappedLine>,
    starts: Vec<usize>,
    line_height: Pixels,
    text: SharedString,
}

impl Layout {
    /// `font` is the element's resolved text style: it must be captured while the element's
    /// style stack is active (Taffy's measure callback runs outside it, with window defaults).
    fn shape(text: &SharedString, placeholder: bool, wrap_width: Pixels, font: &Font, window: &Window) -> Self {
        let (style, font_size) = (&font.style, font.size);
        let color = if placeholder { rgba(crate::theme::t().fg_2).into() } else { style.color };
        let run = |len, color| TextRun { len, font: style.font(), color, background_color: None, underline: None, strikethrough: None };
        // Plain text, with every attachment token in the accent color.
        let mut runs = vec![];
        let mut at = 0;
        for range in if placeholder { vec![] } else { image_tokens(text) } {
            runs.push(run(range.start - at, color));
            runs.push(run(range.len(), rgba(crate::theme::t().accent_ink).into()));
            at = range.end;
        }
        runs.push(run(text.len() - at, color));
        let lines = window.text_system().shape_text(text.clone(), font_size, &runs, Some(wrap_width), None).unwrap_or_default();
        let mut starts = Vec::with_capacity(lines.len());
        let mut off = 0;
        for line in &lines {
            starts.push(off);
            off += line.len() + 1;
        }
        Self { lines: lines.into_vec(), starts, line_height: font.line_height, text: text.clone() }
    }

    fn height(&self) -> Pixels {
        self.lines.iter().map(|l| l.size(self.line_height).height).fold(px(0.), |a, b| a + b)
    }

    /// Pixel position (relative to the text origin) of a byte index.
    fn position(&self, index: usize) -> Point<Pixels> {
        let mut y = px(0.);
        for (line, &start) in self.lines.iter().zip(&self.starts) {
            let end = start + line.len();
            if index >= start && index <= end {
                let p = line.position_for_index(index - start, self.line_height).unwrap_or_default();
                return point(p.x, y + p.y);
            }
            y += line.size(self.line_height).height;
        }
        point(px(0.), y - self.line_height)
    }

    /// Byte index closest to a pixel position (relative to the text origin).
    fn index(&self, pos: Point<Pixels>) -> usize {
        if pos.y < px(0.) {
            return 0;
        }
        let mut y = px(0.);
        for (line, &start) in self.lines.iter().zip(&self.starts) {
            let h = line.size(self.line_height).height;
            if pos.y < y + h {
                let local = point(pos.x.max(px(0.)), pos.y - y);
                return start + line.closest_index_for_position(local, self.line_height).unwrap_or_else(|i| i);
            }
            y += h;
        }
        self.text.len()
    }
}

/// The text style in effect where the input is rendered, resolved to pixels.
struct Font {
    style: TextStyle,
    size: Pixels,
    line_height: Pixels,
}

impl Font {
    fn capture(window: &Window) -> Self {
        let style = window.text_style();
        let size = style.font_size.to_pixels(window.rem_size());
        Self { style, size, line_height: window.line_height() }
    }
}

impl TextInput {
    pub fn new(cx: &mut Context<Self>, placeholder: &str, single_line: bool) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.into(),
            single_line,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            drag_anchor: None,
            scroll_y: px(0.),
            follow_cursor: false,
            undo: vec![],
            redo: vec![],
            last_edit: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn set_text(&mut self, text: &str, window: &Window, cx: &mut Context<Self>) {
        self.record(Edit::Other);
        self.content = text.to_owned().into();
        self.selected_range = self.content.len()..self.content.len();
        self.selection_reversed = false;
        self.marked_range = None;
        self.follow_cursor = true;
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    pub fn content(&self) -> Content {
        Content { text: self.content.to_string(), selection: self.selected_range.clone(), reversed: self.selection_reversed }
    }

    /// Replace text and selection with another draft's. Not an edit: the undo history goes
    /// with the old draft. A selection that doesn't fit the text falls back to its end.
    pub fn set_content(&mut self, c: Content, window: &Window, cx: &mut Context<Self>) {
        let fits =
            c.selection.start <= c.selection.end && c.text.is_char_boundary(c.selection.start) && c.text.is_char_boundary(c.selection.end);
        self.selected_range = if fits { c.selection } else { c.text.len()..c.text.len() };
        self.selection_reversed = c.reversed && fits;
        self.content = c.text.into();
        self.marked_range = None;
        self.follow_cursor = true;
        self.undo.clear();
        self.redo.clear();
        self.last_edit = None;
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    /// Before an edit: remember the state to undo to, unless the edit joins the last one.
    fn record(&mut self, kind: Edit) {
        let now = Instant::now();
        let joins = self.last_edit.is_some_and(|(last, at)| last.joins(kind) && now.duration_since(at) < UNDO_GROUP);
        if !joins {
            self.undo.push(self.snapshot());
            if self.undo.len() > UNDO_DEPTH {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last_edit = Some((kind, now));
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot { content: self.content.clone(), selected_range: self.selected_range.clone(), reversed: self.selection_reversed }
    }

    fn restore(&mut self, s: Snapshot, window: &Window, cx: &mut Context<Self>) {
        self.content = s.content;
        self.selected_range = s.selected_range;
        self.selection_reversed = s.reversed;
        self.marked_range = None;
        self.follow_cursor = true;
        self.last_edit = None;
        window.notify_text_input_changed(&self.focus_handle);
        cx.emit(Changed);
        cx.notify();
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = self.undo.pop() {
            self.redo.push(self.snapshot());
            self.restore(s, window, cx);
        }
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = self.redo.pop() {
            self.undo.push(self.snapshot());
            self.restore(s, window, cx);
        }
    }

    /// Take the content, leaving the input empty.
    pub fn take(&mut self, window: &Window, cx: &mut Context<Self>) -> String {
        let text = self.content.to_string();
        self.set_text("", window, cx);
        text
    }

    /// Remove every occurrence of `needle`, keeping the cursor where it was (clamped).
    pub fn remove_all(&mut self, needle: &str, window: &Window, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        let before = self.content[..cursor].matches(needle).count() * needle.len();
        self.set_text(&self.content.replace(needle, ""), window, cx);
        self.move_to(cursor - before, window, cx);
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(Submitted { queued: false, ctrl: true });
    }

    fn submit_queued(&mut self, _: &SubmitQueued, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(Submitted { queued: true, ctrl: false });
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.single_line {
            cx.emit(Submitted { queued: false, ctrl: false });
        } else {
            self.replace_text_in_range(None, "\n", window, cx);
        }
    }

    fn left(&mut self, _: &Left, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), window, cx);
        } else {
            self.move_to(self.selected_range.start, window, cx)
        }
    }

    fn right(&mut self, _: &Right, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), window, cx);
        } else {
            self.move_to(self.selected_range.end, window, cx)
        }
    }

    fn up(&mut self, _: &Up, window: &mut Window, cx: &mut Context<Self>) {
        if self.single_line {
            return cx.emit(Arrow(-1));
        }
        self.move_vertical(-1., window, cx);
    }

    fn down(&mut self, _: &Down, window: &mut Window, cx: &mut Context<Self>) {
        if self.single_line {
            return cx.emit(Arrow(1));
        }
        self.move_vertical(1., window, cx);
    }

    /// Move the cursor one visual row up/down using the last layout.
    fn move_vertical(&mut self, rows: f32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = self.last_layout.as_ref().filter(|l| l.text == self.content) else { return };
        let pos = layout.position(self.cursor_offset());
        let target = point(pos.x, pos.y + layout.line_height * rows + layout.line_height * 0.5);
        let idx = if target.y < px(0.) { 0 } else { layout.index(target) };
        self.move_to(idx, window, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), window, cx);
    }

    fn select_right(&mut self, _: &SelectRight, window: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), window, cx);
    }

    fn select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, window, cx);
        self.select_to(self.content.len(), window, cx)
    }

    fn home(&mut self, _: &Home, window: &mut Window, cx: &mut Context<Self>) {
        let line_start = self.content[..self.cursor_offset()].rfind('\n').map(|i| i + 1).unwrap_or(0);
        self.move_to(line_start, window, cx);
    }

    fn end(&mut self, _: &End, window: &mut Window, cx: &mut Context<Self>) {
        let c = self.cursor_offset();
        let line_end = self.content[c..].find('\n').map(|i| c + i).unwrap_or(self.content.len());
        self.move_to(line_end, window, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            if self.cursor_offset() == prev {
                return;
            }
            // Not `select_to`: that would end the undo group of a run of backspaces.
            self.selected_range = prev..self.cursor_offset();
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if self.cursor_offset() == next {
                return;
            }
            self.selected_range = self.cursor_offset()..next;
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    /// One click puts the cursor, two select the word, three everything; shift extends.
    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.index_for_mouse_position(event.position);
        let anchor = if event.modifiers.shift {
            let a = if self.selection_reversed { self.selected_range.end } else { self.selected_range.start };
            a..a
        } else {
            match event.click_count {
                1 => idx..idx,
                2 => word_at(&self.content, idx),
                _ => 0..self.content.len(),
            }
        };
        self.drag_anchor = Some(anchor);
        self.drag(event.position, window, cx);
    }

    /// Ends a drag; what it selected goes to the primary selection.
    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.drag_anchor.take().is_some() && !self.selected_range.is_empty() {
            write_to_primary(self.content[self.selected_range.clone()].to_string(), cx);
        }
    }

    /// Extend the selection from the drag anchor to the mouse. Registered on the window
    /// while the button is down, so it keeps working once the mouse leaves the input:
    /// above it selects to the start, below it to the end.
    fn drag(&mut self, position: Point<Pixels>, window: &Window, cx: &mut Context<Self>) {
        let Some(anchor) = self.drag_anchor.clone() else { return };
        let idx = self.index_for_mouse_position(position);
        self.selected_range = anchor.start.min(idx)..anchor.end.max(idx);
        self.selection_reversed = idx < anchor.start;
        self.follow_cursor = true;
        self.last_edit = None;
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify();
    }

    fn on_scroll(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some((layout, bounds)) = self.last_layout.as_ref().zip(self.last_bounds) else { return };
        let max = (layout.height() - bounds.size.height).max(px(0.));
        if max == px(0.) {
            return;
        }
        let dy = event.delta.pixel_delta(layout.line_height).y;
        self.scroll_y = (self.scroll_y - dy).clamp(px(0.), max);
        self.follow_cursor = false;
        cx.notify();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else { return };
        if let Some(text) = item.text() {
            let text = if self.single_line { text.replace('\n', " ") } else { text };
            self.replace_text_in_range(None, &text, window, cx);
        } else if let Some(ClipboardEntry::Image(image)) = item.entries.into_iter().find(|e| matches!(e, ClipboardEntry::Image(_))) {
            cx.emit(ImagePasted(image));
        }
    }

    /// With nothing selected the action goes on to the root, which copies the transcript selection.
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            cx.propagate();
        } else {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.selected_range.clone()].to_string()));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.selected_range.clone()].to_string()));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    pub fn move_to(&mut self, offset: usize, window: &Window, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.follow_cursor = true;
        self.last_edit = None;
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed { self.selected_range.start } else { self.selected_range.end }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        match (self.last_bounds.as_ref(), self.last_layout.as_ref()) {
            (Some(bounds), Some(layout)) if layout.text == self.content => layout.index(position - self.text_origin(bounds)),
            _ => 0,
        }
    }

    /// Where the (possibly scrolled-off) top of the text sits in window coordinates.
    fn text_origin(&self, bounds: &Bounds<Pixels>) -> Point<Pixels> {
        bounds.origin - point(px(0.), self.scroll_y)
    }

    fn select_to(&mut self, offset: usize, window: &Window, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.follow_cursor = true;
        self.last_edit = None;
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content.grapheme_indices(true).rev().find_map(|(idx, _)| (idx < offset).then_some(idx)).unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content.grapheme_indices(true).find_map(|(idx, _)| (idx > offset).then_some(idx)).unwrap_or(self.content.len())
    }
}

impl EventEmitter<Submitted> for TextInput {}
impl EventEmitter<Arrow> for TextInput {}
impl EventEmitter<ImagePasted> for TextInput {}
impl EventEmitter<Changed> for TextInput {}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(&mut self, _ignore_disabled: bool, _window: &mut Window, _cx: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: self.range_to_utf16(&self.selected_range), reversed: self.selection_reversed })
    }

    fn set_selected_text_range(&mut self, range_utf16: Range<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = self.range_from_utf16(&range_utf16);
        self.selection_reversed = false;
        self.follow_cursor = true;
        self.last_edit = None;
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify();
    }

    fn text_length_utf16(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<usize> {
        Some(self.content.encode_utf16().count())
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.marked_range = None;
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    fn replace_text_in_range(&mut self, range_utf16: Option<Range<usize>>, new_text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let range =
            range_utf16.as_ref().map(|r| self.range_from_utf16(r)).or(self.marked_range.clone()).unwrap_or(self.selected_range.clone());
        self.record(Edit::of(&range, new_text));
        self.content = (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.selection_reversed = false;
        self.marked_range.take();
        self.follow_cursor = true;
        window.notify_text_input_changed(&self.focus_handle);
        cx.emit(Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range =
            range_utf16.as_ref().map(|r| self.range_from_utf16(r)).or(self.marked_range.clone()).unwrap_or(self.selected_range.clone());
        self.record(Edit::Other);
        self.content = (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.marked_range = (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        let insertion_start_utf16 = self.offset_to_utf16(range.start);
        let inserted_length_utf16 = new_text.encode_utf16().count();
        self.selected_range = new_selected_range_utf16
            .map(|selected| {
                self.range_from_utf16(
                    &(insertion_start_utf16 + selected.start.min(inserted_length_utf16)
                        ..insertion_start_utf16 + selected.end.min(inserted_length_utf16)),
                )
            })
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.selection_reversed = false;
        self.follow_cursor = true;
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref().filter(|l| l.text == self.content)?;
        let range = self.range_from_utf16(&range_utf16);
        let start = layout.position(range.start);
        let end = layout.position(range.end);
        let o = self.text_origin(&bounds);
        Some(Bounds::from_corners(o + start, o + point(end.x, end.y + layout.line_height)))
    }

    fn selection_bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Bounds<Pixels>> {
        self.bounds_for_range(range_utf16, bounds, window, cx).into_iter().collect()
    }

    fn character_index_for_point(&mut self, point: Point<Pixels>, _window: &mut Window, _cx: &mut Context<Self>) -> Option<usize> {
        let bounds = self.last_bounds?;
        let layout = self.last_layout.as_ref().filter(|l| l.text == self.content)?;
        Some(self.offset_to_utf16(layout.index(point - self.text_origin(&bounds))))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct Prepaint {
    layout: Layout,
    cursor: Option<Bounds<Pixels>>,
    selection: Vec<select::Row>,
    scroll_y: Pixels,
}

/// Wrap width: single-line inputs use it all, multiline ones keep a gutter for the scrollbar.
fn wrap_width(width: Pixels, single_line: bool) -> Pixels {
    if single_line { width } else { (width - u(BAR_GUTTER)).max(px(1.)) }
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    /// Height follows the wrapped content, from one line up to `MAX_HEIGHT_FRACTION` of the window.
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let input = self.input.clone();
        let font = Font::capture(window);
        let id = window.request_measured_layout(style, move |known, available, window, cx| {
            let width = known.width.or(match available.width {
                gpui::AvailableSpace::Definite(w) => Some(w),
                _ => None,
            });
            let line_height = font.line_height;
            let (text, placeholder) = display_text(&input, cx);
            let single_line = input.read(cx).single_line;
            let height = match width {
                Some(w) if w > px(0.) => Layout::shape(&text, placeholder, wrap_width(w, single_line), &font, window).height(),
                _ => line_height,
            };
            let max = if single_line { line_height } else { window.viewport_size().height * MAX_HEIGHT_FRACTION - u(CARD_CHROME) };
            size(width.unwrap_or(px(0.)), height.clamp(line_height, max.max(line_height)))
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let (text, placeholder) = display_text(&self.input, cx);
        let font = Font::capture(window);
        let layout = Layout::shape(&text, placeholder, wrap_width(bounds.size.width, self.input.read(cx).single_line), &font, window);
        let lh = layout.line_height;
        // Scroll: clamp to the content, and bring the cursor into view after an edit or move.
        let scroll_y = self.input.update(cx, |input, _| {
            let max = (layout.height() - bounds.size.height).max(px(0.));
            if input.follow_cursor && !placeholder {
                input.follow_cursor = false;
                let y = layout.position(input.cursor_offset()).y;
                if y < input.scroll_y {
                    input.scroll_y = y;
                } else if y + lh > input.scroll_y + bounds.size.height {
                    input.scroll_y = y + lh - bounds.size.height;
                }
            }
            input.scroll_y = input.scroll_y.clamp(px(0.), max);
            input.scroll_y
        });
        let input = self.input.read(cx);
        let o = bounds.origin - point(px(0.), scroll_y);
        let (cursor, selection) = if placeholder {
            (Some(Bounds::new(bounds.origin, size(px(2.), lh))), vec![])
        } else if input.selected_range.is_empty() {
            let p = layout.position(input.cursor_offset());
            (Some(Bounds::new(o + p, size(px(2.), lh))), vec![])
        } else {
            let lines = layout.lines.iter().map(|l| &***l);
            (None, select::selection_rows(lines, o, lh, input.selected_range.clone()))
        };
        Prepaint { layout, cursor, selection, scroll_y }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.input.clone()), cx);
        // A drag follows the mouse anywhere in the window, not just over the input. Always
        // registered: the press and the first move can land in the same frame.
        let input = self.input.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
            if phase.bubble() && input.read(cx).drag_anchor.is_some() {
                input.update(cx, |input, cx| input.drag(ev.position, window, cx));
            }
        });
        let lh = prepaint.layout.line_height;
        let content_h = prepaint.layout.height();
        let visible = bounds.size.height;
        // Everything scrolled is clipped to the element; lines fully outside are skipped.
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            select::paint_selection(&prepaint.selection, lh, window);
            let mut y = -prepaint.scroll_y;
            for line in &prepaint.layout.lines {
                let h = line.size(lh).height;
                if y + h >= px(0.) && y <= visible {
                    let _ = line.paint(bounds.origin + point(px(0.), y), lh, gpui::TextAlign::Left, None, window, cx);
                }
                y += h;
            }
            if focus_handle.is_focused(window)
                && let Some(cursor) = prepaint.cursor.take()
            {
                window.paint_quad(fill(cursor, rgba(crate::theme::t().fg)));
            }
        });
        if content_h > visible {
            let thumb_h = (visible * (f32::from(visible) / f32::from(content_h))).max(u(20.));
            let thumb_y = (visible - thumb_h) * (f32::from(prepaint.scroll_y) / f32::from(content_h - visible));
            let thumb = Bounds::new(point(bounds.right() - u(4.), bounds.origin.y + thumb_y), size(u(4.), thumb_h));
            window.paint_quad(quad(
                thumb,
                Corners::all(u(2.)),
                rgba(crate::theme::alpha(crate::theme::t().fg_1, 0x66)),
                px(0.),
                rgba(0x00000000),
                gpui::BorderStyle::default(),
            ));
        }
        let layout = std::mem::replace(&mut prepaint.layout, Layout { lines: vec![], starts: vec![], line_height: lh, text: "".into() });
        self.input.update(cx, |input, _| {
            input.last_layout = Some(layout);
            input.last_bounds = Some(bounds);
        });
    }
}

/// Put `text` in the X11 / Wayland primary selection, which a middle click pastes. Other
/// platforms have none.
pub fn write_to_primary(text: String, cx: &mut App) {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    cx.write_to_primary(ClipboardItem::new_string(text));
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    let _ = (text, cx);
}

/// Text to shape, and whether it is the placeholder.
fn display_text(input: &Entity<TextInput>, cx: &App) -> (SharedString, bool) {
    let input = input.read(cx);
    if input.content.is_empty() { (input.placeholder.clone(), true) } else { (input.content.clone(), false) }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::submit_queued))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll(cx.listener(Self::on_scroll))
            .w_full()
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Key bindings for the `Input` context. Registered once in `main`.
pub fn key_bindings() -> Vec<gpui::KeyBinding> {
    use gpui::KeyBinding;
    let c = Some(KEY_CONTEXT);
    vec![
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("left", Left, c),
        KeyBinding::new("right", Right, c),
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("ctrl-a", SelectAll, c),
        KeyBinding::new("ctrl-v", Paste, c),
        KeyBinding::new("ctrl-c", Copy, c),
        KeyBinding::new("ctrl-x", Cut, c),
        KeyBinding::new("ctrl-z", Undo, c),
        KeyBinding::new("ctrl-shift-z", Redo, c),
        KeyBinding::new("ctrl-y", Redo, c),
        KeyBinding::new("home", Home, c),
        KeyBinding::new("end", End, c),
        KeyBinding::new("enter", Newline, c),
        KeyBinding::new("ctrl-enter", Submit, c),
        KeyBinding::new("shift-enter", SubmitQueued, c),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_image_tokens() {
        let text = "see [image-1] and [image-23], not [image-] or [image-x] or [image-4";
        let found: Vec<_> = image_tokens(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(found, ["[image-1]", "[image-23]"]);
        assert_eq!(image_numbers("[image-23] [image-1] [image-23]"), [1, 23]);
        assert_eq!(image_token(7), "[image-7]");
    }
}
