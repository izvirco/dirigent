//! Tiny popup menu: a floating list anchored to its trigger. Only one is open at a
//! time (`Dirigent::menu`); clicking outside closes it (and that click opens no other menu,
//! but a right click opens its context menu).

use crate::app::Dirigent;
use crate::theme::{self, text, u};
use gpui::{
    Anchor, AnyElement, AvailableSpace, Context, Div, ElementId, Pixels, Point, SharedString, Stateful, anchored, canvas, div, point,
    prelude::*, px, rgba, size,
};

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
                    .p_1()
                    .rounded_lg()
                    .bg(rgba(theme::t().popup))
                    .border_1()
                    .border_color(rgba(theme::t().line))
                    .shadow_lg()
                    .text_color(rgba(theme::t().fg))
                    .on_mouse_down_out(dismiss)
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
    row(id, selected)
        .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
        .child(div().flex_shrink_0().child(label.into()))
        .child(div().min_w_0().truncate().text_color(rgba(theme::t().fg_1)).text_size(text::sm()).child(detail.into()))
        .into_any()
}

/// The bare clickable row every item is built on: fixed height, hover and selected fills.
/// `flex_shrink_0` keeps rows from being squashed when the popup is height-capped.
pub fn row(id: impl Into<SharedString>, selected: bool) -> Stateful<Div> {
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
        .when(selected, |d| d.bg(rgba(theme::t().accent_active)))
        .hover(|d| d.bg(rgba(theme::t().line)))
}

/// A destructive action: same row, red label.
pub fn danger(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    on_click: impl Fn(&mut Dirigent, &mut Context<Dirigent>) + 'static,
    cx: &mut Context<Dirigent>,
) -> AnyElement {
    row(id, false)
        .text_color(rgba(theme::t().error))
        .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
        .child(label.into())
        .into_any()
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
