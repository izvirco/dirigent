mod actions;
mod cache;
mod message;

pub(crate) use cache::ConversationRenderCache;
use cache::{ConversationRenderItem, WorkGroupSummary};

#[cfg(test)]
use message::{tool_color, tool_label_colors};

use std::{ops::Range, time::Duration};

use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, FollowMode, HighlightStyle, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, ScrollHandle,
    SharedString, StyledImage, StyledText, Transformation, Window, canvas, deferred, div, fill,
    img, list, point, prelude::*, px, radians, relative, size, svg,
};

use super::composer::dropdown_arrow;

use crate::{
    app::{ComposerDropdown, Dirigent},
    model::{HarnessStatus, Message, MessageRole, RetryStatus},
    theme::{
        blue, faint, green, muted, orange, purple, red, rgb, surface_hover, theme_text,
        thinking_text, yellow,
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
    let character_count = text.chars().count();
    ((delta * character_count as f32) as usize).min(character_count.saturating_sub(1))
}

#[cfg(test)]
fn working_character_is_orange(delta: f32, index: usize, text: &str) -> bool {
    if delta >= 1.0 {
        return false;
    }
    let animation_position = delta * 2.0;
    let filling_orange = animation_position < 1.0;
    let active = working_character(animation_position.fract(), text);
    if filling_orange {
        index <= active
    } else {
        index > active
    }
}

fn working_orange_range(delta: f32, text: &str) -> Option<Range<usize>> {
    if text.is_empty() || delta >= 1.0 {
        return None;
    }
    let animation_position = delta * 2.0;
    let active = working_character(animation_position.fract(), text);
    let active_end = text
        .char_indices()
        .nth(active + 1)
        .map_or(text.len(), |(index, _)| index);
    let range = if animation_position < 1.0 {
        0..active_end
    } else {
        active_end..text.len()
    };
    (!range.is_empty()).then_some(range)
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

impl Dirigent {
    pub(crate) fn reset_conversation_render_cache(&mut self) {
        self.conversation_render_cache = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
            .map(ConversationRenderCache::build)
            .unwrap_or_default();
        self.conversation_list.reset_with_uniform_height(
            self.conversation_render_cache.len(),
            px(self.conversation_render_cache.item_height_hint()),
        );
        self.conversation_list.set_follow_mode(FollowMode::Tail);
    }

    pub(crate) fn sync_conversation_render_cache(&mut self, rebuild_from_message: usize) {
        let old_cache = std::mem::take(&mut self.conversation_render_cache);
        let (new_cache, old_range, new_count) = if let Some(harness) = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
        {
            ConversationRenderCache::update(harness, old_cache, rebuild_from_message)
        } else {
            let old_len = old_cache.len();
            (ConversationRenderCache::default(), 0..old_len, 0)
        };
        self.conversation_list.splice(old_range, new_count);
        self.conversation_render_cache = new_cache;
    }

    pub(crate) fn toggle_work_group(&mut self, id: String, expanded: bool) {
        let Some(index) = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().position(|harness| harness.id == id))
        else {
            return;
        };
        let rebuild_from_message = self
            .conversation_render_cache
            .items
            .iter()
            .find_map(|item| match item {
                ConversationRenderItem::WorkGroup(group) if group.id == id => {
                    Some(group.first_message_index)
                }
                _ => None,
            })
            .unwrap_or_default();
        self.harnesses[index]
            .work_group_expansion
            .insert(id, expanded);
        self.persist();
        self.sync_conversation_render_cache(rebuild_from_message);
    }

    pub(crate) fn set_all_work_groups_expanded(&mut self, expanded: bool) {
        let Some(index) = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().position(|harness| harness.id == id))
        else {
            return;
        };
        let rebuild_from_message = self
            .conversation_render_cache
            .items
            .iter()
            .filter_map(|item| match item {
                ConversationRenderItem::WorkGroup(group) => Some(group.first_message_index),
                _ => None,
            })
            .min()
            .unwrap_or_default();
        let ids = ConversationRenderCache::work_group_ids(&self.harnesses[index]);
        self.harnesses[index]
            .work_group_expansion
            .retain(|id, _| ids.contains(id));
        for id in ids {
            self.harnesses[index]
                .work_group_expansion
                .insert(id, expanded);
        }
        self.persist();
        self.sync_conversation_render_cache(rebuild_from_message);
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
        let markers = self.conversation_render_cache.ruler_markers.clone();
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
                canvas(
                    |_, _, _| (),
                    move |track_bounds, _, window, _| {
                        window.paint_layer(track_bounds, |window| {
                            let orange_color = rgb(orange());
                            let user_color = rgb(blue());
                            let assistant_color = rgb(crate::theme::detail_text());
                            let compaction_color = rgb(purple());
                            let tick_color = orange_color.opacity(0.60);
                            let tick_span = (track_bounds.size.height - px(3.0)).max(px(0.0));
                            for index in 0..17 {
                                let y = track_bounds.top()
                                    + px(1.0)
                                    + tick_span * (index as f32 / 16.0);
                                window.paint_quad(fill(
                                    gpui::Bounds::new(
                                        point(track_bounds.left() + px(2.0), y),
                                        size(px(14.0), px(1.0)),
                                    ),
                                    tick_color,
                                ));
                            }

                            for marker in markers.iter() {
                                let fraction = (marker.message_index as f32 + 0.5) / message_count;
                                let (left, width, color) = if marker.is_compaction {
                                    (2.0, 14.0, compaction_color)
                                } else {
                                    let color = match marker.role {
                                        MessageRole::User => user_color,
                                        MessageRole::Assistant => assistant_color,
                                        _ => continue,
                                    };
                                    (6.0, 6.0, color)
                                };
                                let marker_quad = fill(
                                    gpui::Bounds::new(
                                        point(
                                            track_bounds.left() + px(left),
                                            track_bounds.top()
                                                + track_bounds.size.height * fraction,
                                        ),
                                        size(px(width), px(6.0)),
                                    ),
                                    color,
                                );
                                window.paint_quad(if marker.is_compaction {
                                    marker_quad
                                } else {
                                    marker_quad.corner_radii(px(3.0))
                                });
                            }

                            let viewport_height =
                                (track_bounds.size.height * viewport_fraction).max(px(5.0));
                            let viewport_y =
                                track_bounds.top() + track_bounds.size.height * viewport_top;
                            window.paint_quad(fill(
                                gpui::Bounds::new(
                                    point(track_bounds.left(), viewport_y),
                                    size(track_bounds.size.width, viewport_height),
                                ),
                                orange_color.opacity(0.09),
                            ));
                            let viewport_border = rgb(crate::theme::warning_border());
                            for y in [viewport_y, viewport_y + viewport_height - px(1.0)] {
                                window.paint_quad(fill(
                                    gpui::Bounds::new(
                                        point(track_bounds.left(), y),
                                        size(track_bounds.size.width, px(1.0)),
                                    ),
                                    viewport_border,
                                ));
                            }
                        });

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
        let Some(item) = self.conversation_render_cache.items.get(index) else {
            return div().into_any_element();
        };

        match item {
            ConversationRenderItem::Message {
                message_index,
                queued: false,
                ..
            } => {
                let Some(message) = harness.messages.get(*message_index) else {
                    return div().into_any_element();
                };
                let follows_work_group = index > 0
                    && matches!(
                        self.conversation_render_cache.items.get(index - 1),
                        Some(ConversationRenderItem::WorkGroup(_))
                    );
                let follows_activity = follows_work_group
                    || (*message_index > 0
                        && matches!(message.role, MessageRole::Thinking | MessageRole::Tool)
                        && matches!(
                            harness.messages[*message_index - 1].role,
                            MessageRole::Thinking | MessageRole::Tool
                        ));
                div()
                    .w_full()
                    .child(
                        div()
                            .w_full()
                            .max_w(px(820.0))
                            .mx_auto()
                            .px_7()
                            .when(*message_index > 0 && !follows_activity, |element| {
                                element.mt_2()
                            })
                            .child(self.render_message(message, *message_index, cx)),
                    )
                    .into_any_element()
            }
            ConversationRenderItem::WorkGroup(group) => div()
                .w_full()
                .child(
                    div()
                        .w_full()
                        .max_w(px(820.0))
                        .mx_auto()
                        .px_7()
                        .mt_2()
                        .child(self.render_work_group(group, cx)),
                )
                .into_any_element(),
            ConversationRenderItem::Message {
                message_index,
                queued: true,
                ..
            } => {
                let Some(message) = harness.queued_messages.get(*message_index) else {
                    return div().into_any_element();
                };
                let render_index = harness.messages.len()
                    + usize::from(harness.status == HarnessStatus::Working)
                    + message_index;
                div()
                    .w_full()
                    .child(
                        div()
                            .w_full()
                            .max_w(px(820.0))
                            .mx_auto()
                            .px_7()
                            .mt_2()
                            .child(self.render_message(message, render_index, cx)),
                    )
                    .into_any_element()
            }
            ConversationRenderItem::Working => {
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
                                        .min_h(px(22.0))
                                        .flex()
                                        .flex_col()
                                        .text_xs()
                                        .text_color(rgb(orange()))
                                        .child(format_retry_status(retry))
                                        .child(retry.error_message.clone()),
                                ),
                        )
                        .into_any_element();
                }

                let run_started_at = harness.run_started_at;
                div()
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
                            .text_color(rgb(blue()))
                            .with_animation(
                                "working-indicator",
                                Animation::new(Duration::from_millis(6_660 * 2)).repeat(),
                                move |indicator, delta| {
                                    let elapsed = run_started_at
                                        .map(|started_at| started_at.elapsed())
                                        .unwrap_or_default();
                                    let label =
                                        format!("Working for {}", format_working_duration(elapsed));
                                    let orange_range = working_orange_range(delta, &label);
                                    let mut text = StyledText::new(SharedString::from(label));
                                    if let Some(range) = orange_range {
                                        text = text.with_highlights(std::iter::once((
                                            range,
                                            HighlightStyle {
                                                color: Some(rgb(orange()).into()),
                                                ..Default::default()
                                            },
                                        )));
                                    }
                                    indicator.child(text)
                                },
                            ),
                    )
                    .into_any_element()
            }
        }
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
        format_retry_status, format_working_duration, tool_color, tool_label_colors,
        working_character, working_character_is_orange, working_orange_range,
    };
    use crate::{
        model::RetryStatus,
        theme::{green, purple, red, yellow},
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
    fn working_character_moves_across_spaces() {
        assert_eq!(working_character(0.0, "ab cd"), 0);
        assert_eq!(working_character(0.25, "ab cd"), 1);
        assert_eq!(working_character(0.5, "ab cd"), 2);
        assert_eq!(working_character(0.75, "ab cd"), 3);
        assert_eq!(working_character(1.0, "ab cd"), 4);
    }

    #[test]
    fn working_colors_fill_and_reverse() {
        let text = "abcd";

        assert!(working_character_is_orange(0.0, 0, text));
        assert!(!working_character_is_orange(0.0, 1, text));
        assert!(working_character_is_orange(0.25, 2, text));
        assert!(!working_character_is_orange(0.25, 3, text));
        assert!((0..4).all(|index| working_character_is_orange(0.49, index, text)));

        assert!(!working_character_is_orange(0.5, 0, text));
        assert!(working_character_is_orange(0.5, 1, text));
        assert!(!working_character_is_orange(0.75, 2, text));
        assert!(working_character_is_orange(0.75, 3, text));
        assert!((0..4).all(|index| !working_character_is_orange(1.0, index, text)));

        assert_eq!(working_orange_range(0.0, text), Some(0..1));
        assert_eq!(working_orange_range(0.25, text), Some(0..3));
        assert_eq!(working_orange_range(0.5, text), Some(1..4));
        assert_eq!(working_orange_range(0.75, text), Some(3..4));
        assert_eq!(working_orange_range(1.0, text), None);
    }

    #[test]
    fn compaction_tools_are_purple() {
        assert_eq!(tool_color("compact"), purple());
    }

    #[test]
    fn write_tools_are_yellow() {
        assert_eq!(tool_color("write"), yellow());
    }

    #[test]
    fn edit_and_write_stats_use_diff_colors() {
        let text = "edit src/main.rs +12 -3";
        assert_eq!(
            tool_label_colors(text, "edit"),
            vec![(0..4, yellow()), (17..20, green()), (21..23, red())]
        );

        let text = "write src/main.rs +2 -0";
        assert_eq!(
            tool_label_colors(text, "write"),
            vec![(0..5, yellow()), (18..20, green()), (21..23, red())]
        );
    }
}
