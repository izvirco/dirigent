//! Mouse text selection over the transcript and the work log. Their text is not one
//! buffer but a tree of divs with a `StyledText` per paragraph, so every selectable
//! paragraph is a `Block` keyed by its place in its document (`Key`, document order) and
//! a selection is a pair of (block, byte offset) positions; blocks between the two are
//! selected whole.
//!
//! Blocks register their layout when painted (only the visible ones are), which is what
//! hit testing and the highlight use; they register their text when constructed, which is
//! what copying uses: `Dirigent::copy_selection` constructs the selected range of items
//! again (no layout involved) and reads the texts back, so off-screen blocks copy too.
//! Dragging is driven from the root view (`Dirigent::on_mouse_move`), which also scrolls
//! the document the drag runs out of.
//!
//! A block may carry links (`Block::links`): a click on one that doesn't drag a selection
//! opens it.

use gpui::{
    App, Bounds, CursorStyle, ElementId, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Point, SharedString, StyledText, TextLayout, Window,
    WrappedLineLayout, point, prelude::*, px, rgba,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::Range;
use std::rc::Rc;
use unicode_segmentation::UnicodeSegmentation;

/// A scroll region with its own blocks; a selection never spans two.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Doc {
    Thread,
    Log,
}

/// A block's place in its document: the list item / log step it belongs to, then its
/// order within that. Sorting keys is document order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Key {
    pub doc: Doc,
    pub item: usize,
    pub seq: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Pos {
    pub key: Key,
    pub offset: usize,
}

#[derive(Default)]
struct State {
    /// What the press selected (a point, or a word / block on a double / triple click);
    /// the head extends it either way.
    anchor: Option<(Pos, Pos)>,
    head: Option<Pos>,
    /// The button is down: mouse moves extend the selection.
    dragging: bool,
    /// Blocks painted this frame.
    laid: Vec<(Key, TextLayout)>,
    /// Text of every block constructed since the last `collect`.
    texts: BTreeMap<Key, SharedString>,
    /// The link a plain click went down on; the release opens it if it's still on it.
    link: Option<SharedString>,
}

/// The one selection, shared by every block and the root view.
#[derive(Clone, Default)]
pub struct Selection(Rc<RefCell<State>>);

impl Selection {
    /// Start of a frame: the painted blocks are collected afresh.
    pub fn begin_frame(&self) {
        self.0.borrow_mut().laid.clear();
    }

    pub fn is_dragging(&self) -> bool {
        self.0.borrow().dragging
    }

    pub fn end_drag(&self) {
        self.0.borrow_mut().dragging = false;
    }

    /// Drop the selection; true if there was one.
    pub fn clear(&self) -> bool {
        let mut s = self.0.borrow_mut();
        let had = s.anchor.is_some();
        s.anchor = None;
        s.head = None;
        s.dragging = false;
        had
    }

    /// The selected span, ordered, if anything is selected.
    pub fn range(&self) -> Option<(Pos, Pos)> {
        let s = self.0.borrow();
        let ((a0, a1), h) = s.anchor.zip(s.head)?;
        let (start, end) = (a0.min(h), a1.max(h));
        (start < end).then_some((start, end))
    }

    /// The document being selected in, while something is.
    pub fn doc(&self) -> Option<Doc> {
        self.0.borrow().anchor.map(|(a, _)| a.key.doc)
    }

    /// A press on a block: one click puts the anchor there, two select the word, three
    /// the block. Shift extends the current selection instead.
    fn press(&self, key: Key, text: &str, offset: usize, clicks: usize, shift: bool) {
        let mut s = self.0.borrow_mut();
        let pos = Pos { key, offset };
        if !(shift && s.anchor.is_some_and(|(a, _)| a.key.doc == key.doc)) {
            let range = match clicks {
                1 => offset..offset,
                2 => word_at(text, offset),
                _ => 0..text.len(),
            };
            s.anchor = Some((Pos { key, offset: range.start }, Pos { key, offset: range.end }));
        }
        s.head = Some(pos);
        s.dragging = true;
    }

