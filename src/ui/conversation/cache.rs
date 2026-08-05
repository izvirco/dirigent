use std::{
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    diff::{self, TurnDiff},
    model::{Harness, HarnessStatus, Message, MessageRole},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ConversationRulerMarker {
    pub(super) message_index: usize,
    pub(super) role: MessageRole,
    pub(super) is_compaction: bool,
}

fn conversation_ruler_marker(
    message: &Message,
    message_index: usize,
) -> Option<ConversationRulerMarker> {
    let is_compaction = message.role == MessageRole::Tool && message.is_compaction();
    (matches!(message.role, MessageRole::User | MessageRole::Assistant) || is_compaction).then_some(
        ConversationRulerMarker {
            message_index,
            role: message.role,
            is_compaction,
        },
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkGroupSummary {
    pub(crate) id: String,
    pub(crate) first_message_index: usize,
    pub(crate) last_message_index: usize,
    pub(crate) expanded: bool,
    pub(crate) running: bool,
    pub(crate) model: Option<String>,
    pub(crate) thinking_level: Option<String>,
    pub(crate) started_at: Option<Instant>,
    pub(crate) duration: Option<Duration>,
    pub(crate) additions: usize,
    pub(crate) deletions: usize,
    pub(crate) tool_count: usize,
    pub(crate) write_count: usize,
    pub(crate) edit_count: usize,
    pub(crate) compaction_count: usize,
    pub(crate) misc_count: usize,
}

impl WorkGroupSummary {
    fn from_range(
        harness: &Harness,
        id: String,
        range: Range<usize>,
        running: bool,
        turn: Option<&TurnDiff>,
        latest_group: bool,
    ) -> Self {
        let messages = &harness.messages[range.clone()];
        let model = messages
            .iter()
            .find_map(|message| message.model.clone())
            .or_else(|| harness.model.clone());
        let thinking_level = messages
            .iter()
            .find_map(|message| message.thinking_level.clone())
            .or_else(|| harness.thinking_level.clone());
        let mut write_count = 0;
        let mut edit_count = 0;
        let mut compaction_count = 0;
        let mut misc_count = 0;
        for message in messages
            .iter()
            .filter(|message| message.role == MessageRole::Tool)
        {
            match message.tool_name.as_deref() {
                Some("write") => write_count += 1,
                Some("edit") => edit_count += 1,
                Some("compact") => compaction_count += 1,
                _ if message.is_compaction() => compaction_count += 1,
                _ => misc_count += 1,
            }
        }
        let (additions, deletions) = turn
            .map(|turn| (turn.additions, turn.deletions))
            .unwrap_or_default();
        let duration = if !running && latest_group {
            harness.last_run_duration.or_else(|| {
                turn.map(|turn| {
                    Duration::from_secs(turn.finished_at.saturating_sub(turn.started_at))
                })
            })
        } else if !running {
            turn.map(|turn| Duration::from_secs(turn.finished_at.saturating_sub(turn.started_at)))
        } else {
            None
        };
        let expanded = harness
            .work_group_expansion
            .get(&id)
            .copied()
            .unwrap_or(running);
        Self {
            id,
            first_message_index: range.start,
            last_message_index: range.end - 1,
            expanded,
            running,
            model,
            thinking_level,
            started_at: running.then_some(harness.run_started_at).flatten(),
            duration,
            additions,
            deletions,
            tool_count: write_count + edit_count + compaction_count + misc_count,
            write_count,
            edit_count,
            compaction_count,
            misc_count,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ConversationRenderItem {
    Message { message_index: usize, queued: bool },
    WorkGroup(WorkGroupSummary),
    Working,
}

impl ConversationRenderItem {
    fn same_identity(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Message {
                    message_index: left_index,
                    queued: left_queued,
                },
                Self::Message {
                    message_index: right_index,
                    queued: right_queued,
                },
            ) => left_index == right_index && left_queued == right_queued,
            (Self::WorkGroup(left), Self::WorkGroup(right)) => left.id == right.id,
            (Self::Working, Self::Working) => true,
            _ => false,
        }
    }

    fn needs_remeasurement(&self, other: &Self, rebuild_from_message: usize) -> bool {
        if self != other {
            return true;
        }
        match other {
            Self::Message {
                message_index,
                queued: false,
            } => *message_index >= rebuild_from_message,
            // The working indicator can change between its normal and retry layouts without
            // changing the render-cache item itself.
            Self::Working => true,
            Self::Message { queued: true, .. } | Self::WorkGroup(_) => false,
        }
    }

    fn estimated_height(&self) -> f32 {
        match self {
            Self::Message { .. } => 48.0,
            Self::WorkGroup(_) | Self::Working => 32.0,
        }
    }
}

fn work_group_id(user: &Message, user_index: usize) -> String {
    user.entry_id.as_ref().map_or_else(
        || format!("pending:{user_index}"),
        |id| format!("entry:{id}"),
    )
}

fn matching_turn<'a>(
    harness: &'a Harness,
    user: &Message,
    turn_cursor: &mut usize,
) -> Option<&'a TurnDiff> {
    let prompt = diff::prompt_excerpt(&user.text);
    let relative = harness.turn_diffs[*turn_cursor..]
        .iter()
        .position(|turn| turn.prompt == prompt)?;
    let index = *turn_cursor + relative;
    *turn_cursor = index + 1;
    harness.turn_diffs.get(index)
}

fn build_work_groups(harness: &Harness) -> Vec<WorkGroupSummary> {
    let mut groups = Vec::new();
    let mut turn_cursor = 0;
    let user_indices = harness
        .messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == MessageRole::User).then_some(index))
        .collect::<Vec<_>>();
    for (user_ordinal, user_index) in user_indices.iter().copied().enumerate() {
        let segment_end = user_indices
            .get(user_ordinal + 1)
            .copied()
            .unwrap_or(harness.messages.len());
        let user = &harness.messages[user_index];
        let completed_turn = matching_turn(harness, user, &mut turn_cursor);
        let activity = &harness.messages[user_index + 1..segment_end];
        let first_activity = activity
            .iter()
            .position(|message| matches!(message.role, MessageRole::Thinking | MessageRole::Tool))
            .map(|offset| user_index + 1 + offset);
        let last_activity = activity
            .iter()
            .rposition(|message| matches!(message.role, MessageRole::Thinking | MessageRole::Tool))
            .map(|offset| user_index + 1 + offset);
        let (Some(first_activity), Some(last_activity)) = (first_activity, last_activity) else {
            continue;
        };
        let compaction_only = harness.messages[first_activity..=last_activity]
            .iter()
            .filter(|message| matches!(message.role, MessageRole::Thinking | MessageRole::Tool))
            .all(Message::is_compaction);
        let assistant_precedes_compaction = harness.messages[user_index + 1..first_activity]
            .iter()
            .any(|message| message.role == MessageRole::Assistant);
        let range_start = if compaction_only && assistant_precedes_compaction {
            first_activity
        } else {
            user_index + 1
        };
        let latest_group = user_ordinal + 1 == user_indices.len();
        let running = latest_group && harness.status == HarnessStatus::Working;
        let prompt = diff::prompt_excerpt(&user.text);
        let turn = running
            .then_some(harness.active_turn_preview.as_ref())
            .flatten()
            .filter(|turn| turn.prompt == prompt)
            .or(completed_turn);
        groups.push(WorkGroupSummary::from_range(
            harness,
            work_group_id(user, user_index),
            range_start..last_activity + 1,
            running,
            turn,
            latest_group,
        ));
    }
    groups
}

