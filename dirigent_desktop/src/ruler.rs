//! The ruler: a strip of tick marks down the left edge of the thread pane, one per
//! message (user = blue, reply = gray, pinned = amber, compaction = dotted), centered
//! vertically at a fixed pitch so it grows from the middle as the thread does and
//! compresses once it would run past the edges. Marks for the messages in view are shorter and brighter; hovering
//! a mark previews its message and clicking scrolls the transcript to it.
//!
//! Everything animated (a mark growing in, the in-view state moving as you scroll)
//! is a tween stepped in the canvas' paint, which asks for the next frame until all
//! of them have settled — so the ruler costs nothing while it is still.

use crate::app::{Dirigent, OpenThread, Pane};
use crate::theme::{self, text, u};
use crate::view::Item;
use gpui::{
    Anchor, AnyElement, Bounds, ClickEvent, Context, Hsla, ListState, MouseMoveEvent, Pixels, Point, anchored, canvas, deferred, div, fill,
    point, prelude::*, px, rgba, size,
};
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Instant;

// Lengths are design pixels, drawn `theme::zoom()` times as big (`theme::u`).

/// Strip width; the reading column and composer are centered, so this sits in their left margin.
pub const WIDTH: f32 = 28.;
/// Marks start this far from the pane's left edge and are this long; only being in
/// view changes their length.
const INSET: f32 = 8.;
const LENGTH: f32 = 14.;
/// The ruler keeps this far from the strip's top and bottom.
const PAD: f32 = 14.;
/// Mark thickness (one more while hovered)...
const THICK: f32 = 2.;
/// ...and the distance between marks until they have to squeeze into the strip's height.
const PITCH: f32 = 7.;
/// A mark in view loses this much of its length...
const SHRINK: f32 = 0.4;
/// ...and the others keep this much of their colour.
const DIM: f32 = 0.4;
/// Time constant of every tween, in seconds: ~95% of the way in three of these.
const TAU: f32 = 0.09;
/// Longest preview text in the hover card.
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

/// A timeline item the ruler shows.
#[derive(Clone, Copy)]
struct Mark {
    item: usize,
    kind: Kind,
    pinned: bool,
}

/// The ruler's animated state: one tween per mark (`grow` from nothing on arrival, `on`
/// while its message is in view), the strip's geometry from the last paint (for hit
/// testing), and the hovered mark. Shared with the paint closure, hence the `Rc`.
#[derive(Default)]
pub struct Ruler {
    tweens: Vec<Tween>,
    grid: Grid,
    last_frame: Option<Instant>,
    hover: Option<usize>,
}

/// Where the marks go: the strip's window bounds and the display's scale factor. Every
/// edge is snapped to the device pixel grid, so at fractional scales (1.25x) a 2px
/// line doesn't come out 2 or 3 device px depending on where it lands.
#[derive(Default, Clone, Copy)]
struct Grid {
    bounds: Bounds<Pixels>,
    scale: f32,
}

impl Grid {
    fn snap(&self, v: f32) -> f32 {
        if self.scale > 0. { (v * self.scale).round() / self.scale } else { v.round() }
    }

    /// Vertical distance between marks: `PITCH`, or less once they'd overflow the strip.
    fn pitch(&self, n: usize) -> f32 {
        let z = theme::zoom();
        self.snap((PITCH * z).min((f32::from(self.bounds.size.height) - 2. * PAD * z) / n.max(1) as f32))
    }

    /// Offset of the first mark from the strip's top: the ruler is centered in it.
    fn top(&self, n: usize) -> f32 {
        (f32::from(self.bounds.size.height) - (n.saturating_sub(1)) as f32 * self.pitch(n)) / 2.
    }

    /// Window y of mark `i`.
    fn mark_y(&self, i: usize, n: usize) -> Pixels {
        px(self.snap(f32::from(self.bounds.top()) + self.top(n) + i as f32 * self.pitch(n)))
    }

    /// The mark whose slot contains window position `p`.
    fn mark_at(&self, n: usize, p: Point<Pixels>) -> Option<usize> {
        if n == 0 || !self.bounds.contains(&p) {
            return None;
        }
        let pitch = self.pitch(n);
        let i = (f32::from(p.y - self.bounds.top()) - self.top(n) + pitch / 2.) / pitch;
        (i >= 0. && (i as usize) < n).then_some(i as usize)
    }
}

pub type State = Rc<RefCell<Ruler>>;

#[derive(Default, Clone, Copy)]
struct Tween {
    grow: f32,
    on: f32,
}