    /// Move the head to the position under `p`, among the blocks painted last frame.
    /// True if it moved.
    pub fn drag_to(&self, p: Point<Pixels>) -> bool {
        let mut s = self.0.borrow_mut();
        let Some(doc) = s.anchor.map(|(a, _)| a.key.doc) else { return false };
        let mut blocks: Vec<_> = s.laid.iter().filter(|(k, _)| k.doc == doc).collect();
        blocks.sort_by_key(|(k, _)| *k);
        let Some((last, _)) = blocks.last() else { return false };
        let mut head = Pos { key: *last, offset: blocks.last().map(|(_, l)| l.len()).unwrap_or(0) };
        for (key, layout) in &blocks {
            let b = layout.bounds();
            if p.y < b.top() {
                head = Pos { key: *key, offset: 0 };
                break;
            }
            if p.y <= b.bottom() {
                head = Pos { key: *key, offset: index_at(layout, p) };
                break;
            }
        }
        let moved = s.head != Some(head);
        s.head = Some(head);
        moved
    }

    /// The part of block `key` (of `len` bytes) that is selected.
    fn block_range(&self, key: Key, len: usize) -> Option<Range<usize>> {
        let (start, end) = self.range()?;
        if key < start.key || key > end.key {
            return None;
        }
        let a = if key == start.key { start.offset } else { 0 };
        let b = if key == end.key { end.offset } else { len };
        (a < b).then_some(a..b)
    }

    /// Text of every block constructed since the last `collect`, in document order.
    #[cfg(test)]
    pub fn texts(&self) -> Vec<String> {
        self.0.borrow().texts.values().map(|t| t.to_string()).collect()
    }

    /// Forget the collected texts; the caller constructs the selected items next.
    pub fn collect(&self) {
        self.0.borrow_mut().texts.clear();
    }

    /// The selected text, from the blocks constructed since `collect`, one line per block.
    pub fn text(&self) -> String {
        let Some((start, end)) = self.range() else { return String::new() };
        let s = self.0.borrow();
        let mut out = vec![];
        for (key, text) in s.texts.range(start.key..=end.key) {
            let a = if *key == start.key { start.offset } else { 0 };
            let b = if *key == end.key { end.offset } else { text.len() };
            out.push(text.get(a.min(b)..b).unwrap_or(""));
        }
        out.join("\n")
    }
}

/// Byte offset in `layout` closest to the window point `p`; rows clamp to their ends.
fn index_at(layout: &TextLayout, p: Point<Pixels>) -> usize {
    let (bounds, lh) = (layout.bounds(), layout.line_height());
    let mut y = bounds.top();
    let mut start = 0;
    for line in layout.line_layouts() {
        let h = line.size(lh).height;
        if p.y < y + h {
            let local = point((p.x - bounds.left()).max(Pixels::ZERO), p.y - y);
            return start + line.closest_index_for_position(local, lh).unwrap_or_else(|i| i);
        }
        y += h;
        start += line.len() + 1;
    }
    layout.len()
}

/// The word (unicode word boundaries, so a run of punctuation or spaces counts) at `offset`.
pub fn word_at(text: &str, offset: usize) -> Range<usize> {
    let mut last = 0..0;
    for (i, w) in text.split_word_bound_indices() {
        last = i..i + w.len();
        if offset < last.end {
            return last;
        }
    }
    last
}

/// Hands out keys to one item's blocks in construction order.
pub struct Keys {
    sel: Selection,
    next: Key,
}

impl Keys {
    pub fn new(sel: &Selection, doc: Doc, item: usize) -> Self {
        Self { sel: sel.clone(), next: Key { doc, item, seq: 0 } }
    }

    /// A styled paragraph; `plain` is the same text, for copying.
    pub fn styled(&mut self, text: StyledText, plain: SharedString) -> Block {
        let key = self.next;
        self.next.seq += 1;
        self.sel.0.borrow_mut().texts.insert(key, plain.clone());
        Block { sel: self.sel.clone(), key, text, plain, links: vec![] }
    }

    pub fn plain(&mut self, text: impl Into<SharedString>) -> Block {
        let text = text.into();
        self.styled(StyledText::new(text.clone()), text)
    }
}

/// One selectable paragraph: a `StyledText` that paints the selection under its glyphs
/// and takes mouse presses. Inherits the text style of its parent like any text.
pub struct Block {
    sel: Selection,
    key: Key,
    text: StyledText,
    plain: SharedString,
    links: Vec<(Range<usize>, SharedString)>,
}

impl Block {
    /// Clickable spans of the text and the URLs they open.
    pub fn links(mut self, links: Vec<(Range<usize>, SharedString)>) -> Self {
        self.links = links;
        self
    }
}