#[derive(Default)]
pub(crate) struct ConversationRenderCache {
    pub(super) items: Vec<ConversationRenderItem>,
    pub(super) ruler_markers: Arc<[ConversationRulerMarker]>,
    estimated_height: f32,
}

impl ConversationRenderCache {
    pub(crate) fn build(harness: &Harness) -> Self {
        let groups = build_work_groups(harness);
        let mut items = Vec::new();
        let mut group_index = 0;
        let mut message_index = 0;
        while message_index < harness.messages.len() {
            if let Some(group) = groups.get(group_index)
                && group.first_message_index == message_index
            {
                items.push(ConversationRenderItem::WorkGroup(group.clone()));
                if group.expanded {
                    items.extend((group.first_message_index..=group.last_message_index).map(
                        |message_index| ConversationRenderItem::Message {
                            message_index,
                            queued: false,
                        },
                    ));
                }
                message_index = group.last_message_index + 1;
                group_index += 1;
            } else {
                items.push(ConversationRenderItem::Message {
                    message_index,
                    queued: false,
                });
                message_index += 1;
            }
        }
        if harness.status == HarnessStatus::Working {
            items.push(ConversationRenderItem::Working);
        }
        for (queued_index, _) in harness.queued_messages.iter().enumerate() {
            items.push(ConversationRenderItem::Message {
                message_index: queued_index,
                queued: true,
            });
        }
        let estimated_height = items
            .iter()
            .map(ConversationRenderItem::estimated_height)
            .sum();
        let ruler_markers = harness
            .messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| conversation_ruler_marker(message, index))
            .collect::<Vec<_>>()
            .into();
        Self {
            items,
            ruler_markers,
            estimated_height,
        }
    }

    pub(crate) fn update(
        harness: &Harness,
        old: Self,
        rebuild_from_message: usize,
    ) -> (Self, Range<usize>, usize, Vec<Range<usize>>) {
        let new = Self::build(harness);
        let old_len = old.items.len();
        let new_len = new.items.len();

        // Preserve list items whose identity did not change. In particular, mutable work-group
        // summaries and streaming messages must not cause the entire active tail to lose its
        // measured heights.
        let prefix_len = old
            .items
            .iter()
            .zip(new.items.iter())
            .take_while(|(old_item, new_item)| old_item.same_identity(new_item))
            .count();
        let suffix_len = old.items[prefix_len..]
            .iter()
            .rev()
            .zip(new.items[prefix_len..].iter().rev())
            .take_while(|(old_item, new_item)| old_item.same_identity(new_item))
            .count();
        let old_range = prefix_len..old_len - suffix_len;
        let new_count = new_len - prefix_len - suffix_len;

        let retained_pairs =
            (0..prefix_len)
                .map(|index| (index, index, index))
                .chain((0..suffix_len).map(|offset| {
                    (
                        old_len - suffix_len + offset,
                        new_len - suffix_len + offset,
                        new_len - suffix_len + offset,
                    )
                }));
        let mut remeasure_ranges: Vec<Range<usize>> = Vec::new();
        for (old_index, new_index, rendered_index) in retained_pairs {
            if !old.items[old_index]
                .needs_remeasurement(&new.items[new_index], rebuild_from_message)
            {
                continue;
            }
            if let Some(range) = remeasure_ranges.last_mut()
                && range.end == rendered_index
            {
                range.end += 1;
            } else {
                remeasure_ranges.push(rendered_index..rendered_index + 1);
            }
        }

        (new, old_range, new_count, remeasure_ranges)
    }

    pub(crate) fn working_item_index(&self) -> Option<usize> {
        self.items
            .iter()
            .position(|item| matches!(item, ConversationRenderItem::Working))
    }

    pub(crate) fn work_group_ids(harness: &Harness) -> Vec<String> {
        build_work_groups(harness)
            .into_iter()
            .map(|group| group.id)
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn item_height_hint(&self) -> f32 {
        if self.items.is_empty() {
            48.0
        } else {
            (self.estimated_height / self.items.len() as f32).clamp(32.0, 320.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settled_harness() -> Harness {
        let mut harness = Harness::new(1, 2, "task".into(), 1);
        harness.status = HarnessStatus::Idle;
        harness.messages = vec![
            Message::new(MessageRole::User, "prompt"),
            Message::new(MessageRole::Thinking, "plan"),
            Message::tool("read src/main.rs", None, false, false),
            Message::new(MessageRole::Assistant, "done"),
        ];
        harness.messages[2].tool_name = Some("read".into());
        harness
    }

    #[test]
    fn settled_activity_is_collapsed_between_user_and_response() {
        let harness = settled_harness();
        let cache = ConversationRenderCache::build(&harness);
        assert_eq!(cache.items.len(), 3);
        assert!(matches!(
            cache.items[0],
            ConversationRenderItem::Message {
                message_index: 0,
                ..
            }
        ));
        assert!(matches!(
            cache.items[1],
            ConversationRenderItem::WorkGroup(_)
        ));
        assert!(matches!(
            cache.items[2],
            ConversationRenderItem::Message {
                message_index: 3,
                ..
            }
        ));
    }

    #[test]
    fn interim_assistant_text_is_part_of_the_work_group() {
        let mut harness = Harness::new(1, 2, "task".into(), 1);
        harness.status = HarnessStatus::Idle;
        harness.messages = vec![
            Message::new(MessageRole::User, "prompt"),
            Message::new(MessageRole::Assistant, "I'll inspect it."),
            Message::tool("read src/main.rs", None, false, false),
            Message::new(MessageRole::Assistant, "done"),
        ];
        harness.messages[2].tool_name = Some("read".into());

        let cache = ConversationRenderCache::build(&harness);
        let ConversationRenderItem::WorkGroup(group) = &cache.items[1] else {
            panic!("missing work group");
        };
        assert_eq!(group.first_message_index, 1);
        assert_eq!(group.last_message_index, 2);
        assert!(matches!(
            cache.items[2],
            ConversationRenderItem::Message {
                message_index: 3,
                ..
            }
        ));
    }

    #[test]
    fn compaction_after_a_response_does_not_hide_the_response() {
        let mut harness = Harness::new(1, 2, "task".into(), 1);
        harness.status = HarnessStatus::Idle;
        harness.messages = vec![
            Message::new(MessageRole::User, "prompt"),
            Message::new(MessageRole::Assistant, "done"),
            Message::compaction(None, Some("summary"), false),
        ];

        let cache = ConversationRenderCache::build(&harness);
        assert!(matches!(
            cache.items[1],
            ConversationRenderItem::Message {
                message_index: 1,
                ..
            }
        ));
        let ConversationRenderItem::WorkGroup(group) = &cache.items[2] else {
            panic!("missing compaction group");
        };
        assert_eq!(group.first_message_index, 2);
    }

    #[test]
    fn running_and_explicitly_expanded_activity_shows_original_messages() {
        let mut harness = settled_harness();
        harness.status = HarnessStatus::Working;
        let cache = ConversationRenderCache::build(&harness);
        assert_eq!(cache.items.len(), 6);
        assert!(matches!(
            cache.items.last(),
            Some(ConversationRenderItem::Working)
        ));
        let ConversationRenderItem::WorkGroup(group) = &cache.items[1] else {
            panic!("missing work group");
        };
        assert!(group.expanded);
        assert!(group.running);

        harness.status = HarnessStatus::Idle;
        harness.work_group_expansion.insert(group.id.clone(), true);
        let cache = ConversationRenderCache::build(&harness);
        assert_eq!(cache.items.len(), 5);
    }

    #[test]
    fn work_group_summary_uses_turn_and_tool_metadata() {
        let mut harness = settled_harness();
        harness.messages[1].set_turn_settings(Some("openai/gpt-5".into()), Some("high".into()));
        harness.messages[2].tool_name = Some("write".into());
        harness.turn_diffs.push(TurnDiff {
            id: 1,
            prompt: "prompt".into(),
            started_at: 10,
            finished_at: 15,
            status: crate::diff::TurnDiffStatus::Completed,
            files: Vec::new(),
            additions: 12,
            deletions: 3,
            error: None,
        });

        let cache = ConversationRenderCache::build(&harness);
        let ConversationRenderItem::WorkGroup(group) = &cache.items[1] else {
            panic!("missing work group");
        };
        assert_eq!(group.model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(group.thinking_level.as_deref(), Some("high"));
        assert_eq!(group.duration, Some(Duration::from_secs(5)));
        assert_eq!((group.additions, group.deletions), (12, 3));
        assert_eq!(group.tool_count, 1);
        assert_eq!(group.write_count, 1);
    }

    #[test]
    fn streaming_message_is_remeasured_without_being_spliced() {
        let mut harness = Harness::new(1, 2, "task".into(), 1);
        harness.status = HarnessStatus::Working;
        harness.messages = vec![
            Message::new(MessageRole::User, "prompt"),
            Message::new(MessageRole::Assistant, "partial response"),
        ];
        let old = ConversationRenderCache::build(&harness);

        harness.messages[1].append_text("\nnext line");
        let (new, old_range, new_count, remeasure_ranges) =
            ConversationRenderCache::update(&harness, old, 1);

        assert_eq!(old_range, 3..3);
        assert_eq!(new_count, 0);
        assert_eq!(remeasure_ranges, vec![1..3]);
        assert_eq!(new.working_item_index(), Some(2));
    }

    #[test]
    fn appended_tool_preserves_the_measured_active_tail() {
        let mut harness = settled_harness();
        harness.status = HarnessStatus::Working;
        let old_message_count = harness.messages.len();
        let old = ConversationRenderCache::build(&harness);

        let mut tool = Message::tool("run tests", None, true, false);
        tool.tool_name = Some("bash".into());
        harness.messages.push(tool);
        let (new, old_range, new_count, remeasure_ranges) =
            ConversationRenderCache::update(&harness, old, old_message_count);

        assert_eq!(old_range, 5..5);
        assert_eq!(new_count, 1);
        assert_eq!(remeasure_ranges, vec![1..2, 6..7]);
        assert_eq!(new.working_item_index(), Some(6));
    }

    #[test]
    fn ruler_markers_include_only_navigation_messages() {
        assert_eq!(
            conversation_ruler_marker(&Message::new(MessageRole::User, "prompt"), 4),
            Some(ConversationRulerMarker {
                message_index: 4,
                role: MessageRole::User,
                is_compaction: false,
            })
        );
        assert_eq!(
            conversation_ruler_marker(&Message::compaction(None, None, false), 5),
            Some(ConversationRulerMarker {
                message_index: 5,
                role: MessageRole::Tool,
                is_compaction: true,
            })
        );
        assert_eq!(
            conversation_ruler_marker(&Message::tool("read file", None, false, false), 6),
            None
        );
    }
}
