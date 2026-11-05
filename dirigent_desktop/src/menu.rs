//! Tiny popup menu: a floating list anchored to its trigger. Only one is open at a
//! time (`Dirigent::menu`); clicking outside closes it (and that click opens no other menu,
//! but a right click opens its context menu).
//!
//! Keys, wherever the focus is (an input too): Up / Down move a cursor over the open
//! menu's rows (`stop`), round the ends, from the selected one when none is lit. Left /
//! Right move it over the buttons in its header (`header_stop`); a menu without any leaves
//! them to the focused input. Enter clicks the lit stop, Escape closes the menu
//! (`Dirigent::dismiss`). The mouse moves the cursor too, so one stop is lit at a time.
//! Under a search (`results`) the top match is lit until the cursor moves.

use crate::app::{Dirigent, Menu};
use crate::input::{self, TextInput};
use crate::theme::{self, text, u};
use gpui::{
    Anchor, AnyElement, App, AvailableSpace, Context, Div, ElementId, Entity, Global, KeyBinding, Pixels, Point, ScrollHandle,
    SharedString, Stateful, Window, actions, anchored, canvas, div, point, prelude::*, px, rgba, size,
};
use std::rc::Rc;

actions!(menu, [Up, Down, Left, Right, Pick]);

/// What the root's key context has while a menu is open (`Dirigent::render`).
pub const KEY_CONTEXT: &str = "menu";

/// The arrows and Enter for the open menu. Bound after `input::key_bindings`, so in an
/// input (`menu > Input`, the same depth as the input's own) these win.
pub fn key_bindings() -> Vec<KeyBinding> {
    let in_input = format!("{KEY_CONTEXT} > {}", input::KEY_CONTEXT);
    [KEY_CONTEXT, &in_input]
        .into_iter()
        .flat_map(|c| {
            [
                KeyBinding::new("up", Up, Some(c)),
                KeyBinding::new("down", Down, Some(c)),
                KeyBinding::new("left", Left, Some(c)),
                KeyBinding::new("right", Right, Some(c)),
                KeyBinding::new("enter", Pick, Some(c)),
            ]
        })
        .collect()
}

/// What a stop does on a click or Enter.
type Action = Rc<dyn Fn(&mut Dirigent, &mut Window, &mut Context<Dirigent>)>;

/// The open menu's keyboard navigation. Its stops register here as they are built
/// (`stop`), so every render rebuilds them (`begin`) in build order, which has to be the
/// order they show in.
#[derive(Default)]
pub struct Nav {
    /// The menu the cursor is in; another one starts over.
    menu: Option<Menu>,
    /// The stop the keys or the mouse moved to last.
    cursor: Option<usize>,
    /// The mouse put `cursor` there: leaving the popup takes it away (`popup`), so Enter in
    /// the composer doesn't pick a row the mouse only passed over.
    by_mouse: bool,
    stops: Vec<Stop>,
    /// The first selected row: Up / Down step from it while nothing is lit.
    selected: Option<usize>,
    /// The first stop in `results`: lit while `cursor` is `None`.
    results_start: Option<usize>,
    /// `results` is building: the stops built now scroll in `results_scroll`, the others in
    /// `popup_scroll`.
    in_results: bool,
    /// The stops a `popup` has taken: one without any (a hover card) doesn't take `popup_scroll`.
    claimed: usize,
    results_scroll: ScrollHandle,
    popup_scroll: ScrollHandle,
}

impl Global for Nav {}

struct Stop {
    action: Action,
    /// A button in the header (Left / Right), not a row (Up / Down).
    header: bool,
    /// What it scrolls in: the results list or the popup.
    scroll: ScrollHandle,
    /// Its top and bottom in its scroller's content, where it was painted last (`reveal`).
    span: Option<(Pixels, Pixels)>,
}

impl Nav {
    /// A render begins: no stops yet. A menu other than the last one starts with no cursor,
    /// scrolled to the top.
    pub fn begin(menu: Option<&Menu>, cx: &mut App) {
        let nav = cx.default_global::<Nav>();
        if nav.menu.as_ref() != menu {
            *nav = Nav { menu: menu.cloned(), ..Default::default() };
        }
        nav.stops.clear();
        nav.selected = None;
        nav.results_start = None;
        nav.claimed = 0;
    }