/// The URL of the link in `links` under the window point `p`, if any.
fn link_at(links: &[(Range<usize>, SharedString)], layout: &TextLayout, p: Point<Pixels>) -> Option<SharedString> {
    let ix = layout.index_for_position(p).ok()?;
    links.iter().find(|(r, _)| r.contains(&ix)).map(|(_, url)| url.clone())
}

impl IntoElement for Block {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for Block {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.text.request_layout(None, id, window, cx)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        self.text.prepaint(None, id, bounds, &mut (), window, cx);
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.text.layout().clone();
        let links = Rc::new(std::mem::take(&mut self.links));
        let hovered = link_at(&links, &layout, window.mouse_position());
        window.set_cursor_style(if hovered.is_some() { CursorStyle::PointingHand } else { CursorStyle::IBeam }, hitbox);
        if !links.is_empty() {
            // Repaint when the mouse crosses a link's edge, so the cursor follows.
            window.on_mouse_event({
                let (links, layout) = (links.clone(), layout.clone());
                move |ev: &MouseMoveEvent, phase, window, _| {
                    if phase.bubble() && link_at(&links, &layout, ev.position) != hovered {
                        window.refresh();
                    }
                }
            });
            window.on_mouse_event({
                let (sel, links, layout, hitbox) = (self.sel.clone(), links.clone(), layout.clone(), hitbox.clone());
                move |ev: &MouseUpEvent, phase, window, cx| {
                    if phase.bubble() && ev.button == MouseButton::Left && hitbox.is_hovered(window) {
                        let pressed = sel.0.borrow_mut().link.take();
                        if let Some(url) = pressed.filter(|url| link_at(&links, &layout, ev.position).as_ref() == Some(url))
                            && sel.range().is_none()
                        {
                            cx.open_url(&url);
                        }
                    }
                }
            });
        }
        window.on_mouse_event({
            let (sel, key, plain, hitbox, layout) = (self.sel.clone(), self.key, self.plain.clone(), hitbox.clone(), layout.clone());
            move |ev: &MouseDownEvent, phase, window, _| {
                if phase.bubble() && ev.button == MouseButton::Left && hitbox.is_hovered(window) {
                    sel.press(key, &plain, index_at(&layout, ev.position), ev.click_count, ev.modifiers.shift);
                    let plain_click = ev.click_count == 1 && !ev.modifiers.shift;
                    sel.0.borrow_mut().link = link_at(&links, &layout, ev.position).filter(|_| plain_click);
                    // Keeps the root from taking focus: whatever input was focused stays so.
                    window.prevent_default();
                    window.refresh();
                }
            }
        });
        self.sel.0.borrow_mut().laid.push((self.key, layout.clone()));

        let _ = layout.paint_background(window, cx);
        if let Some(range) = self.sel.block_range(self.key, layout.len()) {
            let (bounds, lh) = (layout.bounds(), layout.line_height());
            let lines = layout.line_layouts();
            paint_selection(&selection_rows(lines.iter().map(|l| &**l), bounds.origin, lh, range), lh, window);
        }
        let _ = layout.paint_foreground(window, cx);
    }
}

/// One wrapped row of a selection: its top and horizontal extent, in window coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Row {
    pub y: Pixels,
    pub x0: Pixels,
    pub x1: Pixels,
}

/// The rows `range` covers in text laid out from `origin` as `lines` (one per `\n`), each
/// only as wide as its selected text. Consecutive, `lh` apart. A blank line inside the
/// range is an empty row; one at its very end is not a row.
pub fn selection_rows<'a>(
    lines: impl IntoIterator<Item = &'a WrappedLineLayout>,
    origin: Point<Pixels>,
    lh: Pixels,
    range: Range<usize>,
) -> Vec<Row> {
    let mut rows = vec![];
    let mut y = origin.y;
    let mut line_start = 0;
    for line in lines {
        let glyphs = &line.unwrapped_layout;
        // Each row ends at a wrap boundary (its glyph's index and x), the last at the line's end.
        let row_ends = line
            .wrap_boundaries()
            .iter()
            .map(|b| {
                let g = &glyphs.runs[b.run_ix].glyphs[b.glyph_ix];
                (line_start + g.index, g.position.x)
            })
            .chain([(line_start + line.len(), glyphs.width)]);
        let (mut row_start, mut row_x) = (line_start, Pixels::ZERO);
        for (row_end, row_end_x) in row_ends {
            let (a, b) = (range.start.max(row_start), range.end.min(row_end));
            if a < b || (row_start == row_end && range.start <= row_start && range.end > row_end) {
                let x = |i: usize| origin.x + glyphs.x_for_index(i - line_start) - row_x;
                rows.push(Row { y, x0: x(a), x1: x(b) });
            }
            (row_start, row_x) = (row_end, row_end_x);
            y += lh;
        }
        line_start += line.len() + 1;
    }
    rows
}

