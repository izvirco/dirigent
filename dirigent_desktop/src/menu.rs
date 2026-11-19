use crate::app::{Dirigent, Menu};
use crate::icons::icon;
use crate::input::{self, TextInput};
use crate::theme::{self, text, u};
use gpui::{
    Anchor, AnyElement, App, AvailableSpace, Context, Div, ElementId, Entity, Global, KeyBinding, Pixels, Point, ScrollHandle,
    SharedString, Stateful, Window, actions, anchored, canvas, div, point, prelude::*, px, rgba, size,
};
use std::rc::Rc;

actions!(menu, [Up, Down, Left, Right, Pick]);

pub const KEY_CONTEXT: &str = "menu";

/// Bound after `input::key_bindings`, so in an input (`menu > Input`, the same depth as the
/// input's own) these win.
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

type Action = Rc<dyn Fn(&mut Dirigent, &mut Window, &mut Context<Dirigent>)>;

#[derive(Default)]
pub struct Nav {
    menu: Option<Menu>,
    cursor: Option<usize>,
    by_mouse: bool,
    stops: Vec<Stop>,
    selected: Option<usize>,
    results_start: Option<usize>,
    in_results: bool,
    claimed: usize,
    results_scroll: ScrollHandle,
    popup_scroll: ScrollHandle,
}

impl Global for Nav {}

struct Stop {
    action: Action,
    header: bool,
    scroll: ScrollHandle,
    span: Option<(Pixels, Pixels)>,
}

impl Nav {
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

    pub fn restart(cx: &mut App) {
        let nav = cx.default_global::<Nav>();
        nav.cursor = None;
        nav.results_scroll = ScrollHandle::new();
    }

    fn lit(&self) -> Option<usize> {
        self.cursor.or(self.results_start).filter(|&i| i < self.stops.len())
    }

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

