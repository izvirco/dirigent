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
    pub(super) render_item_index: usize,
    pub(super) role: MessageRole,
    pub(super) is_compaction: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConversationScrollAnchorIdentity {
    Message {
        entry_id: Option<String>,
        entry_ordinal: usize,
        fallback_message_index: usize,
    },
    WorkGroup(String),
}

#[derive(Clone, Debug)]
pub(crate) struct ConversationScrollAnchor {
    pub(crate) identity: Option<ConversationScrollAnchorIdentity>,
    pub(crate) offset_in_item: f32,
    pub(crate) fallback_fraction: f32,
}

fn build_ruler_markers(
    harness: &Harness,
    items: &[ConversationRenderItem],
) -> Arc<[ConversationRulerMarker]> {
    let mut direct_render_items = vec![None; harness.messages.len()];
    let mut grouped_render_items = vec![None; harness.messages.len()];
    for (render_item_index, item) in items.iter().enumerate() {
        match item {
            ConversationRenderItem::Message {
                message_index,
                queued: false,
            } => direct_render_items[*message_index] = Some(render_item_index),
            ConversationRenderItem::WorkGroup(group) => grouped_render_items
                [group.first_message_index..=group.last_message_index]
                .fill(Some(render_item_index)),
            ConversationRenderItem::Message { queued: true, .. }
            | ConversationRenderItem::Working => {}
        }
    }

    harness
        .messages
        .iter()
        .enumerate()
        .filter_map(|(message_index, message)| {
            let is_compaction = message.role == MessageRole::Tool && message.is_compaction();
            let render_item_index =
                if matches!(message.role, MessageRole::User | MessageRole::Assistant) {
                    // Hidden assistant fragments do not have a location in a collapsed thread.
                    direct_render_items[message_index]
                } else if is_compaction {
                    direct_render_items[message_index].or(grouped_render_items[message_index])
                } else {
                    None
                }?;
            Some(ConversationRulerMarker {
                render_item_index,
                role: message.role,
                is_compaction,
            })
        })
        .collect::<Vec<_>>()
        .into()
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
        let last_activity = activity
            .iter()
            .rposition(|message| matches!(message.role, MessageRole::Thinking | MessageRole::Tool))
            .map(|offset| user_index + 1 + offset);
        let Some(last_activity) = last_activity else {
            continue;
        };
        let latest_group = user_ordinal + 1 == user_indices.len();
        let running = latest_group && harness.status == HarnessStatus::Working;
        let prompt = diff::prompt_excerpt(&user.text);
        let turn = running
            .then_some(harness.active_turn_preview.as_ref())
            .flatten()
            .filter(|turn| turn.prompt == prompt)
            .or(completed_turn);
        let group_id = work_group_id(user, user_index);

        // Compaction can run after the agent has already emitted its final response. In that
        // case the compaction is a separate piece of activity; extending the original work
        // group through it would collapse the response between the two activity ranges.
        let final_response = activity
            .iter()
            .rposition(|message| message.role == MessageRole::Assistant)
            .map(|offset| user_index + 1 + offset);
        let response_before_trailing_compaction = final_response.filter(|response_index| {
            let mut activity_after_response = harness.messages[*response_index + 1..segment_end]
                .iter()
                .filter(|message| {
                    matches!(message.role, MessageRole::Thinking | MessageRole::Tool)
                });
            activity_after_response.next().is_some_and(|message| {
                message.is_compaction() && activity_after_response.all(Message::is_compaction)
            })
        });
        if let Some(response_index) = response_before_trailing_compaction {
            let mut has_pre_response_group = false;
            if let Some(last_pre_response_activity) = harness.messages
                [user_index + 1..response_index]
                .iter()
                .rposition(|message| {
                    matches!(message.role, MessageRole::Thinking | MessageRole::Tool)
                })
                .map(|offset| user_index + 1 + offset)
            {
                groups.push(WorkGroupSummary::from_range(
                    harness,
                    group_id.clone(),
                    user_index + 1..last_pre_response_activity + 1,
                    false,
                    turn,
                    latest_group,
                ));
                has_pre_response_group = true;
            }

            let mut compaction_index = response_index + 1;
            let mut compaction_ordinal = 0;
            while compaction_index < segment_end {
                if !harness.messages[compaction_index].is_compaction() {
                    compaction_index += 1;
                    continue;
                }
                let range_start = compaction_index;
                while compaction_index < segment_end
                    && harness.messages[compaction_index].is_compaction()
                {
                    compaction_index += 1;
                }
                let compaction_running = running && compaction_index - 1 == last_activity;
                let id = if !has_pre_response_group && compaction_ordinal == 0 {
                    group_id.clone()
                } else {
                    format!("{group_id}:compaction:{range_start}")
                };
                groups.push(WorkGroupSummary::from_range(
                    harness,
                    id,
                    range_start..compaction_index,
                    compaction_running,
                    None,
                    false,
                ));
                compaction_ordinal += 1;
            }
            continue;
        }

        groups.push(WorkGroupSummary::from_range(
            harness,
            group_id,
            user_index + 1..last_activity + 1,
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
    pub(super) ruler_item_heights: Vec<f32>,
    pub(super) ruler_layout_pending: bool,
    pub(super) ruler_layout_width: f32,
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
        let ruler_markers = build_ruler_markers(harness, &items);
        let ruler_item_heights = items
            .iter()
            .map(ConversationRenderItem::estimated_height)
            .collect();
        Self {
            items,
            ruler_markers,
            ruler_item_heights,
            ruler_layout_pending: true,
            ruler_layout_width: 0.0,
            estimated_height,
        }
    }

    pub(crate) fn update(
        harness: &Harness,
        old: Self,
        rebuild_from_message: usize,
    ) -> (Self, Range<usize>, usize, Vec<Range<usize>>) {
        let mut new = Self::build(harness);
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

        let retained_pairs = (0..prefix_len)
            .map(|index| (index, index, index))
            .chain((0..suffix_len).map(|offset| {
                (
                    old_len - suffix_len + offset,
                    new_len - suffix_len + offset,
                    new_len - suffix_len + offset,
                )
            }))
            .collect::<Vec<_>>();
        for &(old_index, new_index, _) in &retained_pairs {
            new.ruler_item_heights[new_index] = old.ruler_item_heights[old_index];
        }

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

    pub(crate) fn invalidate_ruler_layout(&mut self) {
        self.ruler_layout_pending = true;
    }

    pub(crate) fn scroll_anchor_identity(
        &self,
        messages: &[Message],
        render_item_index: usize,
    ) -> Option<ConversationScrollAnchorIdentity> {
        match self.items.get(render_item_index)? {
            ConversationRenderItem::Message {
                message_index,
                queued: false,
            } => {
                let message = messages.get(*message_index)?;
                let entry_ordinal = messages[..*message_index]
                    .iter()
                    .filter(|candidate| candidate.entry_id == message.entry_id)
                    .count();
                Some(ConversationScrollAnchorIdentity::Message {
                    entry_id: message.entry_id.clone(),
                    entry_ordinal,
                    fallback_message_index: *message_index,
                })
            }
            ConversationRenderItem::WorkGroup(group) => Some(
                ConversationScrollAnchorIdentity::WorkGroup(group.id.clone()),
            ),
            ConversationRenderItem::Message { queued: true, .. }
            | ConversationRenderItem::Working => None,
        }
    }

    pub(crate) fn render_item_index_for_scroll_anchor(
        &self,
        messages: &[Message],
        identity: &ConversationScrollAnchorIdentity,
    ) -> Option<usize> {
        match identity {
            ConversationScrollAnchorIdentity::Message {
                entry_id,
                entry_ordinal,
                fallback_message_index,
            } => {
                let message_index = messages
                    .iter()
                    .enumerate()
                    .filter(|(_, message)| &message.entry_id == entry_id)
                    .nth(*entry_ordinal)
                    .map(|(index, _)| index)
                    .or_else(|| {
                        (*fallback_message_index < messages.len())
                            .then_some(*fallback_message_index)
                    })?;
                self.message_render_item_index(message_index)
            }
            ConversationScrollAnchorIdentity::WorkGroup(id) => self.items.iter().position(
                |item| matches!(item, ConversationRenderItem::WorkGroup(group) if &group.id == id),
            ),
        }
    }

    pub(crate) fn message_render_item_index(&self, message_index: usize) -> Option<usize> {
        self.items.iter().position(|item| {
            matches!(
                item,
                ConversationRenderItem::Message {
                    message_index: item_message_index,
                    queued: false,
                } if *item_message_index == message_index
            )
        })
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
    fn compaction_after_a_response_does_not_hide_the_work_trail_or_response() {
        let mut harness = Harness::new(1, 2, "task".into(), 1);
        harness.status = HarnessStatus::Idle;
        harness.messages = vec![
            Message::new(MessageRole::User, "prompt"),
            Message::new(MessageRole::Thinking, "plan"),
            Message::tool("read src/main.rs", None, false, false),
            Message::new(MessageRole::Assistant, "done"),
            Message::compaction(None, Some("summary"), false),
        ];
        harness.messages[2].tool_name = Some("read".into());

        let cache = ConversationRenderCache::build(&harness);
        assert_eq!(cache.items.len(), 4);
        let ConversationRenderItem::WorkGroup(work_group) = &cache.items[1] else {
            panic!("missing pre-response work group");
        };
        assert_eq!(work_group.first_message_index, 1);
        assert_eq!(work_group.last_message_index, 2);
        assert!(matches!(
            cache.items[2],
            ConversationRenderItem::Message {
                message_index: 3,
                ..
            }
        ));
        let ConversationRenderItem::WorkGroup(compaction_group) = &cache.items[3] else {
            panic!("missing compaction group");
        };
        assert_eq!(compaction_group.first_message_index, 4);
        assert_eq!(compaction_group.last_message_index, 4);
        assert_ne!(work_group.id, compaction_group.id);
    }

    #[test]
    fn compaction_after_a_response_without_prior_activity_is_its_own_group() {
        let mut harness = Harness::new(1, 2, "task".into(), 1);
        harness.status = HarnessStatus::Idle;
        harness.messages = vec![
            Message::new(MessageRole::User, "prompt"),
            Message::new(MessageRole::Assistant, "done"),
            Message::compaction(None, Some("summary"), false),
        ];

        let cache = ConversationRenderCache::build(&harness);
        assert_eq!(cache.items.len(), 3);
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
        assert_eq!(group.last_message_index, 2);
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
    fn scroll_anchors_survive_canonical_message_replacement() {
        let mut cached = settled_harness();
        cached.messages[0].entry_id = Some("user-1".into());
        cached.messages[3].entry_id = Some("response-1".into());
        let cached_render = ConversationRenderCache::build(&cached);
        let group_anchor = cached_render
            .scroll_anchor_identity(&cached.messages, 1)
            .expect("missing cached work-group anchor");
        let response_anchor = cached_render
            .scroll_anchor_identity(&cached.messages, 2)
            .expect("missing cached response anchor");

        let mut canonical = settled_harness();
        canonical
            .messages
            .insert(0, Message::notice("canonical prelude"));
        canonical.messages[1].entry_id = Some("user-1".into());
        canonical.messages[4].entry_id = Some("response-1".into());
        let canonical_render = ConversationRenderCache::build(&canonical);

        assert_eq!(
            canonical_render
                .render_item_index_for_scroll_anchor(&canonical.messages, &group_anchor),
            Some(2)
        );
        assert_eq!(
            canonical_render
                .render_item_index_for_scroll_anchor(&canonical.messages, &response_anchor),
            Some(3)
        );
    }

    #[test]
    fn ruler_markers_follow_rendered_items() {
        let mut harness = settled_harness();
        harness
            .messages
            .push(Message::compaction(None, None, false));
        let cache = ConversationRenderCache::build(&harness);

        assert_eq!(
            cache.ruler_markers.as_ref(),
            [
                ConversationRulerMarker {
                    render_item_index: 0,
                    role: MessageRole::User,
                    is_compaction: false,
                },
                ConversationRulerMarker {
                    render_item_index: 2,
                    role: MessageRole::Assistant,
                    is_compaction: false,
                },
                ConversationRulerMarker {
                    render_item_index: 3,
                    role: MessageRole::Tool,
                    is_compaction: true,
                },
            ]
        );
    }

    #[test]
    fn ruler_markers_move_when_a_work_group_expands() {
        let mut harness = settled_harness();
        let collapsed = ConversationRenderCache::build(&harness);
        assert_eq!(collapsed.ruler_markers[1].render_item_index, 2);

        harness
            .work_group_expansion
            .insert("pending:0".into(), true);
        let expanded = ConversationRenderCache::build(&harness);
        assert_eq!(expanded.ruler_markers[1].render_item_index, 4);
    }

    #[test]
    fn ruler_omits_assistant_fragments_hidden_in_work_groups() {
        let mut harness = Harness::new(1, 2, "task".into(), 1);
        harness.messages = vec![
            Message::new(MessageRole::User, "prompt"),
            Message::new(MessageRole::Assistant, "interim"),
            Message::tool("read file", None, false, false),
            Message::new(MessageRole::Assistant, "final"),
        ];
        let cache = ConversationRenderCache::build(&harness);

        assert_eq!(cache.ruler_markers.len(), 2);
        assert_eq!(cache.ruler_markers[0].render_item_index, 0);
        assert_eq!(cache.ruler_markers[1].render_item_index, 2);
    }
}