    /// The search changed: its top match is lit again, the results scrolled to the top.
    pub fn restart(cx: &mut App) {
        let nav = cx.default_global::<Nav>();
        nav.cursor = None;
        nav.results_scroll = ScrollHandle::new();
    }

    fn lit(&self) -> Option<usize> {
        self.cursor.or(self.results_start).filter(|&i| i < self.stops.len())
    }

    /// Up / Down over the rows, Left / Right over the `header`'s stops: the cursor to the
    /// one `by` away, round the ends. It steps from the lit stop, or the selected row when
    /// none is lit; from outside the lane, Down / Right enter it at its first, Up / Left at
    /// its last. False when the lane is empty.
    fn step(&mut self, by: i32, header: bool) -> bool {
        let lane: Vec<usize> = (0..self.stops.len()).filter(|&i| self.stops[i].header == header).collect();
        if lane.is_empty() {
            return false;
        }
        let from = self.lit().or(self.selected).and_then(|i| lane.iter().position(|&j| j == i));
        let i = match from {
            Some(p) => lane[(p as i32 + by).rem_euclid(lane.len() as i32) as usize],
            None if by > 0 => lane[0],
            None => lane[lane.len() - 1],
        };
        self.cursor = Some(i);
        self.by_mouse = false;
        self.reveal(i);
        true
    }

    /// Scroll stop `i` into view in its scroller, if it was painted; with the popup's
    /// padding in view too (gpui clamps what a list without any overshoots).
    fn reveal(&self, i: usize) {
        let Stop { scroll, span: Some((top, bottom)), .. } = &self.stops[i] else { return };
        let (top, bottom) = (*top - u(4.), *bottom + u(4.));
        let height = scroll.bounds().size.height;
        let mut offset = scroll.offset();
        // `offset.y` is minus the distance scrolled: the stop shows while -y <= top and
        // bottom <= height - y.
        if top < -offset.y {
            offset.y = -top;
        } else if bottom > height - offset.y {
            offset.y = height - bottom;
        }
        scroll.set_offset(offset);
    }
}

pub fn up(_: &mut Dirigent, _: &Up, _: &mut Window, cx: &mut Context<Dirigent>) {
    step(-1, false, cx);
}

pub fn down(_: &mut Dirigent, _: &Down, _: &mut Window, cx: &mut Context<Dirigent>) {
    step(1, false, cx);
}

pub fn left(_: &mut Dirigent, _: &Left, _: &mut Window, cx: &mut Context<Dirigent>) {
    step(-1, true, cx);
}

pub fn right(_: &mut Dirigent, _: &Right, _: &mut Window, cx: &mut Context<Dirigent>) {
    step(1, true, cx);
}

/// A menu without stops for the key leaves it to whatever else wants it (the cursor of an input).
fn step(by: i32, header: bool, cx: &mut Context<Dirigent>) {
    if cx.default_global::<Nav>().step(by, header) {
        cx.notify();
    } else {
        cx.propagate();
    }
}

/// Enter: click the lit stop. With none (a menu the cursor hasn't moved in yet), Enter
/// does what it would without the menu.
pub fn pick(this: &mut Dirigent, _: &Pick, window: &mut Window, cx: &mut Context<Dirigent>) {
    let nav = cx.default_global::<Nav>();
    let action = nav.lit().map(|i| nav.stops[i].action.clone());
    match action {
        Some(action) => action(this, window, cx),
        None => cx.propagate(),
    }
}

/// Make `el` a row of the open menu (Up / Down): a click or Enter runs `action`, and it's
/// lit while the cursor is on it (moving the mouse over it puts the cursor there).
/// `selected` (the current choice) fills it.
pub fn stop(
    el: Stateful<Div>,
    selected: bool,
    action: impl Fn(&mut Dirigent, &mut Window, &mut Context<Dirigent>) + 'static,
    cx: &mut Context<Dirigent>,
) -> Stateful<Div> {
    if selected {
        let nav = cx.default_global::<Nav>();
        nav.selected.get_or_insert(nav.stops.len());
    }
    add(el.when(selected, |d| d.bg(rgba(theme::t().accent_active))), false, Rc::new(action), cx)
}

