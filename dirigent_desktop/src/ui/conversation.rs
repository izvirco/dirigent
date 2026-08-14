//! Renders the virtualized conversation and its scroll controls.

mod actions;
mod cache;
mod message;

pub(crate) use cache::{ConversationRenderCache, ConversationScrollAnchor};
use cache::{ConversationRenderItem, WorkGroupSummary};

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, FollowMode, HighlightStyle, IntoElement,
    ListOffset, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, ScrollHandle,
    SharedString, StyledImage, StyledText, Transformation, Window, canvas, deferred, div, fill,
    img, list, point, prelude::*, px, radians, relative, rgba, size, svg,
};

use super::composer::{composer_border_rings, dropdown_arrow};

use crate::{
    app::{ComposerDropdown, Dirigent},
    model::{HarnessStatus, Message, MessageRole, RetryStatus},
    theme::{
        blue, faint, green, muted, orange, purple, red, rgb, surface_hover, theme_text,
        thinking_text, yellow,
    },
};

const SLOW_CONVERSATION_SYNC: Duration = Duration::from_millis(16);
const STREAMING_RULER_LAYOUT_INTERVAL: Duration = Duration::from_millis(250);

fn performance_us(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

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

fn ruler_scroll_fraction(pointer: f32, grab_offset: f32, viewport_fraction: f32) -> f32 {
    let travel = 1.0 - viewport_fraction;
    if travel > 0.0 {
        ((pointer - grab_offset) / travel).clamp(0.0, 1.0)
    } else {
        0.0
    }
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
        let total_started = Instant::now();
        let cache_started = Instant::now();
        self.conversation_render_cache = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
            .map(ConversationRenderCache::build)
            .unwrap_or_default();
        let cache_elapsed = cache_started.elapsed();
        let list_started = Instant::now();
        self.conversation_list.reset_with_uniform_height(
            self.conversation_render_cache.len(),
            px(self.conversation_render_cache.item_height_hint()),
        );
        self.conversation_list.set_follow_mode(FollowMode::Tail);
        let list_elapsed = list_started.elapsed();
        let total_elapsed = total_started.elapsed();
        if total_elapsed >= SLOW_CONVERSATION_SYNC {
            let (harness_id, message_count) = self
                .selected_harness
                .and_then(|id| {
                    self.harnesses
                        .iter()
                        .find(|harness| harness.id == id)
                        .map(|harness| (Some(harness.id), harness.messages.len()))
                })
                .unwrap_or((None, 0));
            tracing::warn!(
                ?harness_id,
                message_count,
                render_item_count = self.conversation_render_cache.len(),
                cache_build_us = performance_us(cache_elapsed),
                list_reset_us = performance_us(list_elapsed),
                total_us = performance_us(total_elapsed),
                "slow conversation render cache reset"
            );
        }
    }

    pub(crate) fn sync_conversation_render_cache(&mut self, rebuild_from_message: usize) {
        let total_started = Instant::now();
        let old_cache = std::mem::take(&mut self.conversation_render_cache);
        let old_item_count = old_cache.len();
        let cache_started = Instant::now();
        let (new_cache, old_range, new_count, remeasure_ranges) = if let Some(harness) = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
        {
            ConversationRenderCache::update(harness, old_cache, rebuild_from_message)
        } else {
            (
                ConversationRenderCache::default(),
                0..old_item_count,
                0,
                Vec::new(),
            )
        };
        let cache_elapsed = cache_started.elapsed();
        let new_item_count = new_cache.len();
        let splice_range_start = old_range.start;
        let splice_removed = old_range.len();
        let list_started = Instant::now();
        if !old_range.is_empty() || new_count > 0 {
            let inserted_start = old_range.start;
            self.conversation_list.splice(old_range, new_count);
            if new_count > 0 {
                // `measure_all` only runs again after the measuring behavior is reset.
                self.conversation_list
                    .remeasure_items(inserted_start..inserted_start + new_count);
            }
        }
        let remeasure_range_count = remeasure_ranges.len();
        let remeasured_items = remeasure_ranges.iter().map(Range::len).sum::<usize>();
        for range in remeasure_ranges {
            self.conversation_list.remeasure_items(range);
        }
        let list_elapsed = list_started.elapsed();
        self.conversation_render_cache = new_cache;
        let total_elapsed = total_started.elapsed();
        if total_elapsed >= SLOW_CONVERSATION_SYNC {
            let (harness_id, message_count, queued_message_count) = self
                .selected_harness
                .and_then(|id| {
                    self.harnesses
                        .iter()
                        .find(|harness| harness.id == id)
                        .map(|harness| {
                            (
                                harness.id,
                                harness.messages.len(),
                                harness.queued_messages.len(),
                            )
                        })
                })
                .map(|(id, messages, queued)| (Some(id), messages, queued))
                .unwrap_or((None, 0, 0));
            tracing::warn!(
                ?harness_id,
                message_count,
                queued_message_count,
                rebuild_from_message,
                old_item_count,
                new_item_count,
                splice_range_start,
                splice_removed,
                splice_inserted = new_count,
                remeasure_range_count,
                remeasured_items,
                cache_update_us = performance_us(cache_elapsed),
                list_update_us = performance_us(list_elapsed),
                total_us = performance_us(total_elapsed),
                "slow conversation render cache synchronization"
            );
        }
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

    fn refresh_conversation_ruler_layout(&mut self) {
        if !self.conversation_render_cache.ruler_layout_pending {
            return;
        }
        let started = Instant::now();
        let streaming = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
            .is_some_and(|harness| harness.status == HarnessStatus::Working);
        if streaming
            && started.saturating_duration_since(self.conversation_ruler_last_layout_at)
                < STREAMING_RULER_LAYOUT_INTERVAL
        {
            return;
        }
        self.conversation_ruler_last_layout_at = started;
        let item_count = self.conversation_render_cache.len();
        if item_count == 0 {
            self.conversation_render_cache.ruler_item_heights.clear();
            self.conversation_render_cache.ruler_layout_pending = false;
            return;
        }

        // `measure_all` keeps exact item sizes inside ListState. Temporarily anchor its
        // coordinate queries at the top so bounds_for_item can expose every measured size,
        // then restore the user's logical position before this frame is laid out.
        let original_scroll_top = self.conversation_list.logical_scroll_top();
        let was_following_tail = self.conversation_list.is_following_tail();
        self.conversation_list.scroll_to(ListOffset::default());
        let measured_heights = (0..item_count)
            .map(|index| {
                self.conversation_list
                    .bounds_for_item(index)
                    .map(|bounds| bounds.size.height.as_f32())
            })
            .collect::<Option<Vec<_>>>();
        if was_following_tail {
            self.conversation_list.set_follow_mode(FollowMode::Tail);
        } else {
            self.conversation_list.scroll_to(original_scroll_top);
        }

        let fully_measured = measured_heights.is_some();
        if let Some(measured_heights) = measured_heights {
            self.conversation_render_cache.ruler_item_heights = measured_heights;
            self.conversation_render_cache.ruler_layout_width =
                self.conversation_list.viewport_bounds().size.width.as_f32();
            self.conversation_render_cache.ruler_layout_pending = false;
        }
        let elapsed = started.elapsed();
        if elapsed >= SLOW_CONVERSATION_SYNC {
            tracing::warn!(
                item_count,
                fully_measured,
                elapsed_us = performance_us(elapsed),
                "slow conversation ruler layout refresh"
            );
        }
    }

    fn render_conversation_ruler(&self, cx: &mut Context<Self>) -> AnyElement {
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
        let item_heights = &self.conversation_render_cache.ruler_item_heights;
        let total_height = item_heights.iter().sum::<f32>().max(1.0);
        let mut item_top = Vec::with_capacity(item_heights.len());
        let mut height_before = 0.0;
        for height in item_heights {
            item_top.push(height_before / total_height);
            height_before += height;
        }
        let markers = self
            .conversation_render_cache
            .ruler_markers
            .iter()
            .filter_map(|marker| {
                let start = *item_top.get(marker.render_item_index)?;
                let height = *item_heights.get(marker.render_item_index)? / total_height;
                Some((*marker, start, (start + height).min(1.0)))
            })
            .collect::<Vec<_>>();
        let request_layout_refresh = self.conversation_render_cache.ruler_layout_pending;
        let ruler_layout_width = self.conversation_render_cache.ruler_layout_width;
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
                    {
                        let entity = entity.clone();
                        move |_, _, cx| {
                            let viewport_width = entity
                                .read(cx)
                                .conversation_list
                                .viewport_bounds()
                                .size
                                .width
                                .as_f32();
                            let width_changed = ruler_layout_width > 0.0
                                && (viewport_width - ruler_layout_width).abs() > 0.5;
                            if request_layout_refresh || width_changed {
                                cx.defer(move |cx| {
                                    entity.update(cx, |this, cx| {
                                        this.conversation_render_cache.invalidate_ruler_layout();
                                        cx.notify();
                                    });
                                });
                            }
                        }
                    },
                    move |track_bounds, _, window, _| {
                        window.paint_layer(track_bounds, |window| {
                            let orange_color = rgb(orange());
                            let user_color = rgb(blue()).opacity(0.68);
                            let assistant_color = rgb(crate::theme::detail_text()).opacity(0.68);
                            let compaction_color = rgb(purple());
                            let message_line_width = window.pixel_snap(px(4.0));
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

                            for (marker, start, end) in markers.iter().copied() {
                                if marker.is_compaction {
                                    let center = (start + end) / 2.0;
                                    window.paint_quad(fill(
                                        gpui::Bounds::new(
                                            point(
                                                track_bounds.left() + px(2.0),
                                                track_bounds.top()
                                                    + track_bounds.size.height * center
                                                    - px(1.5),
                                            ),
                                            size(px(14.0), px(3.0)),
                                        ),
                                        compaction_color,
                                    ));
                                    continue;
                                }

                                let (left, color) = match marker.role {
                                    MessageRole::Assistant => (4.0, assistant_color),
                                    MessageRole::User => (11.0, user_color),
                                    _ => continue,
                                };
                                let line_left = window.pixel_snap(track_bounds.left() + px(left));
                                let top = window.pixel_snap(
                                    track_bounds.top() + track_bounds.size.height * start,
                                );
                                let bottom = window.pixel_snap(
                                    track_bounds.top() + track_bounds.size.height * end,
                                );
                                window.paint_quad(
                                    fill(
                                        gpui::Bounds::new(
                                            point(line_left, top),
                                            size(
                                                message_line_width,
                                                (bottom - top).max(message_line_width),
                                            ),
                                        ),
                                        color,
                                    )
                                    .corner_radii(message_line_width / 2.0),
                                );
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
                                let pointer = ((event.position.y - track_bounds.top())
                                    / track_bounds.size.height)
                                    .clamp(0.0, 1.0);
                                let grab_offset = if pointer >= viewport_top
                                    && pointer <= viewport_top + viewport_fraction
                                {
                                    pointer - viewport_top
                                } else {
                                    viewport_fraction / 2.0
                                };
                                let fraction =
                                    ruler_scroll_fraction(pointer, grab_offset, viewport_fraction);
                                entity.update(cx, |this, cx| {
                                    this.conversation_scroll_dragging = true;
                                    this.conversation_scroll_drag_offset = grab_offset;
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
                                let pointer = ((event.position.y - track_bounds.top())
                                    / track_bounds.size.height)
                                    .clamp(0.0, 1.0);
                                entity.update(cx, |this, cx| {
                                    let fraction = ruler_scroll_fraction(
                                        pointer,
                                        this.conversation_scroll_drag_offset,
                                        viewport_fraction,
                                    );
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
        window: &mut Window,
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
                            .child(self.render_message(message, *message_index, window, cx)),
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
                            .child(self.render_message(message, render_index, window, cx)),
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

    pub(super) fn render_conversation(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_conversation_ruler_layout();
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
            .child(self.render_conversation_ruler(cx))
    }
}
