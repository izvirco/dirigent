//! Hand-rolled multiline text input: gpui has no widget, so this implements
//! `EntityInputHandler` (IME, keyboard) plus a custom `Element` that shapes the
//! text with wrapping, grows with content up to a share of the window, then scrolls
//! (wheel, and automatically to keep the cursor in view) and paints cursor/selection.
//! Mouse selection (double click = word, triple = all) keeps following the mouse once it
//! leaves the input, and every edit is undoable (typing a word is one step).
//!
//! Keys (`key_bindings`; "word" is ctrl, alt on macOS):
//! - moves (`Move`): left / right a character, up / down a visual row (a single-line input
//!   emits `Arrow` instead, for a list under it), word+left / right a word, home / end
//!   (cmd+left / right on macOS) the line's start / end, ctrl+home / end (cmd+up / down on
//!   macOS) the whole text's; with shift, each selects to there (shift+up / down too in a
//!   single-line input: there they reach its start / end);
//! - word+backspace / delete delete a word, stopping where word+left / right do
//!   (`word_boundary`); secondary+a selects all.
//!
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

actions!(input, [Backspace, Delete, BackspaceWord, DeleteWord, SelectAll, Paste, Cut, Copy, Undo, Redo, Newline, Submit, SubmitQueued]);

/// A motion key: moves the cursor `to` somewhere, or with `select` (shift) extends the
/// selection to there (`TextInput::move_cursor`).
#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = input, no_json)]
struct Move {
    to: To,
    select: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum To {
    /// A character.
    Left,
    Right,
    /// A visual row: -1 up, 1 down.
    Row(i32),
    /// Where `prev_word` / `next_word` say.
    WordLeft,
    WordRight,
    /// The `\n`-separated line's start / end (Home / End).
    LineStart,
    LineEnd,
    /// The whole text's.
    Start,
    End,
}

pub const KEY_CONTEXT: &str = "Input";
/// A multiline input grows with its text up to this share of the window height, then scrolls.
const MAX_HEIGHT_FRACTION: f32 = 0.7;
/// Room (design pixels, `theme::u`) kept for the composer's padding and chip row so the whole card stays within that share.
const CARD_CHROME: f32 = 90.;
/// Right inset (design pixels) reserved for the scrollbar so text never runs under it.
const BAR_GUTTER: f32 = 12.;
/// What a secret input shows for each character.
const MASK: char = '\u{2022}';
/// Edits of one kind closer together than this join the same undo step.
const UNDO_GROUP: Duration = Duration::from_secs(1);
const UNDO_DEPTH: usize = 200;

/// Emitted on ctrl+Enter (cmd on macOS), or shift+Enter with `queued` (Enter alone inserts a newline;
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

/// Where byte `offset` of `text` falls in its mask (a `MASK` per char).
fn mask_offset(text: &str, offset: usize) -> usize {
    text[..offset].chars().count() * MASK.len_utf8()
}

/// The byte offset of `text` at `offset` of its mask; inside a `MASK`, the char's start.
fn unmask_offset(text: &str, offset: usize) -> usize {
    text.char_indices().nth(offset / MASK.len_utf8()).map_or(text.len(), |(i, _)| i)
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
    /// Shows a `MASK` per character instead of the text (a key), and keeps it off the
    /// clipboards (`set_secret`).
    secret: bool,
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
    /// Horizontal scroll: a single-line input doesn't wrap, it scrolls sideways instead.
    scroll_x: Pixels,
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
    fn shape(text: &SharedString, placeholder: bool, wrap_width: Option<Pixels>, font: &Font, window: &Window) -> Self {
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
        let lines = window.text_system().shape_text(text.clone(), font_size, &runs, wrap_width, None).unwrap_or_default();
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
            secret: false,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            drag_anchor: None,
            scroll_y: px(0.),
            scroll_x: px(0.),
            follow_cursor: false,
            undo: vec![],
            redo: vec![],
            last_edit: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    /// Mask the text (an API key) or show it again. Either way the undo history goes: it
    /// may hold a secret.
    pub fn set_secret(&mut self, secret: bool, cx: &mut Context<Self>) {
        self.secret = secret;
        self.undo.clear();
        self.redo.clear();
        self.last_edit = None;
        cx.notify();
    }

    /// The text as painted: a secret's masked.
    fn shown(&self) -> SharedString {
        if self.secret { MASK.to_string().repeat(self.content.chars().count()).into() } else { self.content.clone() }
    }

    /// Where a byte offset of the text falls in `shown`.
    fn shown_offset(&self, offset: usize) -> usize {
        if self.secret { mask_offset(&self.content, offset) } else { offset }
    }

    /// The byte offset of the text at an offset of `shown`.
    fn text_offset(&self, offset: usize) -> usize {
        if self.secret { unmask_offset(&self.content, offset) } else { offset }
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
        // A secret keeps no history: undo would bring it back after the input is cleared,
        // and unmasked once the input shows text again.
        if self.secret {
            return;
        }
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

    /// Every motion key. Without shift a selection collapses: left / right to the end they
    /// head to, a word jump goes on from that end. Everything else moves from the cursor.
    fn move_cursor(&mut self, m: &Move, window: &mut Window, cx: &mut Context<Self>) {
        let c = self.cursor_offset();
        let collapse = !m.select && !self.selected_range.is_empty();
        let (left, right) = if m.select { (c, c) } else { (self.selected_range.start, self.selected_range.end) };
        let to = match m.to {
            To::Left if collapse => left,
            To::Right if collapse => right,
            To::Left => self.previous_boundary(c),
            To::Right => self.next_boundary(c),
            To::WordLeft => prev_word(&self.content, left),
            To::WordRight => next_word(&self.content, right),
            To::Row(rows) if self.single_line && !m.select => return cx.emit(Arrow(rows)),
            To::Row(rows) => {
                let Some(to) = self.vertical_target(rows) else { return };
                to
            }
            To::LineStart => line_start(&self.content, c),
            To::LineEnd => line_end(&self.content, c),
            To::Start => 0,
            To::End => self.content.len(),
        };
        if m.select { self.select_to(to, window, cx) } else { self.move_to(to, window, cx) }
    }

    /// Where the cursor lands `rows` visual rows down (up if negative) in the last layout;
    /// past the first / last row, at the text's start / end (all a single line has). None
    /// while the layout is stale.
    fn vertical_target(&self, rows: i32) -> Option<usize> {
        let layout = self.last_layout.as_ref().filter(|l| l.text == self.shown())?;
        let pos = layout.position(self.shown_offset(self.cursor_offset()));
        let target = point(pos.x, pos.y + layout.line_height * (rows as f32 + 0.5));
        Some(self.text_offset(layout.index(target)))
    }

    fn select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, window, cx);
        self.select_to(self.content.len(), window, cx)
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_to(self.previous_boundary(self.cursor_offset()), window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_to(self.next_boundary(self.cursor_offset()), window, cx)
    }

    fn backspace_word(&mut self, _: &BackspaceWord, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_to(prev_word(&self.content, self.cursor_offset()), window, cx)
    }

    fn delete_word(&mut self, _: &DeleteWord, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_to(next_word(&self.content, self.cursor_offset()), window, cx)
    }

    /// Delete the selection, or with none, from the cursor to `to` (either side of it).
    fn delete_to(&mut self, to: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let c = self.cursor_offset();
            if c == to {
                return;
            }
            // Not `select_to`: that would end the undo group of a run of deletes.
            self.selected_range = c.min(to)..c.max(to);
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
        if self.drag_anchor.take().is_some() && !self.selected_range.is_empty() && !self.secret {
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
        } else if !self.secret {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.selected_range.clone()].to_string()));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() && !self.secret {
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
            (Some(bounds), Some(layout)) if layout.text == self.shown() => {
                self.text_offset(layout.index(position - self.text_origin(bounds)))
            }
            _ => 0,
        }
    }

