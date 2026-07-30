use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ObjectFit, ScrollHandle, SharedString, StyledImage, Window,
    canvas, div, img, list, prelude::*, px, relative,
};

use super::composer::dropdown_arrow;

use crate::{
    app::{ComposerDropdown, Dirigent},
    model::{HarnessStatus, Message, MessageRole, RetryStatus},
    theme::{
        blue, green, muted, orange, purple, rgb, surface_hover, theme_text, thinking_text, yellow,
    },
};

fn format_working_duration(duration: Duration) -> String {
    let elapsed = duration.as_secs();
    if elapsed < 60 {
        format!("{elapsed}s")
    } else if elapsed < 3_600 {
        format!("{}m {:02}s", elapsed / 60, elapsed % 60)
    } else {
        format!("{}h {:02}m", elapsed / 3_600, (elapsed % 3_600) / 60)
    }
}

fn working_character(delta: f32, text: &str) -> usize {
    let character_count = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .count();
    let active = ((delta * character_count as f32) as usize).min(character_count.saturating_sub(1));
    text.chars()
        .enumerate()
        .filter(|(_, character)| !character.is_whitespace())
        .nth(active)
        .map_or(0, |(index, _)| index)
}

fn trailing_working_character(text: &str, active: usize) -> Option<usize> {
    text.chars()
        .enumerate()
        .take(active)
        .filter(|(_, character)| !character.is_whitespace())
        .map(|(index, _)| index)
        .last()
}

fn blend_colors(first: u32, second: u32) -> u32 {
    [24, 16, 8, 0].into_iter().fold(0, |blended, shift| {
        let channel = (((first >> shift) & 0xff) + ((second >> shift) & 0xff)) / 2;
        blended | (channel << shift)
    })
}

fn format_retry_status(retry: &RetryStatus) -> String {
    if retry.waiting {
        let seconds = retry.delay_ms as f64 / 1_000.0;
        format!(
            "Automatic retry {}/{} in {seconds:.1}s",
            retry.attempt, retry.max_attempts
        )
    } else {
        format!(
            "Automatic retry {}/{} running",
            retry.attempt, retry.max_attempts
        )
    }
}

fn tool_color(tool: &str) -> u32 {
    match tool {
        "read" => green(),
        "edit" | "write" => yellow(),
        "compact" => purple(),
        _ => blue(),
    }
}