/// Corner radius of the selection shape.
const RADIUS: f32 = 5.;
/// An empty row inside a selection (a blank line) still shows as a stub this wide: two
/// radii, enough to stay joined to its neighbours.
const STUB: f32 = 2. * RADIUS;

/// Paint `rows` as rounded shapes: one per run of rows that overlap enough to join, with
/// every corner rounded, including where a row's edge steps against the row above or
/// below. Rows sharing less than two radii horizontally (a selection starting late on one
/// line and ending early on the next) can't meet with rounded corners, so they're pieces
/// of their own, each rounded on its own.
pub fn paint_selection(rows: &[Row], lh: Pixels, window: &mut Window) {
    for piece in pieces(rows) {
        if let Some(path) = rounded_path(&outline(piece, lh)).and_then(|p| p.build().ok()) {
            window.paint_path(path, rgba(crate::theme::t().selection));
        }
    }
}

/// A row's right edge, at least `STUB` past its left one.
fn right(r: &Row) -> f32 {
    f32::from(r.x1).max(f32::from(r.x0) + STUB)
}

/// Runs of consecutive rows each overlapping the next by at least two radii.
fn pieces(rows: &[Row]) -> Vec<&[Row]> {
    rows.chunk_by(|a, b| right(a).min(right(b)) - f32::from(a.x0.max(b.x0)) >= 2. * RADIUS).collect()
}

/// Vertices of the rows' outline, clockwise: down the right side, back up the left.
/// Edges of neighbouring rows closer than two radii are snapped to the outer one
/// (`merge`): there is no room to round both corners of such a step, so the rows join.
fn outline(rows: &[Row], lh: Pixels) -> Vec<(f32, f32)> {
    let lh = f32::from(lh);
    let left = merge(rows.iter().map(|r| f32::from(r.x0)).collect(), f32::min);
    let right = merge(rows.iter().map(right).collect(), f32::max);
    let mut pts = vec![];
    for (r, &x1) in rows.iter().zip(&right) {
        pts.push((x1, f32::from(r.y)));
        pts.push((x1, f32::from(r.y) + lh));
    }
    for (r, &x0) in rows.iter().zip(&left).rev() {
        pts.push((x0, f32::from(r.y) + lh));
        pts.push((x0, f32::from(r.y)));
    }
    // Drop repeated and collinear points: rows sharing an edge form one straight run.
    let n = pts.len();
    let near = |a: f32, b: f32| (a - b).abs() < 0.5;
    (0..n)
        .filter(|&i| {
            let (p, v, nx) = (pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n]);
            let same = near(p.0, v.0) && near(p.1, v.1);
            let collinear = (near(p.0, v.0) && near(nx.0, v.0)) || (near(p.1, v.1) && near(nx.1, v.1));
            !same && !collinear
        })
        .map(|i| pts[i])
        .collect()
}

/// Runs of consecutive edges each within two radii of the next take their outermost
/// value (`outer` picks it), chaining: `[100, 104, 108]` all become 108.
fn merge(mut xs: Vec<f32>, outer: fn(f32, f32) -> f32) -> Vec<f32> {
    let mut i = 0;
    while i < xs.len() {
        let mut j = i;
        while j + 1 < xs.len() && (xs[j + 1] - xs[j]).abs() < 2. * RADIUS {
            j += 1;
        }
        let x = xs[i..=j].iter().copied().fold(xs[i], outer);
        xs[i..=j].fill(x);
        i = j + 1;
    }
    xs
}