    /// Where the (possibly scrolled-off) top of the text sits in window coordinates.
    fn text_origin(&self, bounds: &Bounds<Pixels>) -> Point<Pixels> {
        bounds.origin - point(self.scroll_x, self.scroll_y)
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

/// Where a word move (ctrl+left / right) lands and a word delete (ctrl+backspace / delete)
/// reaches, before `from` in `text`.
fn prev_word(text: &str, from: usize) -> usize {
    word_boundary(text[..from].grapheme_indices(true).rev(), from)
}

/// Like `prev_word`, but after `from`.
fn next_word(text: &str, from: usize) -> usize {
    word_boundary(text[from..].grapheme_indices(true).map(|(i, g)| (from + i + g.len(), g)), from)
}

/// The start of the `\n`-separated line `at` is on (Home).
fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// The end of the `\n`-separated line `at` is on (End).
fn line_end(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |i| at + i)
}

/// How far a word move or delete reaches from `from`: over spaces, then over one run of word
/// characters or of punctuation. A line break is a word of its own, and spaces before it
/// stop there. `graphemes` walks away from `from` (backwards for ctrl+backspace), each with
/// the offset the cursor reaches once it is included.
fn word_boundary<'a>(graphemes: impl Iterator<Item = (usize, &'a str)>, from: usize) -> usize {
    #[derive(PartialEq, Clone, Copy)]
    enum Class {
        Space,
        Break,
        Word,
        Punct,
    }
    let class = |g: &str| match g.chars().next() {
        Some('\n' | '\r') => Class::Break,
        Some(c) if c.is_whitespace() => Class::Space,
        Some(c) if c.is_alphanumeric() || c == '_' => Class::Word,
        _ => Class::Punct,
    };
    let mut to = from;
    let mut run = None;
    for (at, g) in graphemes {
        let k = class(g);
        match run {
            None if k == Class::Space => {}
            None if k == Class::Break && to != from => break,
            None => run = Some(k),
            Some(r) if r == k && k != Class::Break => {}
            _ => break,
        }
        to = at;
    }
    to
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
        let layout = self.last_layout.as_ref().filter(|l| l.text == self.shown())?;
        let range = self.range_from_utf16(&range_utf16);
        let start = layout.position(self.shown_offset(range.start));
        let end = layout.position(self.shown_offset(range.end));
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
        let layout = self.last_layout.as_ref().filter(|l| l.text == self.shown())?;
        Some(self.offset_to_utf16(self.text_offset(layout.index(point - self.text_origin(&bounds)))))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct Prepaint {
    layout: Layout,
    cursor: Option<Bounds<Pixels>>,
    selection: Vec<select::Row>,
    scroll: Point<Pixels>,
}

/// Wrap width: single-line inputs don't wrap (they scroll sideways), multiline ones keep a
/// gutter for the scrollbar.
fn wrap_width(width: Pixels, single_line: bool) -> Option<Pixels> {
    (!single_line).then(|| (width - u(BAR_GUTTER)).max(px(1.)))
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
        // Only a single line (unwrapped) is ever wider than the input; the cursor's width
        // counts at its end.
        let width = layout.lines.iter().map(|l| l.width()).fold(px(0.), Pixels::max) + px(2.);
        let scroll = self.input.update(cx, |input, _| {
            let max = point((width - bounds.size.width).max(px(0.)), (layout.height() - bounds.size.height).max(px(0.)));
            if input.follow_cursor && !placeholder {
                input.follow_cursor = false;
                let p = layout.position(input.shown_offset(input.cursor_offset()));
                if p.y < input.scroll_y {
                    input.scroll_y = p.y;
                } else if p.y + lh > input.scroll_y + bounds.size.height {
                    input.scroll_y = p.y + lh - bounds.size.height;
                }
                if p.x < input.scroll_x {
                    input.scroll_x = p.x;
                } else if p.x + px(2.) > input.scroll_x + bounds.size.width {
                    input.scroll_x = p.x + px(2.) - bounds.size.width;
                }
            }
            input.scroll_y = input.scroll_y.clamp(px(0.), max.y);
            input.scroll_x = input.scroll_x.clamp(px(0.), max.x);
            point(input.scroll_x, input.scroll_y)
        });
        let input = self.input.read(cx);
        let o = bounds.origin - scroll;
        let (cursor, selection) = if placeholder {
            (Some(Bounds::new(bounds.origin, size(px(2.), lh))), vec![])
        } else if input.selected_range.is_empty() {
            let p = layout.position(input.shown_offset(input.cursor_offset()));
            (Some(Bounds::new(o + p, size(px(2.), lh))), vec![])
        } else {
            let lines = layout.lines.iter().map(|l| &***l);
            let range = input.shown_offset(input.selected_range.start)..input.shown_offset(input.selected_range.end);
            (None, select::selection_rows(lines, o, lh, range))
        };
        Prepaint { layout, cursor, selection, scroll }
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
            let mut y = -prepaint.scroll.y;
            for line in &prepaint.layout.lines {
                let h = line.size(lh).height;
                if y + h >= px(0.) && y <= visible {
                    let _ = line.paint(bounds.origin + point(-prepaint.scroll.x, y), lh, gpui::TextAlign::Left, None, window, cx);
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
            let thumb_y = (visible - thumb_h) * (f32::from(prepaint.scroll.y) / f32::from(content_h - visible));
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
    if input.content.is_empty() { (input.placeholder.clone(), true) } else { (input.shown(), false) }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::backspace_word))
            .on_action(cx.listener(Self::delete_word))
            .on_action(cx.listener(Self::move_cursor))
            .on_action(cx.listener(Self::select_all))
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

/// The shortcuts' modifier as hints name it: what gpui's `secondary-` binds, cmd on macOS
/// and ctrl elsewhere.
pub const MOD: &str = if cfg!(target_os = "macos") { "cmd" } else { "ctrl" };

/// Key bindings for the `Input` context. Registered once in `main`. `secondary-` is cmd on
/// macOS, ctrl elsewhere.
pub fn key_bindings() -> Vec<gpui::KeyBinding> {
    use gpui::KeyBinding;
    let c = Some(KEY_CONTEXT);
    let mac = cfg!(target_os = "macos");
    // A word at a time: alt on macOS, where cmd+backspace deletes the whole line.
    let word = if mac { "alt" } else { "ctrl" };
    // The motion keys; each with shift too, selecting.
    let mut motions: Vec<(String, To)> = vec![
        ("left".into(), To::Left),
        ("right".into(), To::Right),
        ("up".into(), To::Row(-1)),
        ("down".into(), To::Row(1)),
        (format!("{word}-left"), To::WordLeft),
        (format!("{word}-right"), To::WordRight),
        ("home".into(), To::LineStart),
        ("end".into(), To::LineEnd),
    ];
    if mac {
        // A MacBook has no Home / End keys; cmd+up / down take the whole text's start / end.
        motions.extend([
            ("cmd-left".into(), To::LineStart),
            ("cmd-right".into(), To::LineEnd),
            ("cmd-up".into(), To::Start),
            ("cmd-down".into(), To::End),
        ]);
    } else {
        motions.extend([("ctrl-home".into(), To::Start), ("ctrl-end".into(), To::End)]);
    }
    let mut bindings = vec![
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new(&format!("{word}-backspace"), BackspaceWord, c),
        KeyBinding::new(&format!("{word}-delete"), DeleteWord, c),
        KeyBinding::new("secondary-a", SelectAll, c),
        KeyBinding::new("secondary-v", Paste, c),
        KeyBinding::new("secondary-c", Copy, c),
        KeyBinding::new("secondary-x", Cut, c),
        KeyBinding::new("secondary-z", Undo, c),
        KeyBinding::new("secondary-shift-z", Redo, c),
        KeyBinding::new("secondary-y", Redo, c),
        KeyBinding::new("enter", Newline, c),
        KeyBinding::new("secondary-enter", Submit, c),
        KeyBinding::new("shift-enter", SubmitQueued, c),
    ];
    for (keys, to) in motions {
        bindings.push(KeyBinding::new(&keys, Move { to, select: false }, c));
        bindings.push(KeyBinding::new(&format!("shift-{keys}"), Move { to, select: true }, c));
    }
    bindings
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

    /// A secret's mask has a char per char of the text, whatever its byte length.
    #[test]
    fn mask_offsets() {
        let text = "a\u{e9}\u{20ac}b"; // 1 + 2 + 3 + 1 bytes
        let ends = [0, 1, 3, 6, 7];
        for (i, end) in ends.into_iter().enumerate() {
            assert_eq!(mask_offset(text, end), i * MASK.len_utf8());
            assert_eq!(unmask_offset(text, i * MASK.len_utf8()), end);
        }
        assert_eq!(unmask_offset(text, MASK.len_utf8() + 1), 1);
    }

    #[test]
    fn word_boundaries() {
        // What ctrl+backspace leaves of `text` with the cursor at its end, and ctrl+delete at its start.
        let back = |text: &str| text[..prev_word(text, text.len())].to_string();
        let fwd = |text: &str| text[next_word(text, 0)..].to_string();
        assert_eq!(back("hello world"), "hello ");
        assert_eq!(back("hello world  "), "hello ");
        assert_eq!(back("src/main.rs"), "src/main.");
        assert_eq!(back("wait..."), "wait");
        assert_eq!(back("snake_case"), "");
        assert_eq!(back("line\n"), "line");
        assert_eq!(back("line\n  "), "line\n");
        assert_eq!(back(""), "");
        assert_eq!(fwd("hello world"), " world");
        assert_eq!(fwd("  hello world"), " world");
        assert_eq!(fwd("\nline"), "line");
    }

    /// Where ctrl+left / ctrl+right stop, pressed over and over from the end / start.
    #[test]
    fn word_jumps() {
        let stops = |text: &str, step: fn(&str, usize) -> usize, from: usize| {
            std::iter::successors(Some(from), |&at| Some(step(text, at)).filter(|&next| next != at)).collect::<Vec<_>>()
        };
        let text = "fix src/main.rs  now\n  then";
        assert_eq!(stops(text, next_word, 0), [0, 3, 7, 8, 12, 13, 15, 20, 21, 27]);
        assert_eq!(stops(text, prev_word, text.len()), [27, 23, 21, 20, 17, 13, 12, 8, 7, 4, 0]);
        // Multi-byte graphemes count as one word character / one punctuation mark.
        assert_eq!(next_word("caf\u{e9} \u{2014} x", 0), 5);
        assert_eq!(prev_word("caf\u{e9} \u{2014}", 9), 6);
    }

    #[test]
    fn line_ends() {
        let text = "one\ntwo\n";
        assert_eq!((line_start(text, 0), line_end(text, 0)), (0, 3));
        assert_eq!((line_start(text, 3), line_end(text, 3)), (0, 3));
        assert_eq!((line_start(text, 5), line_end(text, 5)), (4, 7));
        assert_eq!((line_start(text, 8), line_end(text, 8)), (8, 8));
        assert_eq!((line_start("", 0), line_end("", 0)), (0, 0));
    }
}
