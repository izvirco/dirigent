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

#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = dirigent, no_json)]
pub struct OpenAppLink {
    pub url: SharedString,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Doc {
    Thread,
    Log,
    Changelog,
}

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
    anchor: Option<Range<Pos>>,
    head: Option<Pos>,
    dragging: bool,
    laid: Vec<(Key, TextLayout)>,
    texts: BTreeMap<Key, SharedString>,
    link: Option<SharedString>,
}

#[derive(Clone, Default)]
pub struct Selection(Rc<RefCell<State>>);

impl Selection {
    pub fn begin_frame(&self) {
        self.0.borrow_mut().laid.clear();
    }

    pub fn is_dragging(&self) -> bool {
        self.0.borrow().dragging
    }

    pub fn end_drag(&self) {
        self.0.borrow_mut().dragging = false;
    }

    pub fn clear(&self) -> bool {
        let mut s = self.0.borrow_mut();
        let had = s.anchor.is_some();
        s.anchor = None;
        s.head = None;
        s.dragging = false;
        had
    }

    pub fn range(&self) -> Option<Range<Pos>> {
        let s = self.0.borrow();
        let (anchor, head) = s.anchor.clone().zip(s.head)?;
        Some(anchor.start.min(head)..anchor.end.max(head)).filter(|r| !r.is_empty())
    }

    pub fn doc(&self) -> Option<Doc> {
        self.0.borrow().anchor.as_ref().map(|a| a.start.key.doc)
    }

    fn press(&self, key: Key, text: &str, offset: usize, clicks: usize, shift: bool) {
        let mut s = self.0.borrow_mut();
        let pos = Pos { key, offset };
        if !(shift && s.anchor.as_ref().is_some_and(|a| a.start.key.doc == key.doc)) {
            let range = match clicks {
                1 => offset..offset,
                2 => word_at(text, offset),
                _ => 0..text.len(),
            };
            s.anchor = Some(Pos { key, offset: range.start }..Pos { key, offset: range.end });
        }
        s.head = Some(pos);
        s.dragging = true;
    }

    pub fn drag_to(&self, p: Point<Pixels>) -> bool {
        let mut s = self.0.borrow_mut();
        let Some(doc) = s.anchor.as_ref().map(|a| a.start.key.doc) else { return false };
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

    fn block_range(&self, key: Key, len: usize) -> Option<Range<usize>> {
        let range = self.range()?;
        if key < range.start.key || key > range.end.key {
            return None;
        }
        let a = if key == range.start.key { range.start.offset } else { 0 };
        let b = if key == range.end.key { range.end.offset } else { len };
        (a < b).then_some(a..b)
    }

    #[cfg(test)]
    pub fn texts(&self) -> Vec<String> {
        self.0.borrow().texts.values().map(|t| t.to_string()).collect()
    }

    pub fn text(&self, construct: impl FnOnce(&Range<Pos>)) -> String {
        let Some(range) = self.range() else { return String::new() };
        self.0.borrow_mut().texts.clear();
        construct(&range);
        let s = self.0.borrow();
        let mut out = vec![];
        for (key, text) in s.texts.range(range.start.key..=range.end.key) {
            let a = if *key == range.start.key { range.start.offset } else { 0 };
            let b = if *key == range.end.key { range.end.offset } else { text.len() };
            out.push(text.get(a.min(b)..b).unwrap_or(""));
        }
        out.join("\n")
    }
}

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

pub struct Keys {
    sel: Selection,
    next: Key,
}

impl Keys {
    pub fn new(sel: &Selection, doc: Doc, item: usize) -> Self {
        Self { sel: sel.clone(), next: Key { doc, item, seq: 0 } }
    }

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

pub struct Block {
    sel: Selection,
    key: Key,
    text: StyledText,
    plain: SharedString,
    links: Vec<(Range<usize>, SharedString)>,
}

impl Block {
    pub fn key(&self) -> Key {
        self.key
    }

