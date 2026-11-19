use crate::app::{Dirigent, OpenThread, Pane};
use crate::theme::{self, text, u};
use crate::view::Item;
use gpui::{
    Anchor, AnyElement, Bounds, Context, Hsla, ListState, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    WeakEntity, Window, anchored, canvas, deferred, div, fill, point, prelude::*, px, rgba, size,
};
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Instant;

pub const WIDTH: f32 = 28.;
const INSET: f32 = 8.;
const LENGTH: f32 = 14.;
const PAD: f32 = 14.;
const THICK: f32 = 2.;
const PITCH: f32 = 7.;
const BAND: usize = 32;
const GAP: f32 = 12.;
const FADE: f32 = 21.;
const STACKS: f32 = 1. / 3.;
const SCRUB: f32 = 6.;
const SHRINK: f32 = 0.4;
const DIM: f32 = 0.4;
const TAU: f32 = 0.09;
const NOISE: f32 = 0.01;
const PREVIEW_CHARS: usize = 180;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    User,
    Assistant,
    Compaction,
}

impl Kind {
    fn color(self) -> u32 {
        match self {
            Kind::User => theme::t().accent_ink,
            Kind::Assistant | Kind::Compaction => theme::t().fg_1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::User => "you",
            Kind::Assistant => "reply",
            Kind::Compaction => "context compacted",
        }
    }
}

#[derive(Clone, Copy)]
struct Mark {
    item: usize,
    kind: Kind,
    pinned: bool,
}

#[derive(Default)]
pub struct Ruler {
    tweens: Vec<Tween>,
    scroll: Option<f32>,
    target: f32,
    grid: Grid,
    layout: Layout,
    last_frame: Option<Instant>,
    hover: Option<Hover>,
    scrub: Option<usize>,
}

impl Ruler {
    fn hit(&self, p: Point<Pixels>) -> Option<Hover> {
        self.grid.bounds.contains(&p).then(|| self.layout.hit(f32::from(p.y - self.grid.bounds.top()))).flatten()
    }

