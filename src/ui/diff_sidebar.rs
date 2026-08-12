//! Builds and interacts with the virtualized turn-diff sidebar.

mod render;

use std::{cell::Cell, ops::Range, rc::Rc, sync::Arc};

use gpui::{
    AnyElement, Context, CursorStyle, DragMoveEvent, HighlightStyle, IntoElement, MouseButton,
    Pixels, ScrollHandle, SharedString, StyledText, Window, deferred, div, list, point, prelude::*,
    px, svg,
};

use super::{composer::dropdown_arrow, scrollbar_drag_offset};

use crate::{
    app::Dirigent,
    diff::{
        DiffHunk, DiffRowKind, DiffScope, DiffSelectionReference, DiffViewMode, FileDiff,
        FileDiffKind, SyntaxSpan, TurnDiff, TurnDiffStatus,
    },
    theme::{
        bg, blue, border, code_text, green, muted, orange, red, rgb, surface, surface_hover,
        theme_text,
    },
};

#[derive(Clone, Copy)]
struct DiffSidebarResizeDrag {
    width: f32,
    mouse_x: Pixels,
}

struct DiffSidebarResizePreview;

#[derive(Clone)]
struct DiffListScrollbarDrag {
    list: gpui::ListState,
    start_pointer: Rc<Cell<Pixels>>,
    start_offset: f32,
    max_offset: f32,
    thumb_travel: f32,
}

const MIN_THREAD_WIDTH: f32 = 420.0;
const MIN_DIFF_SIDEBAR_WIDTH: f32 = 420.0;

fn diff_sidebar_replaces_thread(
    viewport_width: f32,
    sidebar_width: f32,
    diff_sidebar_width: f32,
) -> bool {
    viewport_width < sidebar_width + MIN_THREAD_WIDTH + diff_sidebar_width
}

impl gpui::Render for DiffSidebarResizePreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn status_label(status: TurnDiffStatus) -> &'static str {
    match status {
        TurnDiffStatus::Completed => "complete",
        TurnDiffStatus::Aborted => "aborted",
        TurnDiffStatus::Failed => "failed",
        TurnDiffStatus::Interrupted => "interrupted",
        TurnDiffStatus::Unavailable => "unavailable",
    }
}

fn file_kind_label(kind: FileDiffKind) -> &'static str {
    match kind {
        FileDiffKind::Added => "A",
        FileDiffKind::Modified => "M",
        FileDiffKind::Deleted => "D",
        FileDiffKind::Renamed => "R",
        FileDiffKind::Binary => "B",
        FileDiffKind::Omitted => "…",
        FileDiffKind::ModeChanged => "↕",
    }
}

fn render_diff_stats(additions: usize, deletions: usize) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap_1()
        .whitespace_nowrap()
        .text_xs()
        .child(
            div()
                .text_color(rgb(green()))
                .child(format!("+{additions}")),
        )
        .child(div().text_color(rgb(red())).child(format!("−{deletions}")))
        .into_any_element()
}

fn language_for_path(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| match extension {
            "rs" => "rust",
            "js" | "jsx" => "javascript",
            "ts" => "typescript",
            "py" => "python",
            "sh" => "bash",
            "yml" => "yaml",
            "md" => "markdown",
            other => other,
        })
        .unwrap_or("text")
        .to_string()
}

const DIFF_CHUNK_LINES: usize = 32;

