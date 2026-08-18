//! Renders Dirigent build and release information.

use gpui::{Context, IntoElement, div, prelude::*, px};

use crate::{
    app::Dirigent,
    theme::{border, muted, rgb, surface_hover, theme_text},
    update,
};

fn info_row(label: &'static str, value: &'static str) -> impl IntoElement {
    div()
        .h(px(30.0))
        .flex()
        .items_center()
        .gap_4()
        .border_b_1()
        .border_color(rgb(border()))
        .text_sm()
        .child(div().w(px(100.0)).text_color(rgb(muted())).child(label))
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .text_color(rgb(theme_text()))
                .child(value),
        )
}

impl Dirigent {
    pub(super) fn render_about(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("about-scroll")
            .flex_1()
            .overflow_y_scroll()
            .p_8()
            .child(
                div()
                    .w_full()
                    .max_w(px(680.0))
                    .mx_auto()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .text_xl()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme_text()))
                                    .child("About Dirigent"),
                            )
                            .child(
                                div()
                                    .id("close-about")
                                    .h(px(32.0))
                                    .px_2()
                                    .py_1()
                                    .flex()
                                    .items_center()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(rgb(muted()))
                                    .hover(|style| {
                                        style.bg(rgb(surface_hover())).text_color(rgb(theme_text()))
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.about_open = false;
                                        cx.notify();
                                    }))
                                    .child("Close"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .pb_2()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme_text()))
                                    .child("Build information"),
                            )
                            .child(info_row("Version", update::current_version()))
                            .child(info_row("Commit", env!("DIRIGENT_COMMIT_ID")))
                            .child(info_row("Target", update::update_target()))
                            .child(info_row("Channel", update::channel())),
                    ),
            )
    }
}