    fn shown(&self) -> Option<Hover> {
        self.scrub.map(Hover::Mark).or(self.hover)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Hover {
    Mark(usize),
    Gap(usize),
}

#[derive(Default, Clone, Copy)]
struct Grid {
    bounds: Bounds<Pixels>,
    scale: f32,
}

impl Grid {
    fn snap(&self, v: f32) -> f32 {
        if self.scale > 0. { (v * self.scale).round() / self.scale } else { v.round() }
    }
}

#[derive(Default, Clone, Copy)]
struct Metrics {
    pad: f32,
    pitch: f32,
    thick: f32,
    gap: f32,
    fade: f32,
}

impl Metrics {
    fn new(grid: &Grid) -> Self {
        let z = theme::zoom();
        Self { pad: PAD * z, pitch: grid.snap(PITCH * z), thick: THICK * z, gap: GAP * z, fade: FADE * z }
    }

    fn clear(&self) -> f32 {
        self.thick + self.gap + self.fade
    }
}

#[derive(Clone, Copy)]
struct Frame {
    m: Metrics,
    band: (f32, f32),
    origin: f32,
    max_scroll: f32,
    stack_pitch: f32,
}

impl Frame {
    fn new(m: Metrics, h: f32, n: usize, sticky: usize) -> Self {
        let stacked = sticky.saturating_sub(2) as f32;
        let stack = (stacked * m.pitch).min(h * STACKS / 2.);
        let len = ((BAND - 1) as f32 * m.pitch).min(h - 2. * (m.pad + stack + m.clear())).max(0.);
        let band = ((h - len) / 2., (h + len) / 2.);
        let span = n.saturating_sub(1) as f32 * m.pitch;
        Self {
            m,
            band,
            origin: ((h - span) / 2.).max(band.0),
            max_scroll: ((span - len) / m.pitch).max(0.),
            stack_pitch: ((band.0 - m.clear() - m.pad) / stacked.max(1.)).clamp(0., m.pitch),
        }
    }

    fn inside(&self, y: f32) -> f32 {
        let ramp = |d: f32| ((d + NOISE) / self.m.fade).min(1.);
        ramp(y - (self.band.0 - self.m.fade)).min(ramp(self.band.1 + self.m.fade - y)).max(0.)
    }
}

#[derive(Default)]
struct Layout {
    m: Metrics,
    marks: Vec<Placed>,
    band: (f32, f32),
    gaps: [Option<(f32, f32)>; 2],
    beyond: [usize; 2],
}

#[derive(Clone, Copy)]
struct Placed {
    mark: usize,
    y: f32,
    alpha: f32,
    stuck: bool,
}

fn layout(f: &Frame, n: usize, sticky: &[usize], scroll: f32) -> Layout {
    let (m, band) = (f.m, f.band);
    let natural = |i: usize| f.origin + (i as f32 - scroll) * m.pitch;
    let (inner0, inner1) = (band.0 - m.clear(), band.1 + m.clear());
    let mut ys: Vec<f32> = sticky.iter().map(|&i| natural(i)).collect();
    let mut slot = inner0;
    for y in ys.iter_mut().rev() {
        *y = y.max(slot);
        slot = inner0.min(*y - f.stack_pitch);
    }
    let mut slot = inner1;
    for y in ys.iter_mut() {
        *y = y.min(slot);
        slot = inner1.max(*y + f.stack_pitch);
    }
    let mut marks: Vec<Placed> =
        sticky.iter().zip(ys).map(|(&i, y)| Placed { mark: i, y, alpha: 1., stuck: (y - natural(i)).abs() > NOISE }).collect();
    let mut sticky = sticky.iter().peekable();
    for i in 0..n {
        if sticky.next_if_eq(&&i).is_some() {
            continue;
        }
        let y = natural(i);
        let alpha = f.inside(y);
        if alpha > 0. {
            marks.push(Placed { mark: i, y, alpha, stuck: false });
        }
    }
    let mut layout = Layout { m, marks, band, gaps: [None; 2], beyond: [0, 0] };
    let banded = || layout.marks.iter().filter(|p| layout.in_band(p)).map(|p| p.mark);
    if let (Some(first), Some(last)) = (banded().min(), banded().max()) {
        layout.beyond = [first, n - 1 - last];
    }
    let gaps = [(inner0 + m.thick, band.0), (band.1 + m.thick, inner1)];
    layout.gaps = [0, 1].map(|side| (layout.beyond[side] > 0).then_some(gaps[side]));
    layout
}

impl Layout {
    fn y(&self, mark: usize) -> Option<f32> {
        self.marks.iter().find(|p| p.mark == mark).map(|p| p.y)
    }

    fn in_band(&self, p: &Placed) -> bool {
        !p.stuck && p.y >= self.band.0 - NOISE && p.y <= self.band.1 + NOISE
    }

    fn nearest(&self, y: f32, keep: impl Fn(&Placed) -> bool) -> Option<&Placed> {
        let dist = |p: &Placed| (p.y + self.m.thick / 2. - y).abs();
        self.marks.iter().filter(|p| keep(p)).min_by(|a, b| dist(a).total_cmp(&dist(b)))
    }

    fn scrub_target(&self, y: f32) -> Option<usize> {
        self.nearest(y.clamp(self.band.0, self.band.1.max(self.band.0)), |p| self.in_band(p)).map(|p| p.mark)
    }

    fn hit(&self, y: f32) -> Option<Hover> {
        let mark = self.nearest(y, |p| p.alpha >= 1.).filter(|p| (p.y + self.m.thick / 2. - y).abs() <= self.m.pitch / 2.);
        if let Some(p) = mark {
            return Some(Hover::Mark(p.mark));
        }
        let gap = self.gaps.iter().position(|g| g.is_some_and(|(a, b)| y >= a && y < b));
        gap.map(Hover::Gap)
    }
}

fn keep_in_view(f: &Frame, from: f32, hold: bool, a: usize, b: usize) -> f32 {
    let most = a as f32 - (f.band.0 - f.origin) / f.m.pitch;
    let least = b as f32 - (f.band.1 - f.origin) / f.m.pitch;
    let to = if hold { from.max(least) } else { (least + most) / 2. };
    to.min(most).clamp(0., f.max_scroll)
}

pub type State = Rc<RefCell<Ruler>>;

#[derive(Default, Clone, Copy)]
struct Tween {
    grow: f32,
    on: f32,
}

impl Tween {
    fn step(&mut self, k: f32, on: f32) -> bool {
        let mut settled = true;
        for (v, target) in [(&mut self.grow, 1.), (&mut self.on, on)] {
            *v += (target - *v) * k;
            if (target - *v).abs() < 0.01 {
                *v = target;
            } else {
                settled = false;
            }
        }
        settled
    }
}

fn marks(t: &OpenThread) -> Vec<Mark> {
    t.timeline
        .items
        .iter()
        .enumerate()
        .filter_map(|(item, i)| {
            let kind = match i {
                Item::User { .. } => Kind::User,
                Item::Assistant(_) => Kind::Assistant,
                Item::Compaction => Kind::Compaction,
                _ => return None,
            };
            Some(Mark { item, kind, pinned: t.is_pinned(item) })
        })
        .collect()
}

fn visible_items(list: &ListState) -> Range<usize> {
    let vp = list.viewport_bounds();
    let first = list.logical_scroll_top().item_ix;
    let (mut start, mut end) = (None, first);
    for ix in first.. {
        let Some(b) = list.bounds_for_item(ix) else { break };
        if b.top() >= vp.bottom() {
            break;
        }
        let overlap = b.bottom().min(vp.bottom()) - b.top().max(vp.top());
        if overlap >= (b.size.height / 2.).min(u(40.)) {
            start.get_or_insert(ix);
            end = ix + 1;
        }
    }
    start.unwrap_or(end)..end
}

pub fn render(this: &mut Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let Pane::Thread(t) = &this.pane else { return div().into_any() };
    let marks: Rc<[Mark]> = marks(t).into();
    let n = marks.len();
    let sticky: Rc<[usize]> = (0..n).filter(|&i| i == 0 || i + 1 == n || marks[i].pinned).collect();
    let state = t.ruler.clone();
    let card = {
        let s = state.borrow();
        match s.shown() {
            Some(Hover::Mark(i)) if i < n => s.layout.y(i).map(|y| mark_card(&marks[i], t, &s.grid, y)),
            Some(Hover::Gap(side)) => gap_card(&s, side),
            _ => None,
        }
    };

    let (list, paint_marks, paint_state) = (t.list.clone(), marks.clone(), state.clone());
    let this = cx.entity().downgrade();
    let strip = canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
            let mut s = paint_state.borrow_mut();
            s.grid = Grid { bounds, scale: window.scale_factor() };
            let grid = s.grid;
            let f = Frame::new(Metrics::new(&grid), f32::from(bounds.size.height), n, sticky.len());
            let now = Instant::now();
            let dt = s.last_frame.map_or(0., |l| now.duration_since(l).as_secs_f32()).min(1. / 30.);
            s.last_frame = Some(now);
            let k = if cx.reduce_motion() { 1. } else { 1. - (-dt / TAU).exp() };

            let visible = visible_items(&list);
            let first = paint_marks.partition_point(|m| m.item < visible.start).min(n.saturating_sub(1));
            let last = paint_marks.partition_point(|m| m.item < visible.end).saturating_sub(1).max(first);
            let pointer = f32::from(window.mouse_position().y - bounds.top());
            let (top, bottom) = s.layout.band;
            let past = if s.scrub.is_none() {
                0.
            } else if pointer < top {
                pointer - top
            } else {
                (pointer - bottom).max(0.)
            };
            let hold = bounds.contains(&window.mouse_position());
            let target =
                if s.scrub.is_some() { s.target + past / f.m.pitch * SCRUB * dt } else { keep_in_view(&f, s.target, hold, first, last) };
            s.target = target.clamp(0., f.max_scroll);
            let scroll = s.scroll.map_or(s.target, |v| v + (s.target - v) * k);
            let scroll = if (s.target - scroll).abs() < 0.01 { s.target } else { scroll };
            s.scroll = Some(scroll);
            let mut settled = scroll == s.target && past == 0.;

            let lay = layout(&f, n, &sticky, scroll);
            s.tweens.resize(n, Tween::default());
            for (tw, mark) in s.tweens.iter_mut().zip(paint_marks.iter()) {
                settled &= tw.step(k, if visible.contains(&mark.item) { 1. } else { 0. });
            }
            let z = theme::zoom();
            let x = px(grid.snap(f32::from(bounds.left()) + INSET * z));
            let shown = s.shown();
            for p in &lay.marks {
                let (mark, tw) = (paint_marks[p.mark], s.tweens[p.mark]);
                let hovered = shown == Some(Hover::Mark(p.mark));
                let w = LENGTH * z * (1. - SHRINK * tw.on) * tw.grow;
                let color: Hsla = rgba(if mark.pinned { theme::t().warn } else { mark.kind.color() }).into();
                let lit = if mark.pinned { tw.on.max(f.inside(p.y)) } else { tw.on };
                let color = if hovered { rgba(theme::t().fg).into() } else { color.opacity((DIM + (1. - DIM) * lit) * p.alpha) };
                let y = px(grid.snap(f32::from(bounds.top()) + p.y));
                let h = px(grid.snap(z * if hovered { THICK + 1. } else { THICK }));
                if mark.kind == Kind::Compaction {
                    let mut dx = 0.;
                    while dx < w {
                        window.paint_quad(fill(Bounds::new(point(x + px(dx), y), size(px((w - dx).min(2. * z)), h)), color));
                        dx += 4. * z;
                    }
                } else {
                    window.paint_quad(fill(Bounds::new(point(x, y), size(px(w), h)), color));
                }
            }
            if let Some(scrubbed) = s.scrub
                && let Some(i) = lay.scrub_target(pointer)
                && i != scrubbed
            {
                s.scrub = Some(i);
                let (this, item) = (this.clone(), paint_marks[i].item);
                cx.defer(move |cx| {
                    this.update(cx, |this, cx| this.jump_to(item, cx)).ok();
                });
            }
            s.layout = lay;
            if !settled {
                window.request_animation_frame();
            }
            drop(s);
            scrub_across_window(window, paint_state, this);
        },
    )
    .size_full();

