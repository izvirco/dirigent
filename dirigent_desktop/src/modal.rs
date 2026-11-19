use crate::app::Dirigent;
use crate::theme::{self, u};
use gpui::{AnyElement, Context, Div, MouseButton, div, prelude::*, rgba};

const LARGE_W: f32 = 900.;
const LARGE_H: f32 = 640.;

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

pub fn large(body: Div) -> Div {
    let t = theme::t();
    let base = theme::stack().flex_1().min_h_0().rounded_lg().bg(rgba(theme::alpha(t.canvas, 0xff)));
    div()
        .w_full()
        .max_w(u(LARGE_W))
        .h_full()
        .max_h(u(LARGE_H))
        .child(base.child(body.w_full().flex_1().min_h_0().rounded_lg().bg(rgba(t.panel))))
}