    fn reveal(&self, i: usize) {
        let Stop { scroll, span: Some((top, bottom)), .. } = &self.stops[i] else { return };
        let (top, bottom) = (*top - u(PAD), *bottom + u(PAD));
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

fn step(by: i32, header: bool, cx: &mut Context<Dirigent>) {
    if cx.default_global::<Nav>().step(by, header) {
        cx.notify();
    } else {
        cx.propagate();
    }
}

pub fn pick(this: &mut Dirigent, _: &Pick, window: &mut Window, cx: &mut Context<Dirigent>) {
    let nav = cx.default_global::<Nav>();
    let action = nav.lit().map(|i| nav.stops[i].action.clone());
    match action {
        Some(action) => action(this, window, cx),
        None => cx.propagate(),
    }
}

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

pub fn header_stop(
    el: Stateful<Div>,
    action: impl Fn(&mut Dirigent, &mut Window, &mut Context<Dirigent>) + 'static,
    cx: &mut Context<Dirigent>,
) -> Stateful<Div> {
    add(el, true, Rc::new(action), cx)
}

fn add(el: Stateful<Div>, header: bool, action: Action, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let nav = cx.default_global::<Nav>();
    let i = nav.stops.len();
    let lit = nav.cursor.or(nav.results_start) == Some(i);
    let scroll = if nav.in_results { nav.results_scroll.clone() } else { nav.popup_scroll.clone() };
    nav.stops.push(Stop { action: action.clone(), header, scroll: scroll.clone(), span: None });
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

const GAP: f32 = 6.;
const MARGIN: f32 = 8.;
const PAD: f32 = 4.;
const MAX_H: f32 = 400.;
pub const ROW_H: f32 = 26.;

pub enum Side {
    Above,
    Below { trigger_h: f32 },
    BelowRight { trigger_h: f32 },
    Right,
    At(Point<Pixels>),
}

pub fn popup(side: Side, items: Vec<AnyElement>, cx: &mut Context<Dirigent>) -> AnyElement {
    let dismiss = cx.listener(|this, _, _, cx| this.dismiss_menu(cx));
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
    let menu = div()
        .id("menu")
        .occlude()
        .flex()
        .flex_col()
        .max_w(u(520.))
        .overflow_y_scroll()
        .when_some(scroll, |d, s| d.track_scroll(&s))
        .p(u(PAD))
        .rounded_lg()
        .bg(rgba(theme::t().popup))
        .border_1()
        .border_color(rgba(theme::t().line))
        .shadow_lg()
        .text_color(rgba(theme::t().fg))
        .on_mouse_down_out(dismiss)
        .on_hover(leave)
        .children(items);
    float(side, menu)
}

fn float(side: Side, menu: Stateful<Div>) -> AnyElement {
    let (anchor, dx, dy) = match side {
        Side::Above => (Anchor::BottomLeft, 0., -GAP),
        Side::Below { trigger_h } => (Anchor::TopLeft, 0., trigger_h + GAP),
        Side::BelowRight { trigger_h } => (Anchor::TopRight, 0., trigger_h + GAP),
        Side::Right => (Anchor::TopLeft, GAP, 0.),
        Side::At(_) => (Anchor::TopLeft, 0., 0.),
    };
    let pin_right = matches!(side, Side::BelowRight { .. } | Side::Right);
    canvas(
        move |bounds, window, cx| {
            let height = window.viewport_size().height;
            let y = bounds.origin.y + u(dy);
            let room = match side {
                Side::Above => y - u(MARGIN),
                Side::Below { .. } | Side::BelowRight { .. } => height - y - u(MARGIN),
                Side::Right | Side::At(_) => height - u(MARGIN) * 2.,
            };
            let mut anchored = anchored().anchor(anchor).offset(point(u(dx), u(dy))).snap_to_window_with_margin(u(MARGIN));
            if let Side::At(p) = side {
                anchored = anchored.position(p);
            }
            let mut popup = anchored.child(menu.max_h(room.clamp(u(60.), u(MAX_H)))).into_any();
            popup.layout_as_root(size(AvailableSpace::MaxContent, AvailableSpace::MaxContent), window, cx);
            window.defer_draw(popup, bounds.origin, 1, None);
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .when(pin_right, |d| d.right_0())
    .when(!pin_right, |d| d.left_0())
    .size_0()
    .into_any()
}

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

pub fn danger(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    on_click: impl Fn(&mut Dirigent, &mut Context<Dirigent>) + 'static,
    cx: &mut Context<Dirigent>,
) -> AnyElement {
    stop(row(id), false, move |this, _, cx| on_click(this, cx), cx).text_color(rgba(theme::t().error)).child(label.into()).into_any()
}

pub fn disabled(id: impl Into<SharedString>, label: impl Into<SharedString>) -> AnyElement {
    row(id).cursor_default().text_color(rgba(theme::t().fg_2)).child(label.into()).into_any()
}

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

pub fn header_button(id: &'static str, label: &'static str, trailing: Option<&'static str>, color: u32) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_0p5()
        .h(u(22.))
        .px_1p5()
        .rounded_md()
        .cursor_pointer()
        .text_size(text::sm())
        .text_color(rgba(color))
        .hover(|d| d.text_color(rgba(theme::t().fg)))
        .child(label)
        .children(trailing.map(|name| icon(name, color).size(u(12.))))
}

pub fn separator() -> AnyElement {
    div().h(px(1.)).flex_shrink_0().my_1().bg(rgba(theme::t().line)).into_any()
}

pub fn search(input: Entity<TextInput>) -> Div {
    div().flex().items_center().h(u(ROW_H)).flex_shrink_0().px_2().child(div().flex_1().min_w_0().text_size(text::base()).child(input))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!nav(0, 0, None).step(1, false));
    }

    #[test]
    fn steps_from_the_selected_row() {
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
        let mut n = nav(2, 3, None);
        n.selected = Some(3);
        n.step(1, false);
        assert_eq!(n.lit(), Some(4));
        n.step(1, false);
        assert_eq!(n.lit(), Some(2));
        n.step(1, true);
        assert_eq!(n.lit(), Some(0));
        n.step(1, true);
        assert_eq!(n.lit(), Some(1));
        n.step(1, true);
        assert_eq!(n.lit(), Some(0));
        n.step(1, false);
        assert_eq!(n.lit(), Some(2));
        n.step(-1, true);
        assert_eq!(n.lit(), Some(1));
        assert!(!nav(0, 3, None).step(1, true));
    }

    #[test]
    fn results_light_their_first_until_the_cursor_moves() {
        let mut n = nav(0, 4, Some(1));
        assert_eq!(n.lit(), Some(1));
        n.step(-1, false);
        assert_eq!(n.lit(), Some(0));
        n.cursor = Some(9);
        assert_eq!(n.lit(), None);
        n.step(1, false);
        assert_eq!(n.lit(), Some(0));
    }
}
