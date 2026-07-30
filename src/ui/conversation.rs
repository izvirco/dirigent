mod actions;
mod message;

#[cfg(test)]
use message::tool_color;

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
    let character_count = text.chars().count();
    ((delta * character_count as f32) as usize).min(character_count.saturating_sub(1))
}

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
                            Animation::new(Duration::from_millis(6_660 * 2)).repeat(),
                            move |indicator, delta| {
                                let elapsed = run_started_at
                                    .map(|started_at| started_at.elapsed())
                                    .unwrap_or_default();
                                let label =
                                    format!("Working for {}", format_working_duration(elapsed));
                                let color_label = label.clone();
                                indicator.children(label.chars().enumerate().map(
                                    move |(index, character)| {
                                        div()
                                            .text_color(rgb(
                                                if working_character_is_orange(
                                                    delta,
                                                    index,
                                                    &color_label,
                                                ) {
                                                    orange()
                                                } else {
                                                    blue()
                                                },
                                            ))
                                            .child(character.to_string())
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
        format_retry_status, format_working_duration, tool_color, working_character,
        working_character_is_orange,
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
