//! The window's title bar on Windows. The system one is hidden (`appears_transparent`), so
//! the window is the app's edge to edge: the top `HEIGHT` of the main pane drags it
//! (`strip`, under the pane's content), and so does the sidebar's title; our own caption
//! buttons, icon buttons like the sidebar title's, sit in the top-right corner
//! (`controls`). Elsewhere the system keeps its own.
//!
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

/// Whether the app draws its own title bar.
pub const CUSTOM: bool = cfg!(windows);
/// The grab strip's height: `lists::header`'s and the sidebar title's.
pub const HEIGHT: f32 = 40.;
/// A caption button: `theme::icon_button`'s size.
const BUTTON: f32 = 26.;
/// What the caption buttons take off the right end of the main pane's header: the
/// buttons 4px apart (`gap_1`, like the header's), the sidebar title's right padding after
/// them, and before them the break the header leaves before its panel button (`gap_1` +
/// `ml_2`), so they read as one more group in its row.
pub const CONTROLS_WIDTH: f32 = 3. * BUTTON + 2. * 4. + 8. + 12.;

/// The grab strip across the top of the main pane, its first child, so everything in the
/// pane is drawn over it. It starts past the sidebar's resize handle, which reaches 3px
/// into the pane and is drawn under it (an occluding handle would hide its own drag from
/// the root's mouse listeners).
pub fn strip(cx: &mut Context<Dirigent>) -> Div {
    grab(div().absolute().top_0().left(u(3.)).right_0().h(u(HEIGHT)), cx)
}

/// Makes `d` drag the window. Its press, dispatched to us too, must not do what a press
/// on the app does: `prevent_default()` keeps the root from taking focus (the composer
/// would lose it), and `caption_press` has the root's `on_mouse_down` leave the selection
/// and a rename alone. This listener runs first: the root's are registered before it.
pub fn grab(d: Div, cx: &mut Context<Dirigent>) -> Div {
    d.window_control_area(WindowControlArea::Drag)
        .on_any_mouse_down(|_, window, _| window.prevent_default())
        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, _| this.caption_press = true))
}

/// Minimize, maximize / restore and close, in the window's top-right corner, over
/// everything (a modal's dim too: the window can always be closed). Laid out like the
/// sidebar title's buttons: centered in the header's height, as far from the edge.
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
        // Over the grab strip: without this the press would be the strip's.
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
