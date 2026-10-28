//! The frame every modal sits in: a card centered over a dimmed window. A press on the
//! dim closes the modal (`Dirigent::dim_pressed`); the card occludes it, so presses
//! inside the card never get there. One modal is open at a time: Add project
//! (`add_project.rs`), or `Dirigent::modal` (Settings, Usage).

use crate::app::Dirigent;
use crate::theme::{self, u};
use gpui::{AnyElement, Context, Div, MouseButton, div, prelude::*, rgba};

/// The large card (Settings, Usage) at its biggest; smaller windows shrink it.
const LARGE_W: f32 = 900.;
const LARGE_H: f32 = 640.;

/// `card` over the dim, given the card look: the `popup` fill made opaque, so a
/// translucent theme's card never shows the window behind it. The caller sizes the card.
pub fn frame(card: Div, cx: &mut Context<Dirigent>) -> AnyElement {
    let t = theme::t();
    div()
        .id("modal-dim")
        .occlude()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .p_8()
        .bg(rgba(0x00000080))
        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.dim_pressed(cx)))
        .child(
            card.occlude()
                .flex()
                .flex_col()
                .rounded_lg()
                .bg(rgba(theme::alpha(t.popup, 0xff)))
                .border_1()
                .border_color(rgba(t.line))
                .shadow_lg(),
        )
        .into_any()
}

/// The Settings and Usage card, as big as the window allows, up to `LARGE_W` × `LARGE_H`.
/// `body` fills it on the main pane's surface, so it reads like the app behind it (the
/// Settings tab list goes on the sidebar's canvas) and the row lines and hover fills
/// made for that surface show. Under it is the canvas made solid, as the window is under
/// the app: a translucent theme's clear or see-through surfaces then look as they do in the
/// window, not tinted by the card's `popup` fill. An opaque theme's panel covers it.
pub fn large(body: Div) -> Div {
    let t = theme::t();
    let base = div().flex_1().min_h_0().flex().flex_col().rounded_lg().bg(rgba(theme::alpha(t.canvas, 0xff)));
    div().w_full().max_w(u(LARGE_W)).h_full().max_h(u(LARGE_H)).child(base.child(body.flex_1().min_h_0().rounded_lg().bg(rgba(t.panel))))
}
