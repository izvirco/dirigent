//! Builds cached render items and work-group summaries for conversations.

use std::{
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    diff::{self, TurnDiff},
    model::{Harness, HarnessStatus, Message, MessageRole},
};

const LIVE_WORK_PREVIEW_COUNT: usize = 8;

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
            let render_item_index = if matches!(
                message.role,
                MessageRole::User | MessageRole::Agent | MessageRole::Assistant
            ) {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkGroupDiffStats {
    Optimistic { additions: usize, deletions: usize },
    Exact { additions: usize, deletions: usize },
}

impl WorkGroupDiffStats {
    pub(crate) fn counts(self) -> (usize, usize) {
        match self {
            Self::Optimistic {
                additions,
                deletions,
            }
            | Self::Exact {
                additions,
                deletions,
            } => (additions, deletions),
        }
    }

    pub(crate) fn is_optimistic(self) -> bool {
        matches!(self, Self::Optimistic { .. })
    }
}

// Per-tool changes are immediate but not a net workspace diff: later calls can overlap or
// reverse them, and writes do not know the previous file contents.
fn optimistic_diff_stats(messages: &[Message]) -> WorkGroupDiffStats {
    let (additions, deletions) = messages
        .iter()
        .filter(|message| {
            message.role == MessageRole::Tool && !message.running && !message.tool_failed
        })
        .filter_map(|message| message.tool_change_stats)
        .fold((0_usize, 0_usize), |(additions, deletions), stats| {
            (
                additions.saturating_add(stats.0),
                deletions.saturating_add(stats.1),
            )
        });
    WorkGroupDiffStats::Optimistic {
        additions,
        deletions,
    }
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
    pub(crate) diff_stats: WorkGroupDiffStats,
    pub(crate) tool_count: usize,
    pub(crate) write_count: usize,
    pub(crate) edit_count: usize,
    pub(crate) compaction_count: usize,
    pub(crate) misc_count: usize,
}

impl WorkGroupSummary {
    pub(super) fn has_rolling_preview(&self) -> bool {
        self.running
            && !self.expanded
            && self.last_message_index - self.first_message_index + 1 > LIVE_WORK_PREVIEW_COUNT
    }

    fn from_range(
        harness: &Harness,
        id: String,
        range: Range<usize>,
        running: bool,
        turn: Option<&TurnDiff>,
        completed_turn: Option<&TurnDiff>,
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
        let diff_stats = completed_turn
            .filter(|turn| turn.error.is_none())
            .map_or_else(
                || optimistic_diff_stats(messages),
                |turn| WorkGroupDiffStats::Exact {
                    additions: turn.additions,
                    deletions: turn.deletions,
                },
            );
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
            .unwrap_or(false);
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
            diff_stats,
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

// The assignment header identifies runs even before Pi timestamps arrive.
fn assignment_run_id(text: &str) -> Option<&str> {
    let header = text.lines().next()?.strip_prefix('[')?.strip_suffix(']')?;
    let metadata = header.strip_prefix("Dirigent assignment; manager #")?;
    let (manager, run) = metadata.split_once("; run ")?;
    (!manager.is_empty()
        && manager.bytes().all(|byte| byte.is_ascii_digit())
        && !run.is_empty()
        && !run.bytes().any(|byte| byte.is_ascii_whitespace()))
    .then_some(run)
}

pub(crate) fn work_group_for_run(harness: &Harness, run_id: &str) -> Option<WorkGroupSummary> {
    let timestamp = harness
        .delegation
        .runs
        .iter()
        .find(|run| run.id == run_id)
        .and_then(|run| run.prompt_timestamp_ms);
    let (user_index, user) = harness.messages.iter().enumerate().find(|(_, message)| {
        message.role == MessageRole::User
            && match timestamp {
                Some(timestamp) => message.timestamp_ms == Some(timestamp),
                None => assignment_run_id(&message.text) == Some(run_id),
            }
    })?;
    // An assignment can finish with only an assistant reply (no thinking or tools).
    // Summarize the entire assignment, including its final reply's model metadata,
    // instead of requiring a collapsible activity group.
    let end = harness.messages[user_index + 1..]
        .iter()
        .position(|message| message.role == MessageRole::User)
        .map_or(harness.messages.len(), |offset| user_index + 1 + offset);
    let latest = end == harness.messages.len();
    let running = latest && harness.status == HarnessStatus::Working;
    let prompt = diff::prompt_excerpt(&user.text);
    let mut turn_cursor = 0;
    let completed_turn = harness.messages[..=user_index]
        .iter()
        .filter(|message| matches!(message.role, MessageRole::User | MessageRole::Agent))
        .map(|user| matching_turn(harness, user, &mut turn_cursor))
        .last()
        .flatten();
    let turn = harness
        .active_turn_preview
        .as_ref()
        .filter(|turn| running && turn.prompt == prompt)
        .or(completed_turn);
    let mut summary = WorkGroupSummary::from_range(
        harness,
        work_group_id(user, user_index),
        user_index..end,
        running,
        turn,
        completed_turn,
        latest,
    );
    if !latest {
        // A later assignment may use a different model. Only the latest assignment
        // can fall back to the harness settings while canonical messages load.
        let messages = &harness.messages[user_index..end];
        summary.model = messages.iter().find_map(|message| message.model.clone());
        summary.thinking_level = messages
            .iter()
            .find_map(|message| message.thinking_level.clone());
    }
    Some(summary)
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
    harness.turn_diffs.get(index).map(Arc::as_ref)
}

fn build_work_groups(harness: &Harness) -> Vec<WorkGroupSummary> {
    let mut groups = Vec::new();
    let mut turn_cursor = 0;
    let user_indices = harness
        .messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            matches!(message.role, MessageRole::User | MessageRole::Agent).then_some(index)
        })
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
                    completed_turn,
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
            completed_turn,
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
                let visible_start = if group.expanded {
                    group.first_message_index
                } else if group.running {
                    // Tools, thinking snippets, and commentary share the same eight-entry budget.
                    (group.last_message_index + 1)
                        .saturating_sub(LIVE_WORK_PREVIEW_COUNT)
                        .max(group.first_message_index)
                } else {
                    group.last_message_index + 1
                };
                items.extend(
                    (visible_start..=group.last_message_index).map(|message_index| {
                        ConversationRenderItem::Message {
                            message_index,
                            queued: false,
                        }
                    }),
                );
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

    pub(crate) fn refresh_optimistic_diff_stats(
        &mut self,
        messages: &[Message],
        message_index: usize,
    ) {
        let Some(group) = self.items.iter_mut().find_map(|item| match item {
            ConversationRenderItem::WorkGroup(group)
                if group.first_message_index <= message_index
                    && message_index <= group.last_message_index
                    && group.diff_stats.is_optimistic() =>
            {
                Some(group)
            }
            _ => None,
        }) else {
            return;
        };
        group.diff_stats =
            optimistic_diff_stats(&messages[group.first_message_index..=group.last_message_index]);
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

    fn visible_tools(harness: &Harness) -> Vec<usize> {
        ConversationRenderCache::build(harness)
            .items
            .iter()
            .filter_map(|item| match item {
                ConversationRenderItem::Message {
                    message_index,
                    queued: false,
                } if harness.messages[*message_index].role == MessageRole::Tool => {
                    Some(*message_index)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn collapsed_live_groups_preview_eight_work_entries_and_respect_explicit_expansion() {
        let mut harness = Harness::new(1, 1, "agent".into(), 1);
        harness.status = HarnessStatus::Working;
        harness
            .messages
            .push(Message::new(MessageRole::User, "Do work"));
        for _ in 0..4 {
            harness
                .messages
                .push(Message::new(MessageRole::Assistant, "An update"));
            harness.messages.push(Message::new(
                MessageRole::Thinking,
                "Considering the next step",
            ));
            harness
                .messages
                .push(Message::tool("read", None, false, false));
        }
        let group = build_work_groups(&harness).remove(0);
        assert!(!group.expanded);
        assert!(group.has_rolling_preview());
        assert_eq!(visible_tools(&harness), vec![6, 9, 12]);
        let cache = ConversationRenderCache::build(&harness);
        for index in 1..=12 {
            assert_eq!(cache.message_render_item_index(index).is_some(), index >= 5);
        }

        harness.work_group_expansion.insert(group.id.clone(), true);
        assert!(!build_work_groups(&harness)[0].has_rolling_preview());
        for status in [HarnessStatus::Working, HarnessStatus::Idle] {
            harness.status = status;
            let cache = ConversationRenderCache::build(&harness);
            for index in 1..=12 {
                assert!(cache.message_render_item_index(index).is_some());
            }
        }
        harness.status = HarnessStatus::Working;
        harness.work_group_expansion.insert(group.id, false);
        assert_eq!(visible_tools(&harness), vec![6, 9, 12]);
        harness.status = HarnessStatus::Idle;
        let cache = ConversationRenderCache::build(&harness);
        for index in 1..=12 {
            assert!(cache.message_render_item_index(index).is_none());
        }
    }

    #[test]
    fn live_preview_rolls_forward_and_only_applies_to_the_running_group() {
        let mut harness = Harness::new(1, 1, "agent".into(), 1);
        harness.status = HarnessStatus::Working;
        harness.messages = vec![
            Message::new(MessageRole::User, "Previous turn"),
            Message::tool("read", None, false, false),
            Message::new(MessageRole::Assistant, "Done"),
            Message::new(MessageRole::User, "Next turn"),
        ];
        for _ in 0..8 {
            harness
                .messages
                .push(Message::tool("read", None, false, false));
        }
        assert_eq!(visible_tools(&harness), (4..12).collect::<Vec<_>>());
        let old = ConversationRenderCache::build(&harness);
        harness
            .messages
            .push(Message::new(MessageRole::Thinking, "Reviewing the results"));
        let (new, removed, inserted, _) = ConversationRenderCache::update(&harness, old, 12);
        assert_eq!(visible_tools(&harness), (5..12).collect::<Vec<_>>());
        assert_eq!(removed.len(), inserted);
        assert!(new.message_render_item_index(4).is_none());
        assert!(new.message_render_item_index(12).is_some());
        for status in [
            HarnessStatus::Idle,
            HarnessStatus::Failed,
            HarnessStatus::Stopped,
        ] {
            harness.status = status;
            assert!(visible_tools(&harness).is_empty());
            assert!(
                ConversationRenderCache::build(&harness)
                    .message_render_item_index(12)
                    .is_none()
            );
        }
    }

    #[test]
    fn associates_each_delegated_run_with_its_own_work_group_stats() {
        let mut harness = Harness::new(2, 1, "child".into(), 1);
        for (id, timestamp) in [("initial", 1000), ("feedback", 2000)] {
            harness.delegation.runs.push(crate::delegation::AgentRun {
                id: id.into(),
                job_id: "job".into(),
                status: crate::delegation::WorkStatus::Completed,
                result: String::new(),
                error: None,
                stop_reason: Some("stop".into()),
                handed_off: false,
                parent_message: None,
                prompt_timestamp_ms: Some(timestamp),
            });
        }
        // Repeated prompts must still map to distinct assignments.
        let mut initial_prompt = Message::new(MessageRole::User, "Continue");
        initial_prompt.timestamp_ms = Some(1000);
        harness.messages.push(initial_prompt);
        let mut write = Message::tool("write", Some("write-1".into()), false, false);
        write.tool_name = Some("write".into());
        write.tool_change_stats = Some((7, 0));
        harness.messages.push(write);
        let mut feedback_prompt = Message::new(MessageRole::User, "Continue");
        feedback_prompt.timestamp_ms = Some(2000);
        harness.messages.push(feedback_prompt);
        let mut edit = Message::tool("edit", Some("edit-1".into()), false, false);
        edit.tool_name = Some("edit".into());
        edit.tool_change_stats = Some((2, 3));
        harness.messages.push(edit);
        for (id, additions, deletions) in [(1, 7, 0), (2, 2, 3)] {
            harness.turn_diffs.push(Arc::new(TurnDiff {
                id,
                prompt: "Continue".into(),
                started_at: id,
                finished_at: id + 1,
                status: diff::TurnDiffStatus::Completed,
                files: Vec::new(),
                additions,
                deletions,
                error: None,
            }));
        }

        let initial = work_group_for_run(&harness, "initial").expect("initial work group");
        let feedback = work_group_for_run(&harness, "feedback").expect("feedback work group");
        assert_eq!(initial.diff_stats.counts(), (7, 0));
        assert_eq!(initial.write_count, 1);
        assert_eq!(initial.edit_count, 0);
        assert_eq!(feedback.diff_stats.counts(), (2, 3));
        assert_eq!(feedback.write_count, 0);
        assert_eq!(feedback.edit_count, 1);
    }

    #[test]
    fn child_pings_start_work_groups_and_stay_visible_when_activity_is_collapsed() {
        let mut harness = Harness::new(1, 1, "parent".into(), 1);
        harness.status = HarnessStatus::Idle;
        harness.messages = vec![
            Message::new(MessageRole::User, "Delegate the API change"),
            Message::tool("dirigent_agents", None, false, false),
            Message::new(MessageRole::Assistant, "The child is working"),
            Message::new(MessageRole::Agent, "Please review the API"),
            Message::tool("read", None, false, false),
            Message::new(MessageRole::Assistant, "The API looks good"),
        ];
        let groups = build_work_groups(&harness);
        assert_eq!(groups.len(), 2);
        assert_eq!(
            (groups[0].first_message_index, groups[0].last_message_index),
            (1, 1)
        );
        assert_eq!(
            (groups[1].first_message_index, groups[1].last_message_index),
            (4, 4)
        );
        let cache = ConversationRenderCache::build(&harness);
        for index in [2, 3, 5] {
            assert!(cache.items.iter().any(|item| matches!(
                item,
                ConversationRenderItem::Message { message_index, queued: false } if *message_index == index
            )));
        }
        assert!(
            cache
                .ruler_markers
                .iter()
                .any(|marker| marker.role == MessageRole::Agent)
        );
    }

    #[test]
    fn extracts_assignment_run_ids() {
        assert_eq!(
            assignment_run_id("[Dirigent assignment; manager #281; run run-new]\n\nDo work"),
            Some("run-new")
        );
        assert_eq!(assignment_run_id("ordinary user message"), None);
        assert_eq!(
            assignment_run_id("[Dirigent assignment; manager #281; run ]"),
            None
        );
        assert_eq!(assignment_run_id("[Other metadata; run unrelated]"), None);
    }
}