    let hover_state = state.clone();
    let down_state = state.clone();
    div()
        .id("ruler")
        .absolute()
        .left_0()
        .top_0()
        .h_full()
        .w(u(WIDTH))
        .when(n > 0, |d| d.cursor_pointer())
        .on_mouse_move(cx.listener(move |_, ev: &MouseMoveEvent, _, cx| {
            let mut s = hover_state.borrow_mut();
            let hit = s.hit(ev.position);
            if s.scrub.is_none() && s.hover != hit {
                s.hover = hit;
                cx.notify();
            }
        }))
        .on_hover(cx.listener(move |_, hovered, _, cx| {
            let mut s = state.borrow_mut();
            if !*hovered && s.scrub.is_none() {
                s.hover = None;
                cx.notify();
            }
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                // Keeps the root from taking focus: whatever input was focused stays so.
                window.prevent_default();
                let mut s = down_state.borrow_mut();
                match s.hit(ev.position) {
                    Some(Hover::Mark(i)) => {
                        s.scrub = s.layout.marks.iter().any(|p| p.mark == i && s.layout.in_band(p)).then_some(i);
                        drop(s);
                        this.jump_to(marks[i].item, cx);
                    }
                    Some(Hover::Gap(_)) => {
                        s.scrub = s.layout.scrub_target(f32::from(ev.position.y - s.grid.bounds.top()));
                        cx.notify();
                    }
                    None => {}
                }
            }),
        )
        .child(strip)
        .children(card)
        .into_any()
}

