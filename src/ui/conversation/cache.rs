use std::{ops::Range, sync::Arc};

use gpui::SharedString;

use crate::{
    markdown::{MarkdownBlock, MarkdownTable, MarkdownText, markdown_block_selection_text},
    model::{Harness, HarnessStatus, Message, MessageRole},
};

const TEXT_CHUNK_BYTES: usize = 1_536;
const TEXT_CHUNK_LINES: usize = 32;
const LIST_CHUNK_ITEMS: usize = 12;
const TABLE_CHUNK_ROWS: usize = 16;

pub(super) enum AssistantSegmentContent {
    Plain {
        text: SharedString,
        range: Range<usize>,
    },
    Markdown {
        block_index: usize,
        block: Option<MarkdownBlock>,
        selection_text: SharedString,
        selection_offset: usize,
    },
}

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
    Message {
        message_index: usize,
        queued: bool,
    },
    AssistantSegment {
        message_index: usize,
        segment_index: usize,
        content: AssistantSegmentContent,
        first: bool,
        top_gap: bool,
        last: bool,
        estimated_height: f32,
    },
    Working,
}

impl ConversationRenderItem {
    pub(super) fn message_index(&self) -> Option<usize> {
        match self {
            Self::Message {
                message_index,
                queued: false,
            }
            | Self::AssistantSegment { message_index, .. } => Some(*message_index),
            Self::Message { queued: true, .. } | Self::Working => None,
        }
    }