/// A closed path through `pts`, every corner rounded by up to `RADIUS` (less where an edge
/// is shorter than two radii, so the arcs never cross).
fn rounded_path(pts: &[(f32, f32)]) -> Option<PathBuilder> {
    let n = pts.len();
    if n < 3 {
        return None;
    }
    let mut path = PathBuilder::fill();
    for i in 0..n {
        let (p, v, nx) = (pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n]);
        let len = |a: (f32, f32)| ((a.0 - v.0).powi(2) + (a.1 - v.1).powi(2)).sqrt();
        let (d1, d2) = (len(p), len(nx));
        let r = RADIUS.min(d1 / 2.).min(d2 / 2.);
        let towards = |a: (f32, f32), d: f32| point(px(v.0 + (a.0 - v.0) / d * r), px(v.1 + (a.1 - v.1) / d * r));
        let (from, to) = (towards(p, d1), towards(nx, d2));
        if i == 0 {
            path.move_to(from);
        } else {
            path.line_to(from);
        }
        path.curve_to(to, point(px(v.0), px(v.1)));
    }
    path.close();
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(item: usize, seq: u32) -> Key {
        Key { doc: Doc::Thread, item, seq }
    }

    #[test]
    fn outline_rounds_only_real_corners() {
        let row = |y: f32, x0: f32, x1: f32| Row { y: px(y), x0: px(x0), x1: px(x1) };
        // Two rows with the same edges are one rectangle: four corners.
        assert_eq!(outline(&[row(0., 10., 100.), row(20., 10., 100.)], px(20.)).len(), 4);
        // A last row ending early adds a step on the right: two more corners.
        assert_eq!(outline(&[row(0., 10., 100.), row(20., 10., 60.)], px(20.)).len(), 6);
        // A blank row keeps the shape whole as a stub.
        let blank = [row(0., 10., 100.), row(20., 10., 10.), row(40., 10., 100.)];
        assert_eq!(pieces(&blank).len(), 1);
        assert!(outline(&blank, px(20.)).iter().any(|&(x, _)| (x - 20.).abs() < 0.01));
        // Rows that don't share two radii horizontally are separate pieces.
        let apart = [row(0., 95., 870.), row(20., 0., 65.)];
        assert_eq!(pieces(&apart).len(), 2);
        assert_eq!(pieces(&[row(0., 60., 870.), row(20., 0., 65.)]).len(), 2);
        assert_eq!(pieces(&[row(0., 55., 870.), row(20., 0., 65.)]).len(), 1);
        // Edges within two radii of each other join at the outer one, chaining.
        assert_eq!(outline(&[row(0., 10., 100.), row(20., 10., 104.), row(40., 10., 108.)], px(20.)).len(), 4);
        assert_eq!(merge(vec![100., 104., 108., 150.], f32::max), vec![108., 108., 108., 150.]);
        assert_eq!(merge(vec![20., 14., 60.], f32::min), vec![14., 14., 60.]);
        assert!(rounded_path(&outline(&[], px(20.))).is_none());
    }

    #[test]
    fn word_boundaries() {
        let t = "hello, wörld  end";
        assert_eq!(&t[word_at(t, 0)], "hello");
        assert_eq!(&t[word_at(t, 4)], "hello");
        assert_eq!(&t[word_at(t, 5)], ",");
        assert_eq!(&t[word_at(t, 8)], "wörld");
        assert_eq!(&t[word_at(t, 13)], "  ");
        assert_eq!(&t[word_at(t, 17)], "end");
        assert_eq!(word_at("", 0), 0..0);
    }

    #[test]
    fn text_spans_blocks_in_order() {
        let sel = Selection::default();
        let mut keys = Keys::new(&sel, Doc::Thread, 1);
        keys.plain("first block");
        keys.plain("second");
        Keys::new(&sel, Doc::Thread, 2).plain("third block");
        // Log blocks never take part in a thread selection.
        Keys::new(&sel, Doc::Log, 1).plain("log");
        sel.press(key(1, 0), "first block", 6, 1, false);
        sel.0.borrow_mut().head = Some(Pos { key: key(2, 0), offset: 5 });
        assert_eq!(sel.text(), "block\nsecond\nthird");
        // Reversed heads read the same.
        sel.0.borrow_mut().anchor = Some((Pos { key: key(2, 0), offset: 5 }, Pos { key: key(2, 0), offset: 5 }));
        sel.0.borrow_mut().head = Some(Pos { key: key(1, 0), offset: 6 });
        assert_eq!(sel.text(), "block\nsecond\nthird");
        assert_eq!(sel.block_range(key(1, 1), 6), Some(0..6));
        assert_eq!(sel.block_range(key(0, 0), 6), None);
    }

    #[test]
    fn double_click_selects_word_and_shift_extends() {
        let sel = Selection::default();
        Keys::new(&sel, Doc::Thread, 0).plain("one two three");
        sel.press(key(0, 0), "one two three", 5, 2, false);
        assert_eq!(sel.text(), "two");
        sel.end_drag();
        sel.press(key(0, 0), "one two three", 13, 1, true);
        assert_eq!(sel.text(), "two three");
        assert!(sel.clear());
        assert!(sel.range().is_none());
    }
}
