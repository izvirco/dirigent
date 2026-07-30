use gpui::{Context, IntoElement, div, prelude::*, px};

use crate::{
    app::Dirigent,
    model::Id,
    theme::{border, muted, red, rgb, surface, surface_hover, theme_text},
};

impl Dirigent {
    pub(crate) fn render_workspace_delete_dialog(
        &self,
        harness_id: Id,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = self
            .harnesses
            .iter()
            .find(|harness| harness.id == harness_id)
            .map(|harness| harness.title.clone())
            .unwrap_or_else(|| "this thread".into());
        let path = self
            .workspace_for_harness(harness_id)
            .map(|workspace| workspace.root.display().to_string())
            .unwrap_or_else(|| "the managed workspace".into());

        div()
            .id("workspace-delete-backdrop")
            .absolute()
            .inset_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p_8()
            .bg(gpui::rgba(0x000000aa))
            .on_click(cx.listener(|this, _, _, cx| {
                this.cancel_workspace_deletion();
                cx.notify();
            }))
            .child(
                div()
                    .id("workspace-delete-dialog")
                    .w_full()
                    .max_w(px(540.0))
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .rounded_xl()
                    .border_1()
                    .border_color(rgb(border()))
                    .bg(rgb(surface()))
                    .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(theme_text()))
                            .child("Delete thread and workspace?"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .text_sm()
                            .line_height(px(20.0))
                            .text_color(rgb(muted()))
                            .child(format!("“{title}” and its managed checkout will be removed."))
                            .child(
                                div()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis_middle()
                                    .child(path),
                            )
                            .child(
                                "Git branches and JJ commit history are preserved. Dirty Git worktrees are not removed.",
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .id("cancel-workspace-deletion")
                                    .h(px(34.0))
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(border()))
                                    .text_xs()
                                    .text_color(rgb(muted()))
                                    .hover(|style| style.bg(rgb(surface_hover())))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.cancel_workspace_deletion();
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id("confirm-workspace-deletion")
                                    .h(px(34.0))
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .rounded_lg()
                                    .bg(rgb(crate::theme::error_bg()))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_xs()
                                    .text_color(rgb(red()))
                                    .hover(|style| style.bg(rgb(crate::theme::error_bg())).opacity(0.8))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirm_workspace_deletion();
                                        cx.notify();
                                    }))
                                    .child("Delete thread and workspace"),
                            ),
                    ),
            )
    }
}