    pub fn links(mut self, links: Vec<(Range<usize>, SharedString)>) -> Self {
        self.links = links;
        self
    }
}

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
                            if url.starts_with("dirigent://") {
                                window.dispatch_action(Box::new(OpenAppLink { url }), cx);
                            } else {
                                cx.open_url(&url);
                            }
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Row {
    pub y: Pixels,
    pub x0: Pixels,
    pub x1: Pixels,
}

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

const RADIUS: f32 = 5.;
const STUB: f32 = 2. * RADIUS;

pub fn paint_selection(rows: &[Row], lh: Pixels, window: &mut Window) {
    for piece in pieces(rows) {
        if let Some(path) = rounded_path(&outline(piece, lh)).and_then(|p| p.build().ok()) {
            window.paint_path(path, rgba(crate::theme::t().selection));
        }
    }
}

fn right(r: &Row) -> f32 {
    f32::from(r.x1).max(f32::from(r.x0) + STUB)
}

fn pieces(rows: &[Row]) -> Vec<&[Row]> {
    rows.chunk_by(|a, b| right(a).min(right(b)) - f32::from(a.x0.max(b.x0)) >= 2. * RADIUS).collect()
}

fn outline(rows: &[Row], lh: Pixels) -> Vec<(f32, f32)> {
    let lh = f32::from(lh);
    let left = snap_near_edges_outward(rows.iter().map(|r| f32::from(r.x0)).collect(), f32::min);
    let right = snap_near_edges_outward(rows.iter().map(right).collect(), f32::max);
    let mut pts = vec![];
    for (r, &x1) in rows.iter().zip(&right) {
        pts.push((x1, f32::from(r.y)));
        pts.push((x1, f32::from(r.y) + lh));
    }
    for (r, &x0) in rows.iter().zip(&left).rev() {
        pts.push((x0, f32::from(r.y) + lh));
        pts.push((x0, f32::from(r.y)));
    }
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

fn snap_near_edges_outward(mut xs: Vec<f32>, outer: fn(f32, f32) -> f32) -> Vec<f32> {
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
        assert_eq!(outline(&[row(0., 10., 100.), row(20., 10., 100.)], px(20.)).len(), 4);
        assert_eq!(outline(&[row(0., 10., 100.), row(20., 10., 60.)], px(20.)).len(), 6);
        let blank = [row(0., 10., 100.), row(20., 10., 10.), row(40., 10., 100.)];
        assert_eq!(pieces(&blank).len(), 1);
        assert!(outline(&blank, px(20.)).iter().any(|&(x, _)| (x - 20.).abs() < 0.01));
        let apart = [row(0., 95., 870.), row(20., 0., 65.)];
        assert_eq!(pieces(&apart).len(), 2);
        assert_eq!(pieces(&[row(0., 60., 870.), row(20., 0., 65.)]).len(), 2);
        assert_eq!(pieces(&[row(0., 55., 870.), row(20., 0., 65.)]).len(), 1);
        assert_eq!(outline(&[row(0., 10., 100.), row(20., 10., 104.), row(40., 10., 108.)], px(20.)).len(), 4);
        assert_eq!(snap_near_edges_outward(vec![100., 104., 108., 150.], f32::max), vec![108., 108., 108., 150.]);
        assert_eq!(snap_near_edges_outward(vec![20., 14., 60.], f32::min), vec![14., 14., 60.]);
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
        let construct = |_: &Range<Pos>| {
            let mut keys = Keys::new(&sel, Doc::Thread, 1);
            keys.plain("first block");
            keys.plain("second");
            Keys::new(&sel, Doc::Thread, 2).plain("third block");
            Keys::new(&sel, Doc::Log, 1).plain("log");
        };
        sel.press(key(1, 0), "first block", 6, 1, false);
        sel.0.borrow_mut().head = Some(Pos { key: key(2, 0), offset: 5 });
        assert_eq!(sel.text(construct), "block\nsecond\nthird");
        let end = Pos { key: key(2, 0), offset: 5 };
        sel.0.borrow_mut().anchor = Some(end..end);
        sel.0.borrow_mut().head = Some(Pos { key: key(1, 0), offset: 6 });
        assert_eq!(sel.text(construct), "block\nsecond\nthird");
        assert_eq!(sel.block_range(key(1, 1), 6), Some(0..6));
        assert_eq!(sel.block_range(key(0, 0), 6), None);
    }

    #[test]
    fn double_click_selects_word_and_shift_extends() {
        let sel = Selection::default();
        let construct = |_: &Range<Pos>| {
            Keys::new(&sel, Doc::Thread, 0).plain("one two three");
        };
        sel.press(key(0, 0), "one two three", 5, 2, false);
        assert_eq!(sel.text(construct), "two");
        sel.end_drag();
        sel.press(key(0, 0), "one two three", 13, 1, true);
        assert_eq!(sel.text(construct), "two three");
        assert!(sel.clear());
        assert!(sel.range().is_none());
    }
}