fn scrub_across_window(window: &mut Window, state: State, this: WeakEntity<Dirigent>) {
    window.on_mouse_event({
        let state = state.clone();
        move |ev: &MouseMoveEvent, phase, window, _| {
            let mut s = state.borrow_mut();
            if phase.bubble() && s.scrub.is_some() {
                if ev.pressed_button != Some(MouseButton::Left) {
                    s.hover = s.scrub.take().map(Hover::Mark);
                }
                window.refresh();
            }
        }
    });
    window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
        let mut s = state.borrow_mut();
        if phase.bubble() && ev.button == MouseButton::Left && s.scrub.is_some() {
            s.scrub = None;
            s.hover = s.hit(ev.position);
            drop(s);
            this.update(cx, |_, cx| cx.notify()).ok();
        }
    });
}

fn mark_card(m: &Mark, t: &OpenThread, grid: &Grid, y: f32) -> AnyElement {
    let text = match &t.timeline.items[m.item] {
        Item::User { text, .. } => crate::view::split_image_refs(text).0,
        Item::Assistant(text) => text.as_str(),
        _ => "",
    };
    let mut preview: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(PREVIEW_CHARS).collect();
    if preview.chars().count() == PREVIEW_CHARS {
        preview.push('…');
    }
    let label = if m.pinned { format!("pinned · {}", m.kind.label()) } else { m.kind.label().to_owned() };
    let color = if m.pinned { theme::t().warn } else { theme::t().fg_2 };
    card(grid, y, Some((label, color)), Some(preview).filter(|p| !p.is_empty()))
}

