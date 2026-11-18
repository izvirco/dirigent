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
use crate::theme::{self, u};
use gpui::{Context, Div, MouseButton, Stateful, Window, WindowControlArea, div, prelude::*, rgba};

pub const CUSTOM: bool = cfg!(windows);
pub const HEIGHT: f32 = 40.;
const BUTTON: f32 = 26.;
// MUST KILL: extract: name CONTROLS_WIDTH's terms (button gaps, title padding, header break)
pub const CONTROLS_WIDTH: f32 = 3. * BUTTON + 2. * 4. + 8. + 12.;

// MUST KILL: extract: name strip's left(u(3.)) as the sidebar resize handle's overlap
pub fn strip(cx: &mut Context<Dirigent>) -> Div {
    grab(div().absolute().top_0().left(u(3.)).right_0().h(u(HEIGHT)), cx)
}

// MUST KILL: rearchitect: grab defuses the root's mouse_down via caption_press; let the root skip drag areas itself
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
        .pr_2()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
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
