//! Renders the project connection form and path completions.

use gpui::{Context, IntoElement, div, prelude::*, px};

use crate::{
    app::{Dirigent, PathCompletionTarget},
    theme::{bg, blue, rgb, theme_text},
};

impl Dirigent {
    pub(super) fn render_add_project(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .p_8()
            .child(
                div()
                    .w_full()
                    .max_w(px(620.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(theme_text()))
                            .child("Create a project"),
                    )
                    .child(
                        div()
                            .h(px(28.0))
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .relative()
                                    .h_full()
                                    .min_w(px(0.0))
                                    .flex_1()
                                    .child(self.project_input.clone())
                                    .when(
                                        self.has_path_completion(PathCompletionTarget::Project),
                                        |element| {
                                            element.child(
                                                div()
                                                    .absolute()
                                                    .top(px(32.0))
                                                    .left_0()
                                                    .right_0()
                                                    .child(self.render_path_completion_menu(cx)),
                                            )
                                        },
                                    ),
                            )
                            .child(
                                div()
                                    .id("create-project")
                                    .h(px(28.0))
                                    .px_2()
                                    .py_1()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .rounded_md()
                                    .bg(rgb(blue()))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_xs()
                                    .text_color(rgb(bg()))
                                    .hover(|style| style.bg(rgb(crate::theme::accent_hover())))
                                    .on_click(cx.listener(|this, _, _, cx| this.add_project(cx)))
                                    .child("Create"),
                            ),
                    ),
            )
    }
}