fn gap_card(s: &Ruler, side: usize) -> Option<AnyElement> {
    let (top, bottom) = s.layout.gaps[side]?;
    let count = s.layout.beyond[side];
    let text = format!("{count} {} message{}", ["earlier", "later"][side], if count == 1 { "" } else { "s" });
    (count > 0).then(|| card(&s.grid, (top + bottom) / 2., None, Some(text)))
}

fn card(grid: &Grid, y: f32, label: Option<(String, u32)>, body: Option<String>) -> AnyElement {
    let card = div()
        .flex()
        .flex_col()
        .gap_0p5()
        .max_w(u(320.))
        .px_2p5()
        .py_1p5()
        .rounded_md()
        .bg(rgba(theme::t().popup))
        .border_1()
        .border_color(rgba(theme::t().line))
        .shadow_md()
        .text_size(text::sm())
        .line_height(u(17.))
        .when_some(label, |d, (label, color)| d.child(div().text_size(text::xs()).text_color(rgba(color)).child(label)))
        .when_some(body, |d, body| d.child(div().text_color(rgba(theme::t().fg)).child(body)));
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_0()
        .child(deferred(
            anchored()
                .anchor(Anchor::TopLeft)
                .position(point(grid.bounds.left() + u(WIDTH), grid.bounds.top() + px(y) - u(12.)))
                .snap_to_window_with_margin(u(8.))
                .child(card),
        ))
        .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Metrics = Metrics { pad: 14., pitch: 7., thick: 2., gap: 12., fade: 21. };

    #[test]
    fn short_ruler_is_centered_and_still() {
        let f = Frame::new(M, 300., 5, 2);
        let l = layout(&f, 5, &[0, 4], 0.);
        assert_eq!(f.max_scroll, 0.);
        assert_eq!(l.marks.len(), 5);
        assert!(l.marks.iter().all(|p| p.alpha == 1. && !p.stuck));
        assert_eq!(l.y(0), Some((300. - 4. * 7.) / 2.));
    }

    #[test]
    fn band_holds_32_marks_in_the_middle_whatever_is_pinned() {
        let (h, n) = (800., 1000);
        let f = Frame::new(M, h, n, 2);
        assert_eq!(f.band, Frame::new(M, h, n, 12).band);
        assert_eq!(f.band.0 + f.band.1, h);
        let l = layout(&f, n, &[0, 999], 500.);
        assert_eq!(l.marks.iter().filter(|p| l.in_band(p)).count(), BAND);
    }

    #[test]
    fn marks_in_view_are_centered_unless_held() {
        let (h, n) = (800., 1000);
        let f = Frame::new(M, h, n, 2);
        let y = |scroll, i| layout(&f, n, &[0, 999], scroll).y(i).unwrap();
        let centered = keep_in_view(&f, 0., false, 498, 503);
        assert!((y(centered, 498) + y(centered, 503) - h).abs() < NOISE);
        let held = keep_in_view(&f, 0., true, 498, 503);
        assert!((y(held, 503) - f.band.1).abs() < NOISE);
    }

    #[test]
    fn long_ruler_shows_marks_in_view_clear_of_the_stacks() {
        let (n, sticky) = (1000, [0, 40, 500, 520, 999]);
        for (h, (a, b)) in [300., 800.].into_iter().flat_map(|h| [(0, 3), (45, 49), (498, 503), (600, 610), (995, 999)].map(|r| (h, r))) {
            let f = Frame::new(M, h, n, sticky.len());
            let l = layout(&f, n, &sticky, keep_in_view(&f, 0., false, a, b));
            for i in a..=b {
                assert!(l.marks.iter().any(|p| p.mark == i && p.alpha == 1. && l.in_band(p)), "mark {i} of {a}..={b}");
            }
            let mut ys: Vec<f32> = l.marks.iter().map(|p| p.y).collect();
            ys.sort_by(f32::total_cmp);
            assert!(ys.windows(2).all(|w| w[1] - w[0] >= M.thick), "overlap with {a}..={b}");
            assert!(ys[0] >= 0. && ys[ys.len() - 1] <= h);
        }
    }
}
