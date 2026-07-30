use super::*;

impl Dirigent {
    pub(super) fn render_message_action(
        &self,
        id: String,
        label: &'static str,
        text: SharedString,
        index: usize,
        visible: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let copied = label == "Copy"
            && self
                .copied_button
                .as_ref()
                .is_some_and(|(button_id, _)| button_id == &id);
        let shown = visible || copied;
        let clicked_id = id.clone();
        div()
            .id(id)
            .h(px(22.0))
            .px_2()
            .flex_none()
            .flex()
            .items_center()
            .rounded_md()
            .text_xs()
            .text_color(rgb(if copied { blue() } else { muted() }))
            .opacity(if shown { 1.0 } else { 0.0 })
            .when(copied, |element| element.bg(rgb(blue()).opacity(0.15)))
            .when(shown, |element| {
                element
                    .cursor_pointer()
                    .when(!copied, |element| {
                        element.hover(|style| style.bg(rgb(surface_hover())))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        match label {
                            "Copy" => this.copy_text_with_feedback(
                                clicked_id.clone(),
                                text.to_string(),
                                cx,
                            ),
                            "Fork" => this.begin_message_fork(index, cx),
                            "Edit" => this.begin_message_edit(index, cx),
                            _ => {}
                        }
                        cx.stop_propagation();
                    }))
            })
            .child(label)
            .into_any_element()
    }
    pub(super) fn render_message_actions(
        &self,
        message: &Message,
        index: usize,
        visible: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let stable =
            self.session_actions_available() && message.entry_id.is_some() && !message.queued;
        let idle = self.selected_harness.is_some_and(|id| {
            self.harnesses
                .iter()
                .find(|harness| harness.id == id)
                .is_some_and(|harness| harness.status != HarnessStatus::Working)
        });
        let labels: &[&str] = match message.role {
            MessageRole::User if stable && idle => &["Copy", "Edit", "Fork"],
            MessageRole::Assistant if stable && idle => &["Copy", "Fork"],
            MessageRole::User
            | MessageRole::Assistant
            | MessageRole::Thinking
            | MessageRole::Tool
            | MessageRole::Notice
            | MessageRole::Error => &["Copy"],
        };
        let estimated_lines = message
            .display_text
            .lines()
            .map(|line| line.chars().count().max(1).div_ceil(88))
            .sum::<usize>()
            .max(1);
        let line_height = if message.role == MessageRole::Assistant {
            22
        } else {
            21
        };
        let estimated_height = estimated_lines * line_height
            + usize::from(!message.images.is_empty()) * 36
            + usize::from(message.display_detail.is_some()) * 40;
        let required_height = labels.len() * 22;
        let vertical = labels.len() > 1 && estimated_height >= required_height;
        let copy_text = message.copy_text.clone();
        let hover_key = self.selected_harness.map(|harness_id| (harness_id, index));
        div()
            .id(("message-actions", index))
            .absolute()
            .top_0()
            .bottom_0()
            .left(relative(1.0))
            .pl_2()
            .w(px(140.0))
            .flex()
            .on_hover(cx.listener(move |this, hovered, _, cx| {
                let hovered = if *hovered { hover_key } else { None };
                if hovered.is_some() || this.hovered_action_message == hover_key {
                    this.hovered_action_message = hovered;
                    cx.notify();
                }
            }))
            .when(vertical, |element| element.flex_col().items_start())
            .gap(px(0.0))
            .children(labels.iter().map(|label| {
                self.render_message_action(
                    format!("{}-message-{index}", label.to_ascii_lowercase()),
                    label,
                    copy_text.clone(),
                    index,
                    visible,
                    cx,
                )
            }))
            .into_any_element()
    }
    pub(super) fn render_message_edit_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let edit = self.editing_message.as_ref().expect("edit must exist");
        let index = edit.message_index;
        let input = edit.input.clone();
        let model = edit.model.clone();
        let thinking = edit.thinking.clone();
        let submitting = edit.submitting;
        let model_open = self.composer_dropdown == Some(ComposerDropdown::EditModel);
        let thinking_open = self.composer_dropdown == Some(ComposerDropdown::EditReasoning);
        let model_label = model
            .split_once('/')
            .map_or(model.as_str(), |(_, model)| model)
            .to_string();
        let model_picker = div()
            .relative()
            .child(
                div()
                    .id(("edit-model-picker", index))
                    .h(px(26.0))
                    .max_w(px(260.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .text_xs()
                    .text_color(rgb(muted()))
                    .when(model_open, |style| {
                        style.bg(rgb(surface_hover())).text_color(rgb(theme_text()))
                    })
                    .hover(|style| style.bg(rgb(surface_hover())).text_color(rgb(theme_text())))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_composer_dropdown(ComposerDropdown::EditModel);
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
                    .child(dropdown_arrow(model_open)),
            )
            .when(model_open, |element| {
                element.child(
                    div()
                        .id(("edit-model-dropdown", index))
                        .absolute()
                        .bottom(px(36.0))
                        .left_0()
                        .max_w(px(300.0))
                        .max_h(px(300.0))
                        .p_1()
                        .overflow_y_scroll()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(crate::theme::border()))
                        .bg(rgb(surface_hover()))
                        .occlude()
                        .children(self.available_models.iter().enumerate().map(
                            |(option, item)| {
                                let selected = model == format!("{}/{}", item.provider, item.id);
                                let provider = item.provider.clone();
                                let model_id = item.id.clone();
                                let value = format!("{provider}/{model_id}");
                                div()
                                    .id(("edit-model-option", option))
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
                                        element.hover(|style| style.bg(rgb(crate::theme::border())))
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.select_edit_model(value.clone());
                                        cx.notify();
                                        cx.stop_propagation();
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
                                                    .child(item.name.clone()),
                                            )
                                            .child(
                                                div()
                                                    .whitespace_nowrap()
                                                    .overflow_hidden()
                                                    .text_ellipsis()
                                                    .text_xs()
                                                    .text_color(rgb(muted()))
                                                    .child(format!("{provider}/{model_id}")),
                                            ),
                                    )
                            },
                        )),
                )
            });
        let thinking_picker = div()
            .relative()
            .child(
                div()
                    .id(("edit-thinking-picker", index))
                    .h(px(26.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .text_xs()
                    .text_color(rgb(muted()))
                    .when(thinking_open, |style| {
                        style.bg(rgb(surface_hover())).text_color(rgb(theme_text()))
                    })
                    .hover(|style| style.bg(rgb(surface_hover())).text_color(rgb(theme_text())))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_composer_dropdown(ComposerDropdown::EditReasoning);
                        cx.notify();
                        cx.stop_propagation();
                    }))
                    .child(thinking.clone())
                    .child(dropdown_arrow(thinking_open)),
            )
            .when(thinking_open, |element| {
                element.child(
                    div()
                        .absolute()
                        .bottom(px(36.0))
                        .left_0()
                        .p_1()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(crate::theme::border()))
                        .bg(rgb(surface_hover()))
                        .occlude()
                        .children(
                            self.edit_reasoning_options(&model)
                                .into_iter()
                                .enumerate()
                                .map(|(option, level)| {
                                    let selected = thinking == level;
                                    let value = level.clone();
                                    div()
                                        .id(("edit-thinking-option", option))
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
                                            element.hover(|style| {
                                                style.bg(rgb(crate::theme::border()))
                                            })
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.select_edit_thinking(value.clone());
                                            cx.notify();
                                            cx.stop_propagation();
                                        }))
                                        .child(level)
                                }),
                        ),
                )
            });
        div()
            .w_full()
            .max_w(px(820.0))
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_lg()
            .border_1()
            .border_color(rgb(blue()).opacity(0.55))
            .bg(rgb(crate::theme::surface()))
            .child(input)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(model_picker)
                    .child(thinking_picker)
                    .child(div().flex_1())
                    .child(
                        div()
                            .id(("cancel-message-edit", index))
                            .h(px(28.0))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .text_xs()
                            .text_color(rgb(muted()))
                            .when(!submitting, |element| {
                                element
                                    .hover(|style| style.bg(rgb(surface_hover())))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.cancel_message_edit(cx)),
                                    )
                            })
                            .child("Cancel"),
                    )
                    .child(
                        div()
                            .id(("submit-message-edit", index))
                            .size(px(30.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgb(blue()))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgb(crate::theme::bg()))
                            .hover(|style| style.bg(rgb(crate::theme::accent_hover())))
                            .when(!submitting, |element| {
                                element.on_click(
                                    cx.listener(|this, _, _, cx| this.submit_message_edit(cx)),
                                )
                            })
                            .child(if submitting { "…" } else { "↑" }),
                    ),
            )
            .into_any_element()
    }
}
