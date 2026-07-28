use gpui::{
    AnyElement, BoxShadow, Context, Entity, Focusable, IntoElement, ObjectFit, PathBuilder,
    StyledImage, Window, canvas, div, img, point, prelude::*, px, rgb, rgba,
};

use crate::{
    app::{ComposerDropdown, Dirigent},
    model::{ContextUsage, HarnessStatus},
    text_input::TextInput,
    theme::{bg, blue, border, muted, surface, surface_hover, theme_text},
};

fn format_context_usage(usage: ContextUsage) -> String {
    let used = usage.used_tokens / 1_000;
    let total = usage.context_window / 1_000;
    format!("{used}k/{total}k")
}

pub(super) fn dropdown_arrow(open: bool) -> impl IntoElement {
    canvas(
        move |bounds, _, _| {
            let center = bounds.center();
            let endpoint_y = center.y + px(if open { 1.75 } else { -1.75 });
            let tip_y = center.y + px(if open { -1.75 } else { 1.75 });
            let mut chevron = PathBuilder::stroke(px(1.25));
            chevron.move_to(point(center.x - px(3.0), endpoint_y));
            chevron.line_to(point(center.x, tip_y));
            chevron.line_to(point(center.x + px(3.0), endpoint_y));
            chevron.build().ok()
        },
        |_, chevron, window, _| {
            if let Some(chevron) = chevron {
                window.paint_path(chevron, rgb(muted()));
            }
        },
    )
    .size(px(12.0))
    .flex_none()
}

impl Dirigent {
    fn render_new_harness_project_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let open = self.composer_dropdown == Some(ComposerDropdown::Project);
        let selected_project = self
            .selected_project
            .and_then(|id| self.projects.iter().find(|project| project.id == id));
        let selected_name = selected_project
            .map(|project| project.name.clone())
            .unwrap_or_else(|| "Select a project".into());

