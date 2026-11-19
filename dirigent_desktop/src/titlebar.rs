//! Dragging and the buttons are the system's: gpui answers `WM_NCHITTEST` with a caption
//! or a button for the `window_control_area`s under the mouse, so double-click to
//! maximize, Aero Snap and the snap layouts flyout all work. The catch is that the system
//! takes the press there (its release never reaches us): whatever is clickable over the
//! strip must `occlude()` it (the header's actions, the breadcrumb's links).
//!
//! gpui still dispatches the press itself before the system acts on it, so a grab area
//! defuses it (`grab`). It can't `stop_propagation()`: gpui would report the press
//! handled, and the system would never start the move.

use crate::app::Dirigent;
use crate::icons::icon;
use crate::sidebar;
use crate::theme::{self, u};
use gpui::{Context, Div, MouseButton, Stateful, Window, WindowControlArea, div, prelude::*, rgba};

pub const CUSTOM: bool = cfg!(windows);
pub const HEIGHT: f32 = 40.;
const BUTTON: f32 = 26.;
const BUTTON_GAP: f32 = 4.;
const EDGE_PADDING: f32 = 8.;
const HEADER_BREAK: f32 = 12.;
pub const CONTROLS_WIDTH: f32 = HEADER_BREAK + 3. * BUTTON + 2. * BUTTON_GAP + EDGE_PADDING;

pub fn strip(cx: &mut Context<Dirigent>) -> Div {
    grab(div().absolute().top_0().left(u(sidebar::HANDLE_OVERLAP)).right_0().h(u(HEIGHT)), cx)
}

pub fn grab(d: Div, cx: &mut Context<Dirigent>) -> Div {
    d.window_control_area(WindowControlArea::Drag)
        .on_any_mouse_down(|_, window, _| window.prevent_default())
        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, _| this.caption_press = true))
}

pub fn controls(window: &Window) -> Div {
    let max = if window.is_maximized() { "copy" } else { "square-outline" };
    div()
        .absolute()
        .top_0()
        .right_0()
        .h(u(HEIGHT))
        .pr(u(EDGE_PADDING))
        .flex()
        .flex_row()
        .items_center()
        .gap(u(BUTTON_GAP))
        .child(button("caption-min", "minus", WindowControlArea::Min))
        .child(button("caption-max", max, WindowControlArea::Max))
        .child(button("caption-close", "x", WindowControlArea::Close))
}

fn button(id: &'static str, glyph: &'static str, area: WindowControlArea) -> Stateful<Div> {
    div()
        .id(id)
        .occlude()
        .size(u(BUTTON))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .hover(|d| d.bg(rgba(theme::t().hover)))
        .window_control_area(area)
        .child(icon(glyph, theme::t().fg_1))
}