/// Make `el` a button in the open menu's header: a stop like `stop`'s, but Left / Right
/// reach it, not Up / Down. Not in a menu with a search: its input wants those keys.
pub fn header_stop(
    el: Stateful<Div>,
    action: impl Fn(&mut Dirigent, &mut Window, &mut Context<Dirigent>) + 'static,
    cx: &mut Context<Dirigent>,
) -> Stateful<Div> {
    add(el, true, Rc::new(action), cx)
}

/// Register `el` as the menu's next stop, in the header's lane or the rows'.
fn add(el: Stateful<Div>, header: bool, action: Action, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let nav = cx.default_global::<Nav>();
    let i = nav.stops.len();
    let lit = nav.cursor.or(nav.results_start) == Some(i);
    let scroll = if nav.in_results { nav.results_scroll.clone() } else { nav.popup_scroll.clone() };
    nav.stops.push(Stop { action: action.clone(), header, scroll: scroll.clone(), span: None });
    // Where it sits in its scroller's content, for `reveal`; its scroller laid out first.
    let measure = canvas(
        move |bounds, _, cx| {
            let top = bounds.top() - scroll.bounds().top() - scroll.offset().y;
            if let Some(stop) = cx.default_global::<Nav>().stops.get_mut(i) {
                stop.span = Some((top, top + bounds.size.height));
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();
    el.when(lit, |d| d.bg(rgba(theme::t().line)))
        .on_mouse_move(cx.listener(move |_, _, _, cx| {
            let nav = cx.default_global::<Nav>();
            nav.by_mouse = true;
            if nav.cursor.replace(i) != Some(i) {
                cx.notify();
            }
        }))
        .on_click(cx.listener(move |this, _, window, cx| action(this, window, cx)))
        .child(measure)
}

/// A search's results: a list of the stops `build` makes, scrolling past `max_h`. The
/// first is lit until the cursor moves, so Enter takes the top match.
pub fn results(
    id: &'static str,
    max_h: f32,
    cx: &mut Context<Dirigent>,
    build: impl FnOnce(&mut Context<Dirigent>) -> Vec<AnyElement>,
) -> AnyElement {
    let nav = cx.default_global::<Nav>();
    let start = nav.stops.len();
    nav.results_start = Some(start);
    nav.in_results = true;
    let scroll = nav.results_scroll.clone();
    let items = build(cx);
    let nav = cx.default_global::<Nav>();
    nav.in_results = false;
    if nav.stops.len() == start {
        nav.results_start = None;
    }
    div().id(id).flex().flex_col().max_h(u(max_h)).overflow_y_scroll().track_scroll(&scroll).children(items).into_any()
}

/// Gap between trigger and popup.
const GAP: f32 = 6.;
/// The popup's minimum distance from the window edge.
const MARGIN: f32 = 8.;
/// Tallest a popup gets; it shrinks (and scrolls) when there's less room on its side.
const MAX_H: f32 = 400.;
/// Every single-line row (item, header, danger) is this tall.
pub const ROW_H: f32 = 26.;

/// Where the popup goes relative to its trigger.
pub enum Side {
    /// Pops up above the trigger, left-aligned (composer chips).
    Above,
    /// Drops down below the trigger, `trigger_height` tall, left-aligned.
    Below(f32),
    /// Drops down below, right edge aligned with the trigger's right edge (header buttons).
    BelowRight(f32),
    /// Floats to the right of the trigger, top-aligned (sidebar card specs).
    Right,
    /// Its top-left corner at a window position (a right click's), wherever the trigger is.
    At(Point<Pixels>),
}

/// Wrap a popup so it floats over everything else. Put it as a child of a `relative()` trigger.
pub fn popup(side: Side, items: Vec<AnyElement>, cx: &mut Context<Dirigent>) -> AnyElement {
    // `right`: the canvas is pinned to the trigger's top-right corner instead of top-left.
    // `at`: the popup goes there instead, in window coordinates.
    let (anchor, dx, dy, right, at) = match side {
        Side::Above => (Anchor::BottomLeft, 0., -GAP, false, None),
        Side::Below(h) => (Anchor::TopLeft, 0., h + GAP, false, None),
        Side::BelowRight(h) => (Anchor::TopRight, 0., h + GAP, true, None),
        Side::Right => (Anchor::TopLeft, GAP, 0., true, None),
        Side::At(p) => (Anchor::TopLeft, 0., 0., false, Some(p)),
    };
    let dismiss = cx.listener(|this, _, _, cx| this.dismiss_menu(cx));
    // The popup scrolls its stops into view (`Nav::reveal`), unless it has none.
    let nav = cx.default_global::<Nav>();
    let scroll = (nav.stops.len() > nav.claimed).then(|| nav.popup_scroll.clone());
    nav.claimed = nav.stops.len();
    let leave = cx.listener(|_, hovered: &bool, _, cx| {
        let nav = cx.default_global::<Nav>();
        if !hovered && nav.by_mouse {
            nav.cursor = None;
            cx.notify();
        }
    });
    // A zero-size canvas pinned to a corner of the trigger (without the absolute pin it would
    // take the flex "static position", e.g. centered). Its prepaint knows the trigger's window
    // position, so it caps the popup's height to the room on its side (the popup stays put and
    // scrolls instead of sliding over the trigger), then lays it out and defers it so it draws
    // above everything else. A popup at a position (`Side::At`) has no trigger to keep clear: it gets the
    // window's height, and snapping lifts it from a click near the bottom.
    canvas(
        move |bounds, window, cx| {
            let height = window.viewport_size().height;
            let y = bounds.origin.y + u(dy);
            let room = match at {
                Some(_) => height - u(MARGIN) * 2.,
                None if dy < 0. => y - u(MARGIN),
                None => height - y - u(MARGIN),
            };
            let anchored = anchored().anchor(anchor).offset(point(u(dx), u(dy))).snap_to_window_with_margin(u(MARGIN));
            let mut popup = match at {
                Some(p) => anchored.position(p),
                None => anchored,
            }
            .child(
                div()
                    .id("menu")
                    // Swallow mouse events so triggers under the popup don't fire too.
                    .occlude()
                    .flex()
                    .flex_col()
                    .max_w(u(520.))
                    .max_h(room.clamp(u(60.), u(MAX_H)))
                    .overflow_y_scroll()
                    .when_some(scroll, |d, s| d.track_scroll(&s))
                    .p_1()
                    .rounded_lg()
                    .bg(rgba(theme::t().popup))
                    .border_1()
                    .border_color(rgba(theme::t().line))
                    .shadow_lg()
                    .text_color(rgba(theme::t().fg))
                    .on_mouse_down_out(dismiss)
                    .on_hover(leave)
                    .children(items),
            )
            .into_any();
            popup.layout_as_root(size(AvailableSpace::MaxContent, AvailableSpace::MaxContent), window, cx);
            window.defer_draw(popup, bounds.origin, 1, None);
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .when(right, |d| d.right_0())
    .when(!right, |d| d.left_0())
    .size_0()
    .into_any()
}

/// One row: `label` followed by a dim `detail`, highlighted when `selected`.
pub fn item(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    selected: bool,
    on_click: impl Fn(&mut Dirigent, &mut Context<Dirigent>) + 'static,
    cx: &mut Context<Dirigent>,
) -> AnyElement {
    stop(row(id), selected, move |this, _, cx| on_click(this, cx), cx)
        .child(div().flex_shrink_0().child(label.into()))
        .child(div().min_w_0().truncate().text_color(rgba(theme::t().fg_1)).text_size(text::sm()).child(detail.into()))
        .into_any()
}

/// The bare clickable row every item is built on, at a fixed height (`stop` adds the
/// selected and lit fills). `flex_shrink_0` keeps rows from being squashed when the popup
/// is height-capped.
pub fn row(id: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(ElementId::Name(id.into()))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(u(ROW_H))
        .flex_shrink_0()
        .px_2()
        .rounded_md()
        .cursor_pointer()
}

/// A destructive action: same row, red label.
pub fn danger(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    on_click: impl Fn(&mut Dirigent, &mut Context<Dirigent>) + 'static,
    cx: &mut Context<Dirigent>,
) -> AnyElement {
    stop(row(id), false, move |this, _, cx| on_click(this, cx), cx).text_color(rgba(theme::t().error)).child(label.into()).into_any()
}

/// A non-interactive section label.
pub fn header(text: impl Into<SharedString>) -> AnyElement {
    div()
        .px_2()
        .h(u(ROW_H))
        .flex_shrink_0()
        .flex()
        .items_center()
        .text_size(text::xs())
        .text_color(rgba(theme::t().fg_2))
        .child(text.into())
        .into_any()
}

/// A hairline between sections.
pub fn separator() -> AnyElement {
    div().h(px(1.)).flex_shrink_0().my_1().bg(rgba(theme::t().line)).into_any()
}

/// A search field as a row, for a popup that filters its items as you type.
pub fn search(input: Entity<TextInput>) -> Div {
    div().flex().items_center().h(u(ROW_H)).flex_shrink_0().px_2().child(div().flex_1().min_w_0().text_size(text::base()).child(input))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `headers` header stops, then `rows` rows; the results starting at `results_start`.
    fn nav(headers: usize, rows: usize, results_start: Option<usize>) -> Nav {
        let mut nav = Nav { results_start, ..Default::default() };
        for i in 0..headers + rows {
            let action: Action = Rc::new(|_: &mut Dirigent, _: &mut Window, _: &mut Context<Dirigent>| {});
            nav.stops.push(Stop { action, header: i < headers, scroll: ScrollHandle::new(), span: None });
        }
        nav
    }

    #[test]
    fn steps_round_the_ends() {
        // Nothing lit: Down takes the first, Up the last; then round the ends.
        let mut n = nav(0, 3, None);
        assert_eq!(n.lit(), None);
        n.step(1, false);
        assert_eq!(n.lit(), Some(0));
        n.step(-1, false);
        assert_eq!(n.lit(), Some(2));
        n.step(1, false);
        assert_eq!(n.lit(), Some(0));
        let mut n = nav(0, 3, None);
        n.step(-1, false);
        assert_eq!(n.lit(), Some(2));
        // No stops: the key goes on to the input.
        assert!(!nav(0, 0, None).step(1, false));
    }

    #[test]
    fn steps_from_the_selected_row() {
        // `xhigh` selected in low / medium / high / xhigh / max.
        let mut n = nav(0, 5, None);
        n.selected = Some(3);
        n.step(1, false);
        assert_eq!(n.lit(), Some(4));
        n.cursor = None;
        n.step(-1, false);
        assert_eq!(n.lit(), Some(2));
    }

    #[test]
    fn header_and_rows_are_separate_lanes() {
        // `Save preset`, `All models`, then three presets, the second selected.
        let mut n = nav(2, 3, None);
        n.selected = Some(3);
        // Up / Down skip the header, round the rows' ends.
        n.step(1, false);
        assert_eq!(n.lit(), Some(4));
        n.step(1, false);
        assert_eq!(n.lit(), Some(2));
        // Left / Right enter the header at its ends and stay in it.
        n.step(1, true);
        assert_eq!(n.lit(), Some(0));
        n.step(1, true);
        assert_eq!(n.lit(), Some(1));
        n.step(1, true);
        assert_eq!(n.lit(), Some(0));
        // Down from the header: the first row.
        n.step(1, false);
        assert_eq!(n.lit(), Some(2));
        n.step(-1, true);
        assert_eq!(n.lit(), Some(1));
        // No header: Left / Right go on to the input.
        assert!(!nav(0, 3, None).step(1, true));
    }

    #[test]
    fn results_light_their_first_until_the_cursor_moves() {
        // `in project` above the base picker's results.
        let mut n = nav(0, 4, Some(1));
        assert_eq!(n.lit(), Some(1));
        n.step(-1, false);
        assert_eq!(n.lit(), Some(0));
        // A cursor past the stops (the list shrank) lights nothing.
        n.cursor = Some(9);
        assert_eq!(n.lit(), None);
        n.step(1, false);
        assert_eq!(n.lit(), Some(0));
    }
}