        div()
            .relative()
            .child(
                div()
                    .id("new-harness-project-picker")
                    .h(px(32.0))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(border()))
                    .bg(rgb(surface()))
                    .text_xs()
                    .cursor_pointer()
                    .when(open, |style| style.border_color(rgb(blue())))
                    .hover(|style| style.bg(rgb(surface_hover())))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_composer_dropdown(ComposerDropdown::Project);
                        cx.notify();
                        cx.stop_propagation();
                    }))
                    .child(div().text_color(rgb(muted())).child("Create thread in"))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .max_w(px(260.0))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(theme_text()))
                            .child(selected_name),
                    )
                    .child(dropdown_arrow(open)),
            )
            .when(open, |element| {
                element.child(
                    div()
                        .id("new-harness-project-dropdown")
                        .absolute()
                        .bottom(px(38.0))
                        .left_0()
                        .min_w(px(320.0))
                        .max_w(px(420.0))
                        .max_h(px(280.0))
                        .p_1()
                        .overflow_y_scroll()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(border()))
                        .bg(rgb(surface_hover()))
                        .occlude()
                        .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                        .children(self.projects.iter().enumerate().map(|(index, project)| {
                            let project_id = project.id;
                            let selected = self.selected_project == Some(project_id);
                            let name = project.name.clone();
                            let path = project.path.display().to_string();
                            div()
                                .id(("new-harness-project-option", index))
                                .min_h(px(42.0))
                                .mt(px(2.0))
                                .mb(px(2.0))
                                .px_3()
                                .py_1()
                                .flex()
                                .flex_col()
                                .justify_center()
                                .rounded_md()
                                .cursor_pointer()
                                .when(selected, |style| style.bg(rgb(crate::theme::selection())))
                                .when(!selected, |element| {
                                    element.hover(|style| style.bg(rgb(border())))
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if this.selected_project == Some(project_id) {
                                        this.composer_dropdown = None;
                                    } else {
                                        this.start_new_harness(project_id);
                                    }
                                    cx.notify();
                                    cx.stop_propagation();
                                }))
                                .child(
                                    div()
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .text_xs()
                                        .text_color(rgb(theme_text()))
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .text_xs()
                                        .text_color(rgb(muted()))
                                        .child(path),
                                )
                        })),
                )
            })
            .into_any_element()
    }

    fn render_model_picker(&self, current: &str, cx: &mut Context<Self>) -> AnyElement {
        let open = self.composer_dropdown == Some(ComposerDropdown::Model);
        let current = current.to_string();
        let model_label = current
            .split_once('/')
            .map_or(current.as_str(), |(_, model)| model)
            .to_string();
        div()
            .relative()
            .child(
                div()
                    .id("model-picker")
                    .h(px(26.0))
                    .max_w(px(260.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .text_xs()
                    .text_color(rgb(muted()))
                    .when(open, |style| {
                        style.bg(rgb(surface_hover())).text_color(rgb(theme_text()))
                    })
                    .hover(|style| style.bg(rgb(surface_hover())).text_color(rgb(theme_text())))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_composer_dropdown(ComposerDropdown::Model);
                        cx.notify();
                        cx.stop_propagation();
                    }))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(model_label),
                    )
                    .child(dropdown_arrow(open)),
            )
            .when(open, |element| {
                element.child(
                    div()
                        .id("model-dropdown")
                        .absolute()
                        .bottom(px(36.0))
                        .left_0()
                        .max_w(px(300.0))
                        .max_h(px(300.0))
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(border()))
                        .bg(rgb(surface_hover()))
                        .group("model-dropdown-scrollbar")
                        .occlude()
                        .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                        .child(
                            div()
                                .id("model-dropdown-content")
                                .max_h(px(298.0))
                                .overflow_y_scroll()
                                .track_scroll(&self.model_picker_scroll)
                                .p_1()
                                .pr_2()
                                .when(self.available_models.is_empty(), |element| {
                                    element.child(
                                        div()
                                            .h(px(38.0))
                                            .px_3()
                                            .flex()
                                            .items_center()
                                            .text_xs()
                                            .text_color(rgb(muted()))
                                            .child("Loading models…"),
                                    )
                                })
                                .children(self.available_models.iter().cloned().enumerate().map(
                                    |(index, model)| {
                                        let selected =
                                            current == format!("{}/{}", model.provider, model.id);
                                        let provider = model.provider.clone();
                                        let model_id = model.id.clone();
                                        div()
                                            .id(("model-option", index))
                                            .min_h(px(34.0))
                                            .mt(px(2.0))
                                            .mb(px(2.0))
                                            .px_3()
                                            .py_1()
                                            .flex()
                                            .items_center()
                                            .rounded_md()
                                            .when(selected, |style| {
                                                style.bg(rgb(crate::theme::selection()))
                                            })
                                            .when(!selected, |element| {
                                                element.hover(|style| style.bg(rgb(border())))
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.select_model(
                                                    provider.clone(),
                                                    model_id.clone(),
                                                );
                                                cx.notify();
                                            }))
                                            .child(
                                                div()
                                                    .min_w(px(0.0))
                                                    .flex()
                                                    .flex_col()
                                                    .child(
                                                        div()
                                                            .whitespace_nowrap()
                                                            .overflow_hidden()
                                                            .text_ellipsis()
                                                            .text_xs()
                                                            .text_color(rgb(theme_text()))
                                                            .child(model.name),
                                                    )
                                                    .child(
                                                        div()
                                                            .whitespace_nowrap()
                                                            .overflow_hidden()
                                                            .text_ellipsis()
                                                            .text_xs()
                                                            .text_color(rgb(muted()))
                                                            .child(format!(
                                                                "{}/{}",
                                                                model.provider, model.id
                                                            )),
                                                    ),
                                            )
                                    },
                                )),
                        )
                        .child(self.render_thin_scrollbar(
                            "model-dropdown-scrollbar",
                            &self.model_picker_scroll,
                        )),
                )
            })
            .into_any_element()
    }

    fn render_reasoning_picker(&self, current: &str, cx: &mut Context<Self>) -> AnyElement {
        let open = self.composer_dropdown == Some(ComposerDropdown::Reasoning);
        let current = current.to_string();
        div()
            .relative()
            .child(
                div()
                    .id("reasoning-picker")
                    .h(px(26.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .text_xs()
                    .text_color(rgb(muted()))
                    .when(open, |style| {
                        style.bg(rgb(surface_hover())).text_color(rgb(theme_text()))
                    })
                    .hover(|style| style.bg(rgb(surface_hover())).text_color(rgb(theme_text())))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_composer_dropdown(ComposerDropdown::Reasoning);
                        cx.notify();
                        cx.stop_propagation();
                    }))
                    .child(current.clone())
                    .child(dropdown_arrow(open)),
            )
            .when(open, |element| {
                element.child(
                    div()
                        .id("reasoning-dropdown")
                        .absolute()
                        .bottom(px(36.0))
                        .left_0()
                        .p_1()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(border()))
                        .bg(rgb(surface_hover()))
                        .occlude()
                        .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                        .children(self.reasoning_options().into_iter().enumerate().map(
                            |(index, level)| {
                                let selected = current == level;
                                let value = level.to_string();
                                div()
                                    .id(("reasoning-option", index))
                                    .h(px(26.0))
                                    .mt(px(2.0))
                                    .mb(px(2.0))
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(rgb(theme_text()))
                                    .when(selected, |style| {
                                        style.bg(rgb(crate::theme::selection()))
                                    })
                                    .when(!selected, |element| {
                                        element.hover(|style| style.bg(rgb(border())))
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.select_thinking(value.clone());
                                        cx.notify();
                                    }))
                                    .child(level)
                            },
                        )),
                )
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_prompt(
        &self,
        input: Entity<TextInput>,
        model: &str,
        thinking: &str,
        context_usage: Option<ContextUsage>,
        working: bool,
        creating: bool,
        nix_available: bool,
        nix_enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focused = input.focus_handle(cx).is_focused(window);
        let images = input.read(cx).images();
        div()
            .relative()
            .w_full()
            .max_w(px(820.0))
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_xl()
            .border_1()
            .border_color(rgba(0x00000000))
            .bg(rgb(surface()))
            .when(focused, |element| {
                element.border_color(rgb(blue()).opacity(0.5)).shadow(vec![
                    BoxShadow::new(px(0.0), px(0.0), rgb(blue()).opacity(0.19).into())
                        .blur_radius(px(5.0)),
                ])
            })
            .when(!images.is_empty(), |element| {
                element.child(
                    div()
                        .w_full()
                        .px_2()
                        .pt_1()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .children(images.into_iter().enumerate().map(|(index, attachment)| {
                            let preview = attachment.image.clone();
                            let label = attachment.label.clone();
                            let remove_label = attachment.label.clone();
                            let remove_input = input.clone();
                            div()
                                .id(("composer-image", index))
                                .relative()
                                .w(px(96.0))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_image_preview(preview.clone());
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .w(px(96.0))
                                        .h(px(68.0))
                                        .overflow_hidden()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(rgb(border()))
                                        .child(
                                            img(attachment.image)
                                                .size_full()
                                                .object_fit(ObjectFit::Cover),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(blue()))
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(format!("[{label}]")),
                                )
                                .child(
                                    div()
                                        .id(("remove-composer-image", index))
                                        .absolute()
                                        .top(px(-5.0))
                                        .right(px(-5.0))
                                        .size(px(18.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_full()
                                        .bg(rgb(border()))
                                        .text_xs()
                                        .text_color(rgb(theme_text()))
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            remove_input.update(cx, |input, cx| {
                                                input.remove_image(&remove_label, cx)
                                            });
                                            cx.stop_propagation();
                                        }))
                                        .child("×"),
                                )
                        })),
                )
            })
            .child(input)
            .child(
                div()
                    .w_full()
                    .pl_1()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(self.render_model_picker(model, cx))
                    .child(self.render_reasoning_picker(thinking, cx))
                    .when_some(context_usage, |element, usage| {
                        element.child(
                            div()
                                .h(px(26.0))
                                .px_2()
                                .flex()
                                .items_center()
                                .text_xs()
                                .text_color(rgb(muted()))
                                .child(format_context_usage(usage)),
                        )
                    })
                    .when(nix_available, |element| {
                        element.child(
                            div()
                                .id("nix-toggle")
                                .h(px(26.0))
                                .px_2()
                                .flex()
                                .items_center()
                                .rounded_md()
                                .text_xs()
                                .text_color(rgb(if nix_enabled { blue() } else { muted() }))
                                .when(nix_enabled, |style| {
                                    style.bg(rgb(crate::theme::accent_surface()))
                                })
                                .hover(|style| style.bg(rgb(surface_hover())))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.toggle_nix();
                                    cx.notify();
                                }))
                                .child("nix"),
                        )
                    })
                    .child(div().flex_1())
                    .when(working, |element| {
                        element.child(
                            div()
                                .id("abort-agent")
                                .h(px(30.0))
                                .px_3()
                                .flex()
                                .items_center()
                                .rounded_lg()
                                .text_xs()
                                .text_color(rgb(crate::theme::error_text()))
                                .hover(|style| style.bg(rgb(crate::theme::error_bg())))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.abort_selected();
                                    cx.notify();
                                }))
                                .child("Stop"),
                        )
                    })
                    .child(
                        div()
                            .id(if creating {
                                "start-harness"
                            } else {
                                "send-prompt"
                            })
                            .size(px(30.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgb(blue()))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgb(bg()))
                            .hover(|style| style.bg(rgb(crate::theme::accent_hover())))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if creating {
                                    this.create_harness(cx);
                                } else {
                                    this.send_composer(cx);
                                }
                            }))
                            .child("↑"),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn render_composer(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let harness = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id));
        let working = harness.is_some_and(|harness| harness.status == HarnessStatus::Working);
        let composer_input = self
            .selected_composer_input()
            .expect("selected harness must have a composer");
        let model = harness
            .and_then(|harness| harness.model.clone())
            .unwrap_or_else(|| "model loading".into());
        let thinking = harness
            .and_then(|harness| harness.thinking_level.clone())
            .unwrap_or_else(|| "loading".into());
        let context_usage = harness.and_then(|harness| harness.context_usage);
        let nix_available =
            harness.is_some_and(|harness| self.project_has_devshell(harness.project_id));
        let nix_enabled = harness.is_some_and(|harness| harness.nix_enabled);

        div()
            .w_full()
            .px_6()
            .pb_5()
            .pt_2()
            .flex_none()
            .flex()
            .justify_center()
            .child(self.render_prompt(
                composer_input,
                &model,
                &thinking,
                context_usage,
                working,
                false,
                nix_available,
                nix_enabled,
                window,
                cx,
            ))
    }

    pub(super) fn render_new_harness(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let model = self
            .draft_model
            .clone()
            .unwrap_or_else(|| "model loading".into());
        let thinking = self
            .draft_thinking_level
            .clone()
            .unwrap_or_else(|| "loading".into());
        let nix_available = self
            .selected_project
            .is_some_and(|project_id| self.project_has_devshell(project_id));

        div()
            .w_full()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .px_6()
            .child(
                div()
                    .w_full()
                    .max_w(px(820.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .child(self.render_new_harness_project_picker(cx)),
                    )
                    .child(self.render_prompt(
                        self.harness_input.clone(),
                        &model,
                        &thinking,
                        None,
                        false,
                        true,
                        nix_available,
                        self.draft_nix_enabled,
                        window,
                        cx,
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::format_context_usage;
    use crate::model::ContextUsage;

    #[test]
    fn formats_used_and_total_context_in_thousands() {
        assert_eq!(
            format_context_usage(ContextUsage {
                used_tokens: 60_000,
                context_window: 200_000,
            }),
            "60k/200k"
        );
    }
}
