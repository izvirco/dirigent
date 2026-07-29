use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ObjectFit, ScrollHandle, SharedString, StyledImage, Window,
    canvas, div, img, list, prelude::*, px, relative,
};

use crate::{
    app::Dirigent,
    model::{HarnessStatus, Message, MessageRole, RetryStatus},
    theme::{
        blue, green, muted, orange, purple, rgb, surface_hover, theme_text, thinking_text, yellow,
    },
};

fn working_dot(delta: f32) -> usize {
    let position = if delta <= 0.5 {
        delta * 4.0
    } else {
        (1.0 - delta) * 4.0
    };
    (position.round() as usize).min(2)
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
    fn render_copy_button(
        &self,
        id: impl Into<String>,
        text: SharedString,
        visible: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = id.into();
        let copied = self
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
                    .cursor_default()
                    .when(!copied, |element| {
                        element.hover(|style| style.bg(rgb(surface_hover())))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.copy_text_with_feedback(clicked_id.clone(), text.to_string(), cx);
                        cx.stop_propagation();
                    }))
            })
            .child("Copy")
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
        div()
            .relative()
            .group(scrollbar_id.clone())
            .ml_5()
            .pb_2()
            .max_h(px(260.0))
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
        let copy_text = message.copy_text.clone();
        let hover_key = self.selected_harness.map(|harness_id| (harness_id, index));
        let copy_button_visible = self.hovered_copy_message == hover_key;
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
                    .child(self.render_copy_button(
                        format!("copy-user-{index}"),
                        copy_text,
                        copy_button_visible,
                        cx,
                    ))
                    .into_any_element()
            }
            MessageRole::Assistant => div()
                .id(("assistant-message", index))
                .relative()
                .w_full()
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
                .child(div().w_full().min_w(px(0.0)).child(
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
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .child(self.render_copy_button(
                            format!("copy-assistant-{index}"),
                            copy_text,
                            copy_button_visible,
                            cx,
                        )),
                )
                .into_any_element(),
            MessageRole::Thinking => {
                let text = message.display_text.clone();
                div()
                    .id(("thinking-message", index))
                    .w_full()
                    .min_h(px(18.0))
                    .flex()
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
                    .w_full()
                    .flex()
                    .flex_col()
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
                            .child(div().min_w(px(0.0)).flex_1().child(label)),
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
                    .child(self.render_copy_button(
                        format!("copy-notice-{index}"),
                        copy_text,
                        copy_button_visible,
                        cx,
                    ))
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
                        .text_sm()
                        .font_weight(gpui::FontWeight::BOLD)
                        .with_animation(
                            "working-indicator",
                            Animation::new(Duration::from_secs(3)).repeat(),
                            |indicator, delta| {
                                let active = working_dot(delta);
                                indicator.children((0..3).map(move |index| {
                                    div()
                                        .text_color(rgb(if index == active {
                                            orange()
                                        } else {
                                            blue()
                                        }))
                                        .child(".")
                                }))
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
    use super::{format_retry_status, tool_color, working_dot};
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
    fn working_dot_moves_forward_then_back() {
        assert_eq!(working_dot(0.0), 0);
        assert_eq!(working_dot(0.25), 1);
        assert_eq!(working_dot(0.5), 2);
        assert_eq!(working_dot(0.75), 1);
        assert_eq!(working_dot(1.0), 0);
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
