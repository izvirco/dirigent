//! Renders individual conversation messages and tool calls.

use super::*;

pub(super) fn tool_color(tool: &str) -> u32 {
    match tool {
        "read" => green(),
        "edit" | "write" => yellow(),
        "compact" => purple(),
        _ => blue(),
    }
}

pub(super) fn tool_label_colors(text: &str, tool: &str) -> Vec<(Range<usize>, u32)> {
    let mut colors = vec![(0..tool.len(), tool_color(tool))];
    if !matches!(tool, "edit" | "write") {
        return colors;
    }

    let Some(stats_start) = text.rfind(" +") else {
        return colors;
    };
    let stats_start = stats_start + 1;
    let Some((additions, deletions)) = text[stats_start..].split_once(' ') else {
        return colors;
    };
    if additions
        .strip_prefix('+')
        .is_none_or(|count| count.is_empty() || !count.bytes().all(|byte| byte.is_ascii_digit()))
        || deletions.strip_prefix('-').is_none_or(|count| {
            count.is_empty() || !count.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return colors;
    }

    let additions_end = stats_start + additions.len();
    colors.push((stats_start..additions_end, green()));
    colors.push((additions_end + 1..text.len(), red()));
    colors
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
            .child(self.render_thin_scrollbar(scrollbar_id, scroll, cx))
            .into_any_element()
    }
    fn render_work_summary_stats(
        &self,
        group: &WorkGroupSummary,
        leading: AnyElement,
        animation_id: String,
        thread_title: Option<String>,
    ) -> AnyElement {
        let model = group
            .model
            .as_deref()
            .map(|model| model.split_once('/').map_or(model, |(_, model)| model))
            .unwrap_or("unknown model")
            .to_string();
        let reasoning = group.thinking_level.clone();
        let duration = if group.running {
            group.started_at.map(|started_at| {
                div()
                    .flex_none()
                    .text_color(rgb(blue()))
                    .with_animation(
                        animation_id,
                        Animation::new(Duration::from_secs(1)).repeat(),
                        move |timer, _| timer.child(format_working_duration(started_at.elapsed())),
                    )
                    .into_any_element()
            })
        } else {
            group.duration.map(|duration| {
                div()
                    .flex_none()
                    .child(format_working_duration(duration))
                    .into_any_element()
            })
        };
        let mut categories = Vec::new();
        if group.write_count > 0 {
            categories.push(format!("{} write", group.write_count));
        }
        if group.edit_count > 0 {
            categories.push(format!("{} edit", group.edit_count));
        }
        if group.compaction_count > 0 {
            categories.push(format!("{} compaction", group.compaction_count));
        }
        if group.misc_count > 0 {
            categories.push(format!("{} misc", group.misc_count));
        }
        let tool_stats = (!categories.is_empty()).then(|| categories.join(" · "));
        let (additions, deletions) = group.diff_stats.counts();
        let approximate = group.diff_stats.is_optimistic();

        div()
            .min_w(px(0.0))
            .flex_1()
            .min_h(px(22.0))
            .flex()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_xs()
            .text_color(rgb(muted()))
            .child(leading)
            .child(div().flex_none().text_color(rgb(blue())).child(model))
            .when_some(reasoning, |element, reasoning| {
                element.child(div().flex_none().text_color(rgb(purple())).child(reasoning))
            })
            .when_some(duration, |element, duration| {
                element.child("·").child(duration)
            })
            .when_some(thread_title, |element, title| {
                element.child("·").child(
                    div()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(title),
                )
            })
            .when(additions > 0 || deletions > 0, |element| {
                let approximation = if approximate { "~" } else { "" };
                element
                    .child("·")
                    .child(
                        div()
                            .flex_none()
                            .text_color(rgb(green()))
                            .child(format!("{approximation}+{additions}")),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(rgb(red()))
                            .child(format!("{approximation}-{deletions}")),
                    )
            })
            .when_some(tool_stats, |element, tool_stats| {
                element.child("·").child(
                    div()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(tool_stats),
                )
            })
            .into_any_element()
    }

    fn delegated_work_rows(
        &self,
        group: &WorkGroupSummary,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(parent) = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
        else {
            return Vec::new();
        };
        let tool_call_ids = parent.messages[group.first_message_index..=group.last_message_index]
            .iter()
            .filter_map(|message| message.tool_call_id.as_deref())
            .collect::<Vec<_>>();
        let job_ids = parent
            .delegation
            .jobs
            .iter()
            .filter(|job| tool_call_ids.contains(&job.tool_call_id.as_str()))
            .map(|job| job.id.as_str())
            .collect::<Vec<_>>();
        if job_ids.is_empty() {
            return Vec::new();
        }

        self.harnesses
            .iter()
            .filter(|child| child.delegation.parent == Some(parent.id))
            .filter_map(|child| {
                let run = child
                    .delegation
                    .runs
                    .iter()
                    .rev()
                    .find(|run| job_ids.contains(&run.job_id.as_str()))?;
                let running = run.status == crate::delegation::WorkStatus::Running
                    && child
                        .delegation
                        .runs
                        .last()
                        .is_some_and(|latest| latest.id == run.id)
                    && matches!(
                        child.status,
                        HarnessStatus::Starting | HarnessStatus::Working
                    );
                let mut summary = if let Some(summary) = work_group_for_run(child, &run.id) {
                    summary
                } else {
                    WorkGroupSummary {
                        id: format!("delegated:{}:{}", child.id, run.id),
                        first_message_index: 0,
                        last_message_index: 0,
                        expanded: false,
                        running,
                        model: None,
                        thinking_level: None,
                        started_at: None,
                        duration: None,
                        diff_stats: WorkGroupDiffStats::Optimistic {
                            additions: 0,
                            deletions: 0,
                        },
                        tool_count: 0,
                        write_count: 0,
                        edit_count: 0,
                        compaction_count: 0,
                        misc_count: 0,
                    }
                };
                summary.running = running;
                if running {
                    // Live stream fragments lack canonical turn metadata; the active latest run
                    // can safely use the child's current settings and timer until rebuild catches up.
                    summary.model = summary.model.or_else(|| child.model.clone());
                    summary.thinking_level = summary
                        .thinking_level
                        .or_else(|| child.thinking_level.clone());
                    summary.started_at = summary.started_at.or(child.run_started_at);
                } else {
                    summary.started_at = None;
                }
                // A shared checkout's net diff cannot be attributed to one child reliably.
                if child.workspace_id == parent.workspace_id {
                    summary.diff_stats = WorkGroupDiffStats::Optimistic {
                        additions: 0,
                        deletions: 0,
                    };
                }
                let child_id = child.id;
                let leading = svg()
                    .path("icon/corner-down-right.svg")
                    .size(px(12.0))
                    .text_color(rgb(faint()))
                    .flex_none()
                    .into_any_element();
                Some(
                    div()
                        .id(format!("delegated-work-row-{}-{}", group.id, child_id))
                        .w_full()
                        .min_h(px(22.0))
                        .pl_4()
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .hover(|style| style.text_color(rgb(theme_text())).bg(rgb(surface_hover())))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_harness(child_id);
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .child(self.render_work_summary_stats(
                            &summary,
                            leading,
                            format!("delegated-work-timer-{child_id}"),
                            Some(child.title.clone()),
                        ))
                        .when(running, |row| {
                            row.child(
                                div()
                                    .id(("stop-delegated-agent", child_id as usize))
                                    .px_1()
                                    .text_xs()
                                    .text_color(rgb(red()))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.abort_harness(child_id);
                                        cx.stop_propagation();
                                        cx.notify();
                                    }))
                                    .child("Stop"),
                            )
                        })
                        .into_any_element(),
                )
            })
            .collect()
    }

    pub(super) fn render_work_group(
        &self,
        group: &WorkGroupSummary,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let group_id = group.id.clone();
        let expanded = group.expanded;
        let chevron = svg()
            .path("icon/chevron-down.svg")
            .size(px(12.0))
            .text_color(rgb(faint()))
            .flex_none();
        let chevron = if expanded {
            chevron
        } else {
            chevron.with_transformation(Transformation::rotate(radians(
                -std::f32::consts::FRAC_PI_2,
            )))
        };

        div()
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id(format!("work-group-{}", group.id))
                    .w_full()
                    .min_h(px(22.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgb(theme_text())).bg(rgb(surface_hover())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_work_group(group_id.clone(), !expanded);
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .child(self.render_work_summary_stats(
                        group,
                        chevron.into_any_element(),
                        format!("work-group-timer-{}", group.id),
                        None,
                    )),
            )
            .children(self.delegated_work_rows(group, cx))
            .into_any_element()
    }

    pub(super) fn render_message(
        &self,
        message: &Message,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.editing_message.as_ref().is_some_and(|edit| {
            edit.harness_id == self.selected_harness.unwrap_or_default()
                && edit.message_index == index
        }) {
            return self.render_message_edit_composer(window, cx);
        }
        let hover_key = self.selected_harness.map(|harness_id| (harness_id, index));
        let actions_visible = self.hovered_copy_message == hover_key
            || self.hovered_action_message == hover_key
            || self.hovered_tool_detail_message == hover_key;
        match message.role {
            MessageRole::User => {
                let images = message.images.clone();
                div()
                    .id(("user-message", index))
                    .relative()
                    .w_full()
                    .px_1()
                    .py_2()
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
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(-8.0))
                            .right(px(-8.0))
                            .rounded_xl()
                            .bg(rgb(blue()).opacity(0.10)),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(if let Some(markdown) = message.markdown.as_ref() {
                                self.render_markdown(markdown, index, cx)
                            } else {
                                self.render_selectable_text(
                                    format!("user-message-text-{index}"),
                                    message.display_text.clone(),
                                    &[],
                                    cx,
                                )
                            })
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
                let label_id = format!("tool-label-{index}");
                let label_colors = tool_label_colors(&message.display_text, tool);
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
                                    if let Some(render_index) = this
                                        .conversation_render_cache
                                        .message_render_item_index(index)
                                    {
                                        this.conversation_list
                                            .remeasure_items(render_index..render_index + 1);
                                        this.conversation_render_cache.invalidate_ruler_layout();
                                    }
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
