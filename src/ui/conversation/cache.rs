use std::{ops::Range, sync::Arc};

use crate::model::{Harness, HarnessStatus, Message, MessageRole};

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

pub(super) enum ConversationRenderItem {
    Message { message_index: usize, queued: bool },
    Working,
}

impl ConversationRenderItem {
    pub(super) fn message_index(&self) -> Option<usize> {
        match self {
            Self::Message {
                message_index,
                queued: false,
            } => Some(*message_index),
            Self::Message { queued: true, .. } | Self::Working => None,
        }
    }

    fn estimated_height(&self) -> f32 {
        match self {
            Self::Message { .. } => 48.0,
            Self::Working => 32.0,
        }
    }
}

#[derive(Default)]
pub(crate) struct ConversationRenderCache {
    pub(super) items: Vec<ConversationRenderItem>,
    pub(super) ruler_markers: Arc<[ConversationRulerMarker]>,
    estimated_height: f32,
}

impl ConversationRenderCache {
    pub(crate) fn build(harness: &Harness) -> Self {
        Self::update(harness, Self::default(), 0).0
    }

    pub(crate) fn update(
        harness: &Harness,
        old: Self,
        rebuild_from_message: usize,
    ) -> (Self, Range<usize>, usize) {
        let old_len = old.items.len();
        let prefix_len = old
            .items
            .iter()
            .take_while(|item| {
                item.message_index()
                    .is_some_and(|index| index < rebuild_from_message)
            })
            .count();
        let marker_prefix_len = old
            .ruler_markers
            .partition_point(|marker| marker.message_index < rebuild_from_message);
        let mut ruler_markers = Vec::with_capacity(
            marker_prefix_len + harness.messages.len().saturating_sub(rebuild_from_message),
        );
        ruler_markers.extend_from_slice(&old.ruler_markers[..marker_prefix_len]);
        let mut cache = Self {
            items: old.items.into_iter().take(prefix_len).collect(),
            ruler_markers: Arc::default(),
            estimated_height: 0.0,
        };
        for (message_index, message) in harness
            .messages
            .iter()
            .enumerate()
            .skip(rebuild_from_message)
        {
            ruler_markers.extend(conversation_ruler_marker(message, message_index));
            cache.items.push(ConversationRenderItem::Message {
                message_index,
                queued: false,
            });
        }
        if harness.status == HarnessStatus::Working {
            cache.items.push(ConversationRenderItem::Working);
        }
        for (queued_index, _) in harness.queued_messages.iter().enumerate() {
            cache.items.push(ConversationRenderItem::Message {
                message_index: queued_index,
                queued: true,
            });
        }
        cache.estimated_height = cache
            .items
            .iter()
            .map(ConversationRenderItem::estimated_height)
            .sum();
        cache.ruler_markers = ruler_markers.into();
        let new_count = cache.items.len() - prefix_len;
        (cache, prefix_len..old_len, new_count)
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