    fn estimated_height(&self) -> f32 {
        match self {
            Self::Message { .. } => 48.0,
            Self::AssistantSegment {
                estimated_height, ..
            } => *estimated_height,
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
            cache.push_message(message, message_index);
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

    fn push_message(&mut self, message: &Message, message_index: usize) {
        if message.role != MessageRole::Assistant {
            self.items.push(ConversationRenderItem::Message {
                message_index,
                queued: false,
            });
            return;
        }

        let first_item = self.items.len();
        if let Some(document) = message.markdown.as_ref() {
            let mut segments = Vec::new();
            let mut selection = String::new();
            for (block_index, original_block) in document.blocks.iter().enumerate() {
                for (chunk_index, block) in markdown_block_chunks(original_block)
                    .into_iter()
                    .enumerate()
                {
                    let rendered_block = block.as_ref().unwrap_or(original_block);
                    let piece = markdown_block_selection_text(rendered_block);
                    if !piece.is_empty() && !selection.is_empty() {
                        selection.push('\n');
                    }
                    let selection_offset = selection.len();
                    selection.push_str(&piece);
                    let estimate = block.as_ref().map_or_else(
                        || estimate_markdown_height(block_index, document),
                        estimate_block_height,
                    );
                    segments.push((
                        block_index,
                        block,
                        selection_offset,
                        block_index > 0 && chunk_index == 0,
                        estimate,
                    ));
                }
            }
            let selection_text = SharedString::from(selection);
            for (block_index, block, selection_offset, top_gap, estimated_height) in segments {
                self.items.push(ConversationRenderItem::AssistantSegment {
                    message_index,
                    segment_index: self.items.len() - first_item,
                    content: AssistantSegmentContent::Markdown {
                        block_index,
                        block,
                        selection_text: selection_text.clone(),
                        selection_offset,
                    },
                    first: false,
                    top_gap,
                    last: false,
                    estimated_height,
                });
            }
        } else {
            let text = message.display_text.clone();
            for range in text_chunk_ranges(&text) {
                let estimate = estimate_text_height(&text[range.clone()], 22.0);
                self.items.push(ConversationRenderItem::AssistantSegment {
                    message_index,
                    segment_index: self.items.len() - first_item,
                    content: AssistantSegmentContent::Plain {
                        text: text.clone(),
                        range,
                    },
                    first: false,
                    top_gap: false,
                    last: false,
                    estimated_height: estimate,
                });
            }
        }

        if self.items.len() == first_item {
            self.items.push(ConversationRenderItem::Message {
                message_index,
                queued: false,
            });
            return;
        }
        if let ConversationRenderItem::AssistantSegment { first, .. } = &mut self.items[first_item]
        {
            *first = true;
        }
        if let Some(ConversationRenderItem::AssistantSegment { last, .. }) = self.items.last_mut() {
            *last = true;
        }
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

fn text_chunk_ranges(text: &str) -> Vec<Range<usize>> {
    if text.is_empty() {
        return std::iter::once(0..0).collect();
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = text.len().min(start + TEXT_CHUNK_BYTES);
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }

        let line_end = text[start..]
            .match_indices('\n')
            .nth(TEXT_CHUNK_LINES - 1)
            .map(|(index, _)| start + index + 1);
        if let Some(line_end) = line_end {
            end = end.min(line_end);
        }
        if end < text.len()
            && line_end.is_none_or(|line_end| end < line_end)
            && let Some((index, character)) = text[start..end]
                .char_indices()
                .rev()
                .find(|(_, character)| character.is_whitespace())
            && index > 0
        {
            end = start + index + character.len_utf8();
        }
        if end <= start {
            end = text[start..]
                .chars()
                .next()
                .map_or(text.len(), |character| start + character.len_utf8());
        }
        ranges.push(start..end);
        start = end;
    }
    ranges
}

fn markdown_block_chunks(block: &MarkdownBlock) -> Vec<Option<MarkdownBlock>> {
    match block {
        MarkdownBlock::Paragraph(text) => {
            let ranges = text_chunk_ranges(&text.text);
            if ranges.len() == 1 {
                vec![None]
            } else {
                ranges
                    .into_iter()
                    .map(|range| Some(MarkdownBlock::Paragraph(slice_markdown_text(text, range))))
                    .collect()
            }
        }
        MarkdownBlock::CodeBlock { language, code } => {
            let ranges = text_chunk_ranges(code);
            if ranges.len() == 1 {
                vec![None]
            } else {
                ranges
                    .into_iter()
                    .map(|range| {
                        Some(MarkdownBlock::CodeBlock {
                            language: language.clone(),
                            code: code[range].to_string(),
                        })
                    })
                    .collect()
            }
        }
        MarkdownBlock::BlockQuote(blocks) if !blocks.is_empty() => blocks
            .iter()
            .flat_map(|block| {
                markdown_block_chunks(block).into_iter().map(move |chunk| {
                    Some(MarkdownBlock::BlockQuote(vec![
                        chunk.unwrap_or_else(|| block.clone()),
                    ]))
                })
            })
            .collect(),
        MarkdownBlock::List { start, items } if items.len() > LIST_CHUNK_ITEMS => items
            .chunks(LIST_CHUNK_ITEMS)
            .enumerate()
            .map(|(chunk_index, items)| {
                Some(MarkdownBlock::List {
                    start: start.map(|start| start + (chunk_index * LIST_CHUNK_ITEMS) as u64),
                    items: items.to_vec(),
                })
            })
            .collect(),
        MarkdownBlock::Table(table) if table.rows.len() > TABLE_CHUNK_ROWS => table
            .rows
            .chunks(TABLE_CHUNK_ROWS)
            .map(|rows| {
                Some(MarkdownBlock::Table(MarkdownTable {
                    alignments: table.alignments.clone(),
                    header: table.header.clone(),
                    rows: rows.to_vec(),
                    scroll: table.scroll.clone(),
                }))
            })
            .collect(),
        _ => vec![None],
    }
}

fn slice_markdown_text(text: &MarkdownText, range: Range<usize>) -> MarkdownText {
    let spans = text
        .spans
        .iter()
        .filter_map(|span| {
            let start = span.range.start.max(range.start);
            let end = span.range.end.min(range.end);
            (start < end).then(|| crate::markdown::MarkdownSpan {
                range: start - range.start..end - range.start,
                style: span.style.clone(),
            })
        })
        .collect();
    MarkdownText {
        text: text.text[range.clone()].to_string(),
        spans,
    }
}

fn estimate_text_height(text: &str, line_height: f32) -> f32 {
    let wrapped_lines = text
        .lines()
        .map(|line| (line.chars().count() / 76).max(1))
        .sum::<usize>()
        .max(1);
    wrapped_lines as f32 * line_height
}

fn estimate_block_height(block: &MarkdownBlock) -> f32 {
    match block {
        MarkdownBlock::Paragraph(text) => estimate_text_height(&text.text, 22.0),
        MarkdownBlock::Heading { text, .. } => estimate_text_height(&text.text, 27.0),
        MarkdownBlock::CodeBlock { code, .. } => estimate_text_height(code, 19.0) + 18.0,
        MarkdownBlock::List { items, .. } => items.len().max(1) as f32 * 28.0,
        MarkdownBlock::Table(table) => (table.rows.len() + 1) as f32 * 38.0,
        MarkdownBlock::BlockQuote(blocks) => blocks.len().max(1) as f32 * 32.0,
        MarkdownBlock::Rule => 17.0,
    }
}

fn estimate_markdown_height(
    block_index: usize,
    document: &crate::markdown::MarkdownDocument,
) -> f32 {
    document
        .blocks
        .get(block_index)
        .map_or(48.0, estimate_block_height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::{MarkdownSpan, MarkdownSpanStyle};

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

    #[test]
    fn long_text_is_split_at_utf8_boundaries() {
        let text = "é".repeat(TEXT_CHUNK_BYTES);
        let ranges = text_chunk_ranges(&text);
        assert!(ranges.len() > 1);
        assert!(ranges.iter().all(|range| text.is_char_boundary(range.start)
            && text.is_char_boundary(range.end)));
        assert_eq!(ranges.first().unwrap().start, 0);
        assert_eq!(ranges.last().unwrap().end, text.len());
    }

    #[test]
    fn markdown_spans_are_rebased_when_paragraphs_are_chunked() {
        let text = MarkdownText {
            text: "a".repeat(TEXT_CHUNK_BYTES * 2),
            spans: vec![MarkdownSpan {
                range: TEXT_CHUNK_BYTES - 10..TEXT_CHUNK_BYTES + 10,
                style: MarkdownSpanStyle {
                    strong: true,
                    ..Default::default()
                },
            }],
        };
        let chunks = markdown_block_chunks(&MarkdownBlock::Paragraph(text));
        assert!(chunks.len() > 1);
        assert!(chunks.iter().filter_map(Option::as_ref).all(|block| {
            let MarkdownBlock::Paragraph(text) = block else {
                return false;
            };
            text.spans
                .iter()
                .all(|span| span.range.end <= text.text.len())
        }));
    }
}