impl Tween {
    /// Move `k` of the remaining way to the targets; true once there (snapped).
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

/// The marks of a thread, in order.
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

/// Items of which enough is in the viewport to count as "what you're looking at": at
/// least half of a short item, 40px of a tall one. From the list's last layout.
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

/// The strip, to be laid over the thread column's left edge (transcript and composer,
/// so the composer's height doesn't move it).
pub fn render(this: &mut Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let Pane::Thread(t) = &this.pane else { return div().into_any() };
    let marks: Rc<[Mark]> = marks(t).into();
    let n = marks.len();
    let visible = visible_items(&t.list);
    let state = t.ruler.clone();
    let hover = state.borrow().hover.filter(|&i| i < n);
    let preview = hover.map(|i| preview_card(i, &marks[i], t, &state.borrow().grid, n));

    let paint_marks = marks.clone();
    let paint_state = state.clone();
    let strip = canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
            let mut s = paint_state.borrow_mut();
            s.grid = Grid { bounds, scale: window.scale_factor() };
            let grid = s.grid;
            let now = Instant::now();
            // Capped: after an idle stretch the first frame is one normal step, not a snap.
            let dt = s.last_frame.map_or(0., |l| now.duration_since(l).as_secs_f32()).min(1. / 30.);
            s.last_frame = Some(now);
            // Exponential approach, frame-rate independent; reduced motion snaps.
            let z = theme::zoom();
            let k = if cx.reduce_motion() { 1. } else { 1. - (-dt / TAU).exp() };
            s.tweens.resize(n, Tween::default());
            let mut settled = true;
            for (i, m) in paint_marks.iter().enumerate() {
                let hovered = s.hover == Some(i);
                let tw = &mut s.tweens[i];
                settled &= tw.step(k, if visible.contains(&m.item) { 1. } else { 0. });
                let w = LENGTH * z * (1. - SHRINK * tw.on) * tw.grow;
                let color: Hsla = rgba(if m.pinned { theme::t().warn } else { m.kind.color() }).into();
                let color = if hovered { rgba(theme::t().fg).into() } else { color.opacity(DIM + (1. - DIM) * tw.on) };
                let (x, y) = (px(grid.snap(f32::from(bounds.left()) + INSET * z)), grid.mark_y(i, n));
                let h = px(grid.snap(z * if hovered { THICK + 1. } else { THICK }));
                if m.kind == Kind::Compaction {
                    // Dotted: 2px dots with 2px gaps (zoomed) across the (shrinking) width.
                    let mut dx = 0.;
                    while dx < w {
                        window.paint_quad(fill(Bounds::new(point(x + px(dx), y), size(px((w - dx).min(2. * z)), h)), color));
                        dx += 4. * z;
                    }
                } else {
                    window.paint_quad(fill(Bounds::new(point(x, y), size(px(w), h)), color));
                }
            }
            if !settled {
                window.request_animation_frame();
            }
        },
    )
    .size_full();

    let hover_state = state.clone();
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
            let hit = s.grid.mark_at(n, ev.position);
            if s.hover != hit {
                s.hover = hit;
                cx.notify();
            }
        }))
        .on_hover(cx.listener(move |_, hovered, _, cx| {
            if !*hovered {
                state.borrow_mut().hover = None;
                cx.notify();
            }
        }))
        .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
            let Pane::Thread(t) = &this.pane else { return };
            let hit = t.ruler.borrow().grid.mark_at(n, ev.position());
            if let Some(i) = hit {
                this.jump_to(marks[i].item, cx);
            }
        }))
        .child(strip)
        .children(preview)
        .into_any()
}

/// The hover card for mark `i`: what kind of message, and its first line or two,
/// floated to the right of the mark over the transcript (or composer).
fn preview_card(i: usize, m: &Mark, t: &OpenThread, grid: &Grid, n: usize) -> AnyElement {
    let text = match &t.timeline.items[m.item] {
        Item::User { text, .. } | Item::Assistant(text) => text.as_str(),
        _ => "",
    };
    let mut preview: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(PREVIEW_CHARS).collect();
    if preview.chars().count() == PREVIEW_CHARS {
        preview.push('…');
    }
    let label = if m.pinned { format!("pinned · {}", m.kind.label()) } else { m.kind.label().to_owned() };
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
        .child(div().text_size(text::xs()).text_color(rgba(if m.pinned { theme::t().warn } else { theme::t().fg_2 })).child(label))
        .when(!preview.is_empty(), |d| d.child(div().text_color(rgba(theme::t().fg)).child(preview)));
    // Window-positioned beside the mark; the zero-size absolute wrapper keeps the
    // anchored element out of the flex flow (see AGENTS.md).
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_0()
        .child(deferred(
            anchored()
                .anchor(Anchor::TopLeft)
                .position(point(grid.bounds.left() + u(WIDTH), grid.mark_y(i, n) - u(12.)))
                .snap_to_window_with_margin(u(8.))
                .child(card),
        ))
        .into_any()
}