impl Dirigent {
    fn render_message_action(
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

    fn render_message_actions(
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

    fn render_tool_detail(
        &self,
        detail: SharedString,
        colors: &[(std::ops::Range<usize>, u32)],
        index: usize,
        scroll: &ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let scrollbar_id = format!("tool-scrollbar-{index}");
        let hover_key = self.selected_harness.map(|harness_id| (harness_id, index));
        div()
            .id(("tool-detail-hover", index))
            .relative()
            .group(scrollbar_id.clone())
            .ml_5()
            .pb_2()
            .max_h(px(260.0))
            .on_hover(cx.listener(move |this, hovered, _, cx| {
                let hovered = if *hovered { hover_key } else { None };
                if hovered.is_some() || this.hovered_tool_detail_message == hover_key {
                    this.hovered_tool_detail_message = hovered;
                    cx.notify();
                }
            }))
            // Keep the list hitbox behind this nested scroll area from handling the same wheel event.
            .occlude()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id(("tool-output", index))
                    .max_h(px(258.0))
                    .pr_2()
                    .overflow_y_scroll()
                    .track_scroll(scroll)
                    .text_xs()
                    .line_height(px(18.0))
                    .text_color(rgb(crate::theme::detail_text()))
                    .child(self.render_selectable_text(
                        format!("tool-output-{index}"),
                        detail,
                        colors,
                        cx,
                    )),
            )
            .child(self.render_thin_scrollbar(scrollbar_id, scroll))
            .into_any_element()
    }

    fn render_message(
        &self,
        message: &Message,
        index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.editing_message.as_ref().is_some_and(|edit| {
            edit.harness_id == self.selected_harness.unwrap_or_default()
                && edit.message_index == index
        }) {
            return self.render_message_edit_composer(cx);
        }
        let hover_key = self.selected_harness.map(|harness_id| (harness_id, index));
        let actions_visible = self.hovered_copy_message == hover_key
            || self.hovered_action_message == hover_key
            || self.hovered_tool_detail_message == hover_key;
        match message.role {
            MessageRole::User => {
                let images = message.images.clone();
                let queued = message.queued;
                div()
                    .id(("user-message", index))
                    .relative()
                    .w_full()
                    .pr_3()
                    .border_r_4()
                    .border_color(rgb(if queued { orange() } else { blue() }))
                    .flex()
                    .items_start()
                    .gap_2()
                    .text_sm()
                    .line_height(px(21.0))
                    .text_color(rgb(theme_text()))
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        let hovered = if *hovered { hover_key } else { None };
                        if hovered.is_some() || this.hovered_copy_message == hover_key {
                            this.hovered_copy_message = hovered;
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(self.render_selectable_text(
                                format!("user-message-text-{index}"),
                                message.display_text.clone(),
                                &[],
                                cx,
                            ))
                            .when(!images.is_empty(), |element| {
                                element.child(div().flex().flex_wrap().gap_1().children(
                                    images.into_iter().enumerate().map(|(image_index, image)| {
                                        let preview = image.clone();
                                        div()
                                            .id(("sent-image", index * 1_000 + image_index))
                                            .w(px(42.0))
                                            .h(px(32.0))
                                            .overflow_hidden()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(rgb(crate::theme::border_emphasized()))
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.open_image_preview(preview.clone());
                                                cx.stop_propagation();
                                                cx.notify();
                                            }))
                                            .child(
                                                img(image).size_full().object_fit(ObjectFit::Cover),
                                            )
                                    }),
                                ))
                            }),
                    )
                    .child(self.render_message_actions(message, index, actions_visible, cx))
                    .into_any_element()
            }
            MessageRole::Assistant => div()
                .id(("assistant-message", index))
                .relative()
                .w_full()
                .flex()
                .items_start()
                .text_sm()
                .line_height(px(22.0))
                .text_color(rgb(crate::theme::code_text()))
                .on_hover(cx.listener(move |this, hovered, _, cx| {
                    let hovered = if *hovered { hover_key } else { None };
                    if hovered.is_some() || this.hovered_copy_message == hover_key {
                        this.hovered_copy_message = hovered;
                        cx.notify();
                    }
                }))
                .child(div().w_full().min_w(px(0.0)).flex_1().child(
                    if let Some(markdown) = message.markdown.as_ref() {
                        self.render_markdown(markdown, index, cx)
                    } else {
                        self.render_selectable_text(
                            format!("assistant-message-text-{index}"),
                            message.display_text.clone(),
                            &[],
                            cx,
                        )
                    },
                ))
                .child(self.render_message_actions(message, index, actions_visible, cx))
                .into_any_element(),
            MessageRole::Thinking => {
                let text = message.display_text.clone();
                div()
                    .id(("thinking-message", index))
                    .relative()
                    .w_full()
                    .min_h(px(18.0))
                    .flex()
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        let hovered = if *hovered { hover_key } else { None };
                        if hovered.is_some() || this.hovered_copy_message == hover_key {
                            this.hovered_copy_message = hovered;
                            cx.notify();
                        }
                    }))
                    .items_start()
                    .gap_2()
                    .text_xs()
                    .line_height(px(18.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(rgb(thinking_text()))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .child(self.render_selectable_text(
                                format!("thinking-message-text-{index}"),
                                text,
                                &[],
                                cx,
                            )),
                    )
                    .child(self.render_message_actions(message, index, actions_visible, cx))
                    .into_any_element()
            }
            MessageRole::Tool => {
                let selected_harness = self.selected_harness;
                let expanded = message.expanded;
                let tool = message
                    .text
                    .split_once(' ')
                    .map_or(message.text.as_str(), |(tool, _)| tool);
                let tool_color = tool_color(tool);
                let label_id = format!("tool-label-{index}");
                let label_colors = vec![(0..tool.len(), tool_color)];
                let label = if expanded {
                    self.render_selectable_text(
                        label_id.clone(),
                        message.display_text.clone(),
                        &label_colors,
                        cx,
                    )
                } else {
                    self.render_single_line_selectable_text(
                        label_id.clone(),
                        message.display_text.clone(),
                        &label_colors,
                        cx,
                    )
                };
                div()
                    .id(("tool-message", index))
                    .relative()
                    .w_full()
                    .flex()
                    .flex_col()
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        let hovered = if *hovered { hover_key } else { None };
                        if hovered.is_some() || this.hovered_copy_message == hover_key {
                            this.hovered_copy_message = hovered;
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .id(("toggle-tool", index))
                            .w_full()
                            .min_h(px(22.0))
                            .flex()
                            .items_start()
                            .text_xs()
                            .text_color(rgb(muted()))
                            .hover(|style| {
                                style.text_color(rgb(theme_text())).bg(rgb(surface_hover()))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let text_selected = this
                                    .thread_text_selection
                                    .as_ref()
                                    .is_some_and(|selection| {
                                        selection.id == label_id && !selection.range.is_empty()
                                    });
                                if text_selected {
                                    return;
                                }
                                if let Some(harness) = selected_harness.and_then(|id| {
                                    this.harnesses.iter_mut().find(|harness| harness.id == id)
                                }) && let Some(message) = harness.messages.get_mut(index)
                                {
                                    message.expanded = !message.expanded;
                                    this.conversation_list.remeasure_items(index..index + 1);
                                    cx.notify();
                                }
                            }))
                            .child(div().min_w(px(0.0)).flex_1().child(label))
                            .child(self.render_message_actions(
                                message,
                                index,
                                actions_visible,
                                cx,
                            )),
                    )
                    .when(expanded, |element| {
                        element.when_some(message.display_detail.clone(), |element, detail| {
                            element.child(self.render_tool_detail(
                                detail,
                                &message.detail_colors,
                                index,
                                &message.detail_scroll,
                                cx,
                            ))
                        })
                    })
                    .into_any_element()
            }
            MessageRole::Notice | MessageRole::Error => {
                let error = message.role == MessageRole::Error;
                div()
                    .id(("notice-message", index))
                    .relative()
                    .w_full()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(if error {
                        crate::theme::error_bg()
                    } else {
                        crate::theme::notice_bg()
                    }))
                    .flex()
                    .items_start()
                    .gap_2()
                    .text_xs()
                    .line_height(px(18.0))
                    .text_color(rgb(if error {
                        crate::theme::error_text()
                    } else {
                        muted()
                    }))
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        let hovered = if *hovered { hover_key } else { None };
                        if hovered.is_some() || this.hovered_copy_message == hover_key {
                            this.hovered_copy_message = hovered;
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .child(self.render_selectable_text(
                                format!("notice-message-text-{index}"),
                                message.display_text.clone(),
                                &[],
                                cx,
                            )),
                    )
                    .child(self.render_message_actions(message, index, actions_visible, cx))
                    .into_any_element()
            }
        }
    }

    fn render_conversation_ruler(
        &self,
        messages: &[Message],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let max_offset = self.conversation_list.max_offset_for_scrollbar().y.as_f32();
        let viewport = self
            .conversation_list
            .viewport_bounds()
            .size
            .height
            .as_f32();
        let viewport_fraction = if viewport > 0.0 && max_offset > 0.0 {
            let min_fraction = (15.0 / viewport).clamp(0.06, 1.0);
            (viewport / (viewport + max_offset)).clamp(min_fraction, 1.0)
        } else {
            1.0
        };
        let scroll_fraction = if max_offset > 0.0 {
            (-self
                .conversation_list
                .scroll_px_offset_for_scrollbar()
                .y
                .as_f32()
                / max_offset)
                .clamp(0.0, 1.0)
        } else {
            0.0
        };
        let viewport_top = (1.0 - viewport_fraction) * scroll_fraction;
        let message_count = messages.len().max(1) as f32;
        let entity = cx.entity();

        div()
            .id("conversation-ruler")
            .absolute()
            .top(relative(1.0 / 4.0))
            .left(px(10.0))
            .h(relative(1.0 / 2.0))
            .w(px(18.0))
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .justify_between()
                    .py(px(1.0))
                    .children((0..17).map(|_| {
                        div()
                            .h(px(1.0))
                            .w(px(14.0))
                            .mx_auto()
                            .flex_none()
                            .bg(rgb(orange()).opacity(0.60))
                    })),
            )
            .children(messages.iter().enumerate().filter_map(|(index, message)| {
                let (color, is_compaction) = match message.role {
                    MessageRole::User => (blue(), false),
                    MessageRole::Assistant => (crate::theme::detail_text(), false),
                    MessageRole::Tool if message.is_compaction() => (purple(), true),
                    _ => return None,
                };
                Some(
                    div()
                        .absolute()
                        .top(relative((index as f32 + 0.5) / message_count))
                        .when(is_compaction, |marker| {
                            marker.left(px(2.0)).right(px(2.0)).h(px(6.0))
                        })
                        .when(!is_compaction, |marker| {
                            marker.left(px(6.0)).size(px(6.0)).rounded_full()
                        })
                        .bg(rgb(color)),
                )
            }))
            .child(
                div()
                    .absolute()
                    .top(relative(viewport_top))
                    .left_0()
                    .right_0()
                    .h(relative(viewport_fraction))
                    .min_h(px(5.0))
                    .border_t_1()
                    .border_b_1()
                    .border_color(rgb(crate::theme::warning_border()))
                    .bg(rgb(orange()).opacity(0.09)),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |track_bounds, _, window, _| {
                        window.on_mouse_event({
                            let entity = entity.clone();
                            move |event: &MouseDownEvent, _, _, cx| {
                                if event.button != MouseButton::Left
                                    || !track_bounds.contains(&event.position)
                                {
                                    return;
                                }
                                let fraction = ((event.position.y - track_bounds.top())
                                    / track_bounds.size.height)
                                    .clamp(0.0, 1.0);
                                entity.update(cx, |this, cx| {
                                    this.conversation_scroll_dragging = true;
                                    this.conversation_list.scrollbar_drag_started();
                                    this.scroll_conversation_to_fraction(fraction);
                                    cx.notify();
                                });
                                cx.stop_propagation();
                            }
                        });
                        window.on_mouse_event({
                            let entity = entity.clone();
                            move |event: &MouseMoveEvent, _, _, cx| {
                                if !event.dragging()
                                    || !entity.read(cx).conversation_scroll_dragging
                                {
                                    return;
                                }
                                let fraction = ((event.position.y - track_bounds.top())
                                    / track_bounds.size.height)
                                    .clamp(0.0, 1.0);
                                entity.update(cx, |this, cx| {
                                    this.scroll_conversation_to_fraction(fraction);
                                    cx.notify();
                                });
                                cx.stop_propagation();
                            }
                        });
                        window.on_mouse_event(move |_: &MouseUpEvent, _, _, cx| {
                            if entity.read(cx).conversation_scroll_dragging {
                                entity.update(cx, |this, cx| {
                                    this.conversation_scroll_dragging = false;
                                    this.conversation_list.scrollbar_drag_ended();
                                    cx.notify();
                                });
                                cx.stop_propagation();
                            }
                        });
                    },
                )
                .absolute()
                .inset_0(),
            )
            .into_any_element()
    }

    fn render_conversation_item(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let harness = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
            .expect("selected harness must exist");

        if let Some(message) = harness.messages.get(index) {
            let follows_activity = index > 0
                && matches!(message.role, MessageRole::Thinking | MessageRole::Tool)
                && matches!(
                    harness.messages[index - 1].role,
                    MessageRole::Thinking | MessageRole::Tool
                );
            return div()
                .w_full()
                .child(
                    div()
                        .w_full()
                        .max_w(px(820.0))
                        .mx_auto()
                        .px_7()
                        .when(index > 0 && !follows_activity, |element| element.mt_2())
                        .child(self.render_message(message, index, cx)),
                )
                .into_any_element();
        }

        let working = harness.status == HarnessStatus::Working;
        if working && index == harness.messages.len() {
            if let Some(retry) = harness.retry_status.as_ref() {
                return div()
                    .w_full()
                    .child(
                        div()
                            .w_full()
                            .max_w(px(820.0))
                            .mx_auto()
                            .px_7()
                            .mt_4()
                            .child(
                                div()
                                    .w_full()
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .bg(rgb(orange()).opacity(0.10))
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .text_xs()
                                    .line_height(px(18.0))
                                    .text_color(rgb(orange()))
                                    .child(format_retry_status(retry))
                                    .child(
                                        div()
                                            .text_color(rgb(muted()))
                                            .child(retry.error_message.clone()),
                                    ),
                            ),
                    )
                    .into_any_element();
            }

            let run_started_at = harness.run_started_at;
            return div()
                .w_full()
                .child(
                    div()
                        .w_full()
                        .max_w(px(820.0))
                        .mx_auto()
                        .px_7()
                        .mt_4()
                        .flex()
                        .text_xs()
                        .with_animation(
                            "working-indicator",
                            Animation::new(Duration::from_millis(6_660)).repeat(),
                            move |indicator, delta| {
                                let elapsed = run_started_at
                                    .map(|started_at| started_at.elapsed())
                                    .unwrap_or_default();
                                let label =
                                    format!("Working for {}", format_working_duration(elapsed));
                                let active = working_character(delta, &label);
                                let trailing = trailing_working_character(&label, active);
                                let trailing_color = blend_colors(orange(), blue());
                                indicator.children(label.chars().enumerate().map(
                                    move |(index, character)| {
                                        let color = if index == active {
                                            orange()
                                        } else if Some(index) == trailing {
                                            trailing_color
                                        } else {
                                            blue()
                                        };
                                        div().text_color(rgb(color)).child(character.to_string())
                                    },
                                ))
                            },
                        ),
                )
                .into_any_element();
        }

        let queued_start = harness.messages.len() + usize::from(working);
        if let Some(message) = index
            .checked_sub(queued_start)
            .and_then(|queued_index| harness.queued_messages.get(queued_index))
        {
            return div()
                .w_full()
                .child(
                    div()
                        .w_full()
                        .max_w(px(820.0))
                        .mx_auto()
                        .px_7()
                        .mt_2()
                        .child(self.render_message(message, index, cx)),
                )
                .into_any_element();
        }

        div().into_any_element()
    }

    pub(super) fn render_conversation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let harness = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
            .expect("selected harness must exist");
        let empty = harness.messages.is_empty() && harness.queued_messages.is_empty();
        let empty_label = if harness.loaded_messages || harness.cached_entries.is_some() {
            "This pi session has no messages yet."
        } else {
            "Loading conversation…"
        };

        div()
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .child(
                div()
                    .id("conversation-scroll")
                    .size_full()
                    .child(
                        list(
                            self.conversation_list.clone(),
                            cx.processor(Self::render_conversation_item),
                        )
                        .size_full()
                        .py_7(),
                    )
                    .when(empty, |element| {
                        element.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .right_0()
                                .py_12()
                                .text_center()
                                .text_sm()
                                .text_color(rgb(muted()))
                                .child(empty_label),
                        )
                    }),
            )
            .child(self.render_conversation_ruler(&harness.messages, cx))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        blend_colors, format_retry_status, format_working_duration, tool_color,
        trailing_working_character, working_character,
    };
    use crate::{
        model::RetryStatus,
        theme::{purple, yellow},
    };

    #[test]
    fn formats_waiting_and_running_retry_status() {
        let mut retry = RetryStatus {
            attempt: 2,
            max_attempts: 3,
            delay_ms: 2_500,
            error_message: "overloaded".into(),
            waiting: true,
        };
        assert_eq!(format_retry_status(&retry), "Automatic retry 2/3 in 2.5s");
        retry.waiting = false;
        assert_eq!(format_retry_status(&retry), "Automatic retry 2/3 running");
    }

    #[test]
    fn formats_working_duration() {
        assert_eq!(format_working_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_working_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(
            format_working_duration(Duration::from_secs(3_661)),
            "1h 01m"
        );
    }

    #[test]
    fn working_character_moves_across_the_visible_text() {
        assert_eq!(working_character(0.0, "ab cd"), 0);
        assert_eq!(working_character(0.25, "ab cd"), 1);
        assert_eq!(working_character(0.5, "ab cd"), 3);
        assert_eq!(working_character(0.75, "ab cd"), 4);
        assert_eq!(working_character(1.0, "ab cd"), 4);
    }

    #[test]
    fn trailing_working_character_skips_spaces() {
        assert_eq!(trailing_working_character("ab cd", 0), None);
        assert_eq!(trailing_working_character("ab cd", 3), Some(1));
        assert_eq!(trailing_working_character("ab cd", 4), Some(3));
    }

    #[test]
    fn blends_each_color_channel_evenly() {
        assert_eq!(blend_colors(0xff0000ff, 0x0000ffff), 0x7f007fff);
    }

    #[test]
    fn compaction_tools_are_purple() {
        assert_eq!(tool_color("compact"), purple());
    }

    #[test]
    fn write_tools_are_yellow() {
        assert_eq!(tool_color("write"), yellow());
    }
}