fn line_offsets(text: Option<&str>) -> Vec<usize> {
    let Some(text) = text else {
        return Vec::new();
    };
    let mut offsets = vec![0];
    offsets.extend(
        text.match_indices('\n')
            .filter_map(|(index, _)| (index + 1 < text.len()).then_some(index + 1)),
    );
    offsets
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffGutterContent {
    Unified {
        sign: Option<char>,
        old_number: Option<u32>,
        new_number: Option<u32>,
    },
    Single(Option<u32>),
}

impl DiffGutterContent {
    fn label(self) -> SharedString {
        match self {
            Self::Unified {
                sign,
                old_number,
                new_number,
            } => format!(
                "{:>5} {:>5} {}",
                old_number.map_or_else(String::new, |number| number.to_string()),
                new_number.map_or_else(String::new, |number| number.to_string()),
                sign.unwrap_or(' '),
            )
            .into(),
            Self::Single(number) => format!(
                "{:>5}",
                number.map_or_else(String::new, |number| number.to_string())
            )
            .into(),
        }
    }
}

#[derive(Clone)]
struct DiffGutterLine {
    label: SharedString,
    color: u32,
    background: Option<u32>,
}

struct DiffBlockBuilder {
    text: String,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    gutter: Vec<DiffGutterLine>,
}

impl DiffBlockBuilder {
    fn new() -> Self {
        Self {
            text: String::new(),
            highlights: Vec::new(),
            gutter: Vec::new(),
        }
    }

    fn finish(mut self) -> DiffBlock {
        if self.text.ends_with('\n') {
            self.text.pop();
            let text_len = self.text.len();
            for (range, _) in &mut self.highlights {
                range.end = range.end.min(text_len);
            }
            self.highlights.retain(|(range, _)| !range.is_empty());
        }
        DiffBlock::new(self.text.into(), self.highlights, self.gutter)
    }
}

struct GutterColumn {
    text: SharedString,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
}

struct GutterColumns {
    old: GutterColumn,
    new: GutterColumn,
    sign: GutterColumn,
}

struct DiffBlock {
    text: SharedString,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    gutter: Vec<DiffGutterLine>,
    gutter_text: SharedString,
    gutter_highlights: Vec<(Range<usize>, HighlightStyle)>,
    gutter_columns: Option<GutterColumns>,
    gutter_border: GutterColumn,
    code_padding: GutterColumn,
}

fn push_gutter_column_line(
    text: &mut String,
    highlights: &mut Vec<(Range<usize>, HighlightStyle)>,
    value: &str,
    color: u32,
    line_index: usize,
) {
    if line_index > 0 {
        text.push('\n');
    }
    let start = text.len();
    if value.is_empty() {
        text.push('\u{00a0}');
    } else {
        text.push_str(value);
    }
    highlights.push((
        start..text.len(),
        HighlightStyle {
            color: Some(rgb(color).into()),
            ..Default::default()
        },
    ));
}

impl DiffBlock {
    fn new(
        text: SharedString,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
        gutter: Vec<DiffGutterLine>,
    ) -> Self {
        let unified = gutter.first().is_some_and(|line| line.label.len() > 5);
        let mut gutter_text = String::new();
        let mut gutter_highlights = Vec::with_capacity(gutter.len());
        let mut old_text = String::new();
        let mut old_highlights = Vec::with_capacity(gutter.len());
        let mut new_text = String::new();
        let mut new_highlights = Vec::with_capacity(gutter.len());
        let mut sign_text = String::new();
        let mut sign_highlights = Vec::with_capacity(gutter.len());
        let mut border_text = String::new();
        let mut border_highlights = Vec::with_capacity(gutter.len());
        let mut padding_text = String::new();
        let mut padding_highlights = Vec::with_capacity(gutter.len());
        for (line_index, line) in gutter.iter().enumerate() {
            if line_index > 0 {
                gutter_text.push('\n');
            }
            let start = gutter_text.len();
            gutter_text.push_str(&line.label);
            let color = if line.color == border() {
                muted()
            } else {
                line.color
            };
            gutter_highlights.push((
                start..gutter_text.len(),
                HighlightStyle {
                    color: Some(rgb(color).into()),
                    ..Default::default()
                },
            ));
            if unified {
                let label: &str = &line.label;
                push_gutter_column_line(
                    &mut old_text,
                    &mut old_highlights,
                    label[..5].trim(),
                    color,
                    line_index,
                );
                push_gutter_column_line(
                    &mut new_text,
                    &mut new_highlights,
                    label[6..11].trim(),
                    color,
                    line_index,
                );
                push_gutter_column_line(
                    &mut sign_text,
                    &mut sign_highlights,
                    label[12..].trim(),
                    color,
                    line_index,
                );
            }
            if line_index > 0 {
                border_text.push('\n');
            }
            let border_start = border_text.len();
            border_text.push('\u{00a0}');
            border_highlights.push((
                border_start..border_text.len(),
                HighlightStyle {
                    background_color: Some(rgb(line.color).into()),
                    ..Default::default()
                },
            ));

            if line_index > 0 {
                padding_text.push('\n');
            }
            let padding_start = padding_text.len();
            padding_text.push('\u{00a0}');
            if let Some(color) = line.background {
                padding_highlights.push((
                    padding_start..padding_text.len(),
                    HighlightStyle {
                        background_color: Some(rgb(color).opacity(0.10).into()),
                        ..Default::default()
                    },
                ));
            }
        }
        Self {
            text,
            highlights,
            gutter,
            gutter_text: gutter_text.into(),
            gutter_highlights,
            gutter_columns: unified.then(|| GutterColumns {
                old: GutterColumn {
                    text: old_text.into(),
                    highlights: old_highlights,
                },
                new: GutterColumn {
                    text: new_text.into(),
                    highlights: new_highlights,
                },
                sign: GutterColumn {
                    text: sign_text.into(),
                    highlights: sign_highlights,
                },
            }),
            gutter_border: GutterColumn {
                text: border_text.into(),
                highlights: border_highlights,
            },
            code_padding: GutterColumn {
                text: padding_text.into(),
                highlights: padding_highlights,
            },
        }
    }

    fn line_count(&self) -> usize {
        self.gutter.len()
    }

    fn chunks(&self) -> Vec<Self> {
        let line_count = self.line_count();
        if line_count <= DIFF_CHUNK_LINES {
            return vec![Self::new(
                self.text.clone(),
                self.highlights.clone(),
                self.gutter.clone(),
            )];
        }

        let mut starts = vec![0];
        starts.extend(self.text.match_indices('\n').map(|(index, _)| index + 1));
        debug_assert_eq!(starts.len(), line_count);
        (0..line_count)
            .step_by(DIFF_CHUNK_LINES)
            .map(|first_line| {
                let end_line = (first_line + DIFF_CHUNK_LINES).min(line_count);
                let byte_start = starts[first_line];
                let byte_end = if end_line < line_count {
                    starts[end_line] - 1
                } else {
                    self.text.len()
                };
                let highlights = self
                    .highlights
                    .iter()
                    .filter_map(|(range, style)| {
                        let start = range.start.max(byte_start);
                        let end = range.end.min(byte_end);
                        (start < end).then_some((start - byte_start..end - byte_start, *style))
                    })
                    .collect();
                Self::new(
                    self.text[byte_start..byte_end].to_string().into(),
                    highlights,
                    self.gutter[first_line..end_line].to_vec(),
                )
            })
            .collect()
    }
}

fn append_syntax(
    block: &mut DiffBlockBuilder,
    text: &str,
    line_number: Option<u32>,
    source_offsets: &[usize],
    source_spans: &[SyntaxSpan],
    changed_color: Option<u32>,
) {
    let line_start = block.text.len();
    block.text.push_str(text);
    block.text.push('\n');
    let line_end = block.text.len();
    let changed_background = changed_color.map(|color| rgb(color).opacity(0.10).into());
    let mut rendered_through = line_start;

    if let Some(line_number) = line_number
        && let Some(&file_start) = source_offsets.get(line_number.saturating_sub(1) as usize)
    {
        let file_end = file_start + text.len();
        let first_span = source_spans.partition_point(|span| span.range.end <= file_start);
        for span in &source_spans[first_span..] {
            if span.range.start >= file_end {
                break;
            }
            let start = line_start + span.range.start.max(file_start) - file_start;
            let end = line_start + span.range.end.min(file_end) - file_start;
            if start >= end {
                continue;
            }
            if let Some(background_color) = changed_background
                && rendered_through < start
            {
                block.highlights.push((
                    rendered_through..start,
                    HighlightStyle {
                        background_color: Some(background_color),
                        ..Default::default()
                    },
                ));
            }
            block.highlights.push((
                start..end,
                HighlightStyle {
                    color: Some(rgb(span.color).into()),
                    background_color: changed_background,
                    ..Default::default()
                },
            ));
            rendered_through = end;
        }
    }
    if let Some(background_color) = changed_background
        && rendered_through < line_end
    {
        block.highlights.push((
            rendered_through..line_end,
            HighlightStyle {
                background_color: Some(background_color),
                ..Default::default()
            },
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn append_diff_line(
    block: &mut DiffBlockBuilder,
    gutter: DiffGutterContent,
    text: &str,
    line_number: Option<u32>,
    source_offsets: &[usize],
    source_spans: &[SyntaxSpan],
    changed_color: Option<u32>,
) {
    block.gutter.push(DiffGutterLine {
        label: gutter.label(),
        color: changed_color.unwrap_or_else(border),
        background: changed_color,
    });
    append_syntax(
        block,
        text,
        line_number,
        source_offsets,
        source_spans,
        changed_color,
    );
}

fn unified_hunk_block(
    file: &FileDiff,
    hunk: &DiffHunk,
    old_offsets: &[usize],
    new_offsets: &[usize],
) -> DiffBlock {
    let mut block = DiffBlockBuilder::new();
    let mut index = 0;
    while index < hunk.rows.len() {
        let row = &hunk.rows[index];
        if row.kind == DiffRowKind::Context {
            append_diff_line(
                &mut block,
                DiffGutterContent::Unified {
                    sign: None,
                    old_number: row.old_number,
                    new_number: row.new_number,
                },
                row.new_text.as_deref().unwrap_or_default(),
                row.new_number,
                new_offsets,
                &file.new_highlights,
                None,
            );
            index += 1;
            continue;
        }

        let changed_start = index;
        while index < hunk.rows.len() && hunk.rows[index].kind != DiffRowKind::Context {
            index += 1;
        }
        let changed_rows = &hunk.rows[changed_start..index];
        for row in changed_rows {
            if let Some(text) = row.old_text.as_deref() {
                append_diff_line(
                    &mut block,
                    DiffGutterContent::Unified {
                        sign: Some('-'),
                        old_number: row.old_number,
                        new_number: None,
                    },
                    text,
                    row.old_number,
                    old_offsets,
                    &file.old_highlights,
                    Some(red()),
                );
            }
        }
        for row in changed_rows {
            if let Some(text) = row.new_text.as_deref() {
                append_diff_line(
                    &mut block,
                    DiffGutterContent::Unified {
                        sign: Some('+'),
                        old_number: None,
                        new_number: row.new_number,
                    },
                    text,
                    row.new_number,
                    new_offsets,
                    &file.new_highlights,
                    Some(green()),
                );
            }
        }
    }
    block.finish()
}

fn split_hunk_block(
    file: &FileDiff,
    hunk: &DiffHunk,
    old_side: bool,
    offsets: &[usize],
) -> DiffBlock {
    let mut block = DiffBlockBuilder::new();
    let spans = if old_side {
        &file.old_highlights
    } else {
        &file.new_highlights
    };
    for row in &hunk.rows {
        let (number, text, changed) = if old_side {
            (
                row.old_number,
                row.old_text.as_deref(),
                matches!(row.kind, DiffRowKind::Removed | DiffRowKind::Replaced),
            )
        } else {
            (
                row.new_number,
                row.new_text.as_deref(),
                matches!(row.kind, DiffRowKind::Added | DiffRowKind::Replaced),
            )
        };
        let changed_color =
            (changed && text.is_some()).then_some(if old_side { red() } else { green() });
        append_diff_line(
            &mut block,
            DiffGutterContent::Single(number),
            text.unwrap_or_default(),
            number,
            offsets,
            spans,
            changed_color,
        );
    }
    block.finish()
}

struct PreparedDiffFile {
    content_width: f32,
    language: String,
}

enum DiffRenderItem {
    FileHeader {
        file_index: usize,
        last_in_file: bool,
    },
    UnifiedChunk {
        file_index: usize,
        hunk_index: usize,
        chunk_index: usize,
        block: Box<DiffBlock>,
        reference: DiffSelectionReference,
        top_gap: bool,
        last_in_file: bool,
    },
    SplitChunk {
        file_index: usize,
        hunk_index: usize,
        chunk_index: usize,
        old_block: Box<DiffBlock>,
        new_block: Box<DiffBlock>,
        old_reference: DiffSelectionReference,
        new_reference: DiffSelectionReference,
        top_gap: bool,
        last_in_file: bool,
    },
}

impl DiffRenderItem {
    fn mark_last_in_file(&mut self) {
        match self {
            Self::FileHeader { last_in_file, .. }
            | Self::UnifiedChunk { last_in_file, .. }
            | Self::SplitChunk { last_in_file, .. } => *last_in_file = true,
        }
    }

    fn estimated_height(&self) -> f32 {
        match self {
            Self::FileHeader { .. } => 34.0,
            Self::UnifiedChunk { block, top_gap, .. } => {
                block.line_count() as f32 * 18.0 + if *top_gap { 8.0 } else { 0.0 }
            }
            Self::SplitChunk {
                old_block, top_gap, ..
            } => old_block.line_count() as f32 * 18.0 + if *top_gap { 8.0 } else { 0.0 },
        }
    }
}

#[derive(Default)]
pub(crate) struct DiffRenderCache {
    files: Vec<PreparedDiffFile>,
    items: Vec<DiffRenderItem>,
    estimated_height: f32,
}

impl DiffRenderCache {
    fn build(turn: &TurnDiff, mode: DiffViewMode) -> Self {
        let mut cache = Self::default();
        for (file_index, file) in turn.files.iter().enumerate() {
            let old_offsets = line_offsets(file.old_text.as_deref());
            let new_offsets = line_offsets(file.new_text.as_deref());
            cache.files.push(PreparedDiffFile {
                content_width: Dirigent::diff_file_code_width(file),
                language: language_for_path(&file.path),
            });
            let first_item = cache.items.len();
            cache.items.push(DiffRenderItem::FileHeader {
                file_index,
                last_in_file: false,
            });

            for (hunk_index, hunk) in file.hunks.iter().enumerate() {
                let top_gap = hunk_index > 0;
                match mode {
                    DiffViewMode::Unified => {
                        let reference = DiffSelectionReference {
                            turn_id: turn.id,
                            path: file.path.clone(),
                            side: "unified",
                            lines: format!(
                                "-{},{} +{},{}",
                                hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
                            ),
                            language: cache.files[file_index].language.clone(),
                        };
                        for (chunk_index, block) in
                            unified_hunk_block(file, hunk, &old_offsets, &new_offsets)
                                .chunks()
                                .into_iter()
                                .enumerate()
                        {
                            cache.items.push(DiffRenderItem::UnifiedChunk {
                                file_index,
                                hunk_index,
                                chunk_index,
                                block: Box::new(block),
                                reference: reference.clone(),
                                top_gap: top_gap && chunk_index == 0,
                                last_in_file: false,
                            });
                        }
                    }
                    DiffViewMode::Split => {
                        let old_reference = DiffSelectionReference {
                            turn_id: turn.id,
                            path: file.old_path.clone().unwrap_or_else(|| file.path.clone()),
                            side: "old",
                            lines: format!(
                                "{}–{}",
                                hunk.old_start,
                                hunk.old_start + hunk.old_len.saturating_sub(1)
                            ),
                            language: cache.files[file_index].language.clone(),
                        };
                        let new_reference = DiffSelectionReference {
                            turn_id: turn.id,
                            path: file.path.clone(),
                            side: "new",
                            lines: format!(
                                "{}–{}",
                                hunk.new_start,
                                hunk.new_start + hunk.new_len.saturating_sub(1)
                            ),
                            language: cache.files[file_index].language.clone(),
                        };
                        let old_chunks = split_hunk_block(file, hunk, true, &old_offsets).chunks();
                        let new_chunks = split_hunk_block(file, hunk, false, &new_offsets).chunks();
                        debug_assert_eq!(old_chunks.len(), new_chunks.len());
                        for (chunk_index, (old_block, new_block)) in
                            old_chunks.into_iter().zip(new_chunks).enumerate()
                        {
                            cache.items.push(DiffRenderItem::SplitChunk {
                                file_index,
                                hunk_index,
                                chunk_index,
                                old_block: Box::new(old_block),
                                new_block: Box::new(new_block),
                                old_reference: old_reference.clone(),
                                new_reference: new_reference.clone(),
                                top_gap: top_gap && chunk_index == 0,
                                last_in_file: false,
                            });
                        }
                    }
                }
            }
            debug_assert!(cache.items.len() > first_item);
            cache.items.last_mut().unwrap().mark_last_in_file();
        }
        cache.estimated_height = cache
            .items
            .iter()
            .map(DiffRenderItem::estimated_height)
            .sum();
        cache
    }

    fn item_height_hint(&self) -> f32 {
        if self.items.is_empty() {
            18.0
        } else {
            (self.estimated_height / self.items.len() as f32).max(18.0)
        }
    }
}
