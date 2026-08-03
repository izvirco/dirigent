use super::*;

pub(super) fn tool_color(tool: &str) -> u32 {
    match tool {
        "read" => green(),
        "edit" | "write" => yellow(),
        "compact" => purple(),
        _ => blue(),
    }
}

impl Dirigent {
    pub(super) fn render_tool_detail(
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
    pub(super) fn render_message(
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
                .pr_3()
                .border_r_4()
                .border_color(rgb(crate::theme::detail_text()))
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
                let timer = if message.running {
                    message.tool_started_at.map(|started_at| {
                        div()
                            .ml_2()
                            .flex_none()
                            .text_color(rgb(blue()))
                            .with_animation(
                                format!("tool-timer-{index}"),
                                Animation::new(Duration::from_secs(1)).repeat(),
                                move |timer, _| {
                                    timer.child(format_working_duration(started_at.elapsed()))
                                },
                            )
                            .into_any_element()
                    })
                } else {
                    message.tool_duration.map(|duration| {
                        div()
                            .ml_2()
                            .flex_none()
                            .text_color(rgb(if message.tool_failed { red() } else { muted() }))
                            .child(format_working_duration(duration))
                            .into_any_element()
                    })
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
                            .when_some(timer, |element, timer| element.child(timer))
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
                    .min_h(px(22.0))
                    .flex()
                    .items_start()
                    .text_xs()
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
}
