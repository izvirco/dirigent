use gpui::{Context, IntoElement, div, prelude::*, px};

use crate::{
    app::Dirigent,
    model::Id,
    theme::{blue, border, muted, rgb, surface, surface_hover, theme_text},
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
        let custom = project
            .and_then(|project| project.workspace_root.as_ref())
            .is_some();
        let default_path = self
            .default_workspace_path_for_project(project_id)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Unavailable".into());

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
                                    .px_3()
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
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(muted()))
                                    .child("Changing this location affects new workspaces only."),
                            )
                            .child(
                                div()
                                    .p_4()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(if custom { border() } else { blue() }))
                                    .bg(rgb(surface()))
                                    .child(
                                        div()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_sm()
                                            .child("Default location"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(muted()))
                                            .child(default_path),
                                    )
                                    .child(
                                        div()
                                            .id("use-default-workspace-root")
                                            .h(px(30.0))
                                            .self_start()
                                            .px_3()
                                            .flex()
                                            .items_center()
                                            .rounded_md()
                                            .text_xs()
                                            .text_color(rgb(if custom { muted() } else { blue() }))
                                            .hover(|style| style.bg(rgb(surface_hover())))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.use_default_workspace_root(cx);
                                                cx.notify();
                                            }))
                                            .child(if custom { "Use default" } else { "In use" }),
                                    ),
                            )
                            .child(
                                div()
                                    .p_4()
                                    .flex()
                                    .flex_col()
                                    .gap_3()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(if custom { blue() } else { border() }))
                                    .bg(rgb(surface()))
                                    .child(
                                        div()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_sm()
                                            .child("Custom location"),
                                    )
                                    .child(self.workspace_settings_input.clone())
                                    .child(
                                        div()
                                            .id("save-custom-workspace-root")
                                            .h(px(32.0))
                                            .self_start()
                                            .px_4()
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
                                            .child("Save custom location"),
                                    ),
                            ),
                    ),
            )
    }
}
