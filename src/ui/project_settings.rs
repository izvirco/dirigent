use gpui::{Context, IntoElement, div, prelude::*, px};

use crate::{
    app::Dirigent,
    model::Id,
    theme::{blue, border, muted, rgb, surface_hover, theme_text},
};

impl Dirigent {
    pub(super) fn render_project_settings(
        &self,
        project_id: Id,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let project = self
            .projects
            .iter()
            .find(|project| project.id == project_id);
        let name = project
            .map(|project| project.name.clone())
            .unwrap_or_else(|| "Project".into());
        let default_path = self
            .default_workspace_path_for_project(project_id)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Unavailable".into());
        let current_path = project
            .and_then(|project| project.workspace_root.as_ref())
            .map(|path| path.display().to_string())
            .unwrap_or(default_path);
        let keep_active_threads_in_project =
            project.is_some_and(|project| project.keep_active_threads_in_project);

        div()
            .id("project-settings-scroll")
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
                                    .child(format!("{name} settings")),
                            )
                            .child(
                                div()
                                    .id("close-project-settings")
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
                                        this.close_project_settings();
                                        cx.notify();
                                    }))
                                    .child("Close"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme_text()))
                                    .child("Workspace location"),
                            )
                            .when(!self.workspace_settings_editing, |element| {
                                element.child(
                                    div()
                                        .h(px(28.0))
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w(px(0.0))
                                                .whitespace_nowrap()
                                                .overflow_hidden()
                                                .text_ellipsis_middle()
                                                .text_sm()
                                                .text_color(rgb(theme_text()))
                                                .child(current_path),
                                        )
                                        .child(
                                            div()
                                                .id("edit-workspace-root")
                                                .h(px(28.0))
                                                .px_2()
                                                .flex()
                                                .items_center()
                                                .rounded_md()
                                                .text_xs()
                                                .text_color(rgb(blue()))
                                                .hover(|style| style.bg(rgb(surface_hover())))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.begin_workspace_root_edit(cx);
                                                    cx.notify();
                                                }))
                                                .child("Edit"),
                                        ),
                                )
                            })
                            .when(self.workspace_settings_editing, |element| {
                                element.child(
                                    div()
                                        .h(px(28.0))
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .h_full()
                                                .flex_1()
                                                .min_w(px(0.0))
                                                .child(self.workspace_settings_input.clone()),
                                        )
                                        .child(
                                            div()
                                                .id("save-workspace-root")
                                                .h(px(28.0))
                                                .px_2()
                                                .flex()
                                                .items_center()
                                                .rounded_md()
                                                .bg(rgb(blue()))
                                                .text_xs()
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                .text_color(rgb(crate::theme::bg()))
                                                .hover(|style| {
                                                    style.bg(rgb(crate::theme::accent_hover()))
                                                })
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.save_custom_workspace_root(cx)
                                                }))
                                                .child("Save"),
                                        )
                                        .child(
                                            div()
                                                .id("cancel-workspace-root-edit")
                                                .h(px(28.0))
                                                .px_2()
                                                .flex()
                                                .items_center()
                                                .rounded_md()
                                                .text_xs()
                                                .text_color(rgb(muted()))
                                                .hover(|style| style.bg(rgb(surface_hover())))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.cancel_workspace_root_edit(cx);
                                                    cx.notify();
                                                }))
                                                .child("Cancel"),
                                        )
                                        .child(
                                            div()
                                                .id("reset-workspace-root")
                                                .h(px(28.0))
                                                .px_2()
                                                .flex()
                                                .items_center()
                                                .rounded_md()
                                                .border_1()
                                                .border_color(rgb(border()))
                                                .text_xs()
                                                .text_color(rgb(muted()))
                                                .hover(|style| style.bg(rgb(surface_hover())))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.use_default_workspace_root(cx);
                                                    cx.notify();
                                                }))
                                                .child("Reset"),
                                        ),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme_text()))
                                    .child("Thread placement"),
                            )
                            .child(
                                div()
                                    .id(("keep-active-threads-in-project", project_id as usize))
                                    .w_full()
                                    .p_3()
                                    .flex()
                                    .items_center()
                                    .gap_4()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(border()))
                                    .hover(|style| style.bg(rgb(surface_hover())))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_project_active_thread_placement(project_id);
                                        cx.notify();
                                    }))
                                    .child(
                                        div()
                                            .min_w(px(0.0))
                                            .flex_1()
                                            .flex()
                                            .flex_col()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                                    .text_color(rgb(theme_text()))
                                                    .child("Keep active threads inside this project"),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(muted()))
                                                    .child(
                                                        "Show this project's Inbox and Workpool in its sidebar section instead of the global sections.",
                                                    ),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .w(px(36.0))
                                            .h(px(20.0))
                                            .p(px(2.0))
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .rounded_full()
                                            .bg(rgb(if keep_active_threads_in_project {
                                                blue()
                                            } else {
                                                border()
                                            }))
                                            .when(keep_active_threads_in_project, |element| {
                                                element.justify_end()
                                            })
                                            .child(
                                                div()
                                                    .size(px(16.0))
                                                    .rounded_full()
                                                    .bg(rgb(crate::theme::bg())),
                                            ),
                                    ),
                            ),
                    ),
            )
    }
}
