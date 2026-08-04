use std::ops::Range;

use gpui::{
    AnyElement, Context, CursorStyle, DragMoveEvent, HighlightStyle, IntoElement, Pixels,
    ScrollHandle, SharedString, StyledText, Window, deferred, div, list, prelude::*, px, svg,
};

use super::composer::dropdown_arrow;

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

impl Dirigent {
    pub(super) fn render_changes_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.diff_sidebar_open {
            return div().into_any_element();
        }
        div()
            .id("changes-toggle")
            .absolute()
            .top(px(4.0))
            .right(px(4.0))
            .size(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .text_color(rgb(muted()))
            .hover(|style| style.text_color(rgb(theme_text())))
            .on_click(cx.listener(|this, _, _, cx| {
                this.toggle_diff_sidebar();
                cx.notify();
            }))
            .child(
                svg()
                    .path("icon/panel-right.svg")
                    .size(px(18.0))
                    .text_color(rgb(theme_text())),
            )
            .into_any_element()
    }

    fn render_diff_mode_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let (label, next) = match self.diff_view_mode {
            DiffViewMode::Unified => ("Unified", DiffViewMode::Split),
            DiffViewMode::Split => ("Split", DiffViewMode::Unified),
        };
        div()
            .id("diff-mode-toggle")
            .h(px(24.0))
            .px_2()
            .flex_none()
            .flex()
            .items_center()
            .rounded_md()
            .whitespace_nowrap()
            .text_xs()
            .text_color(rgb(theme_text()))
            .hover(|style| style.bg(rgb(surface_hover())))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_diff_view_mode(next);
                cx.notify();
            }))
            .child(label)
            .into_any_element()
    }

    fn render_diff_scope_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let (label, next) = match self.diff_scope {
            DiffScope::Cumulative => ("Cumulative", DiffScope::Turn),
            DiffScope::Turn => ("Turn only", DiffScope::Cumulative),
        };
        div()
            .id("diff-scope-toggle")
            .h(px(24.0))
            .px_2()
            .flex_none()
            .flex()
            .items_center()
            .rounded_md()
            .whitespace_nowrap()
            .text_xs()
            .text_color(rgb(theme_text()))
            .hover(|style| style.bg(rgb(surface_hover())))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_diff_scope(next);
                cx.notify();
                cx.stop_propagation();
            }))
            .child(label)
            .into_any_element()
    }

    fn render_diff_turn_row(
        &self,
        turn: &TurnDiff,
        selected: bool,
        live: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let turn_id = turn.id;
        div()
            .id(("diff-turn", turn.id))
            .w_full()
            .px_2()
            .py_1()
            .flex()
            .items_center()
            .gap_2()
            .rounded_md()
            .when(selected, |element| element.bg(rgb(surface_hover())))
            .hover(|style| style.bg(rgb(surface_hover())))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_diff_turn(turn_id);
                cx.notify();
            }))
            .child(
                div()
                    .w(px(54.0))
                    .flex_none()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(if selected { blue() } else { muted() }))
                    .child(format!("Turn {}", turn.id)),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_xs()
                            .text_color(rgb(theme_text()))
                            .child(turn.prompt.clone()),
                    )
                    .child(div().text_xs().text_color(rgb(muted())).child(if live {
                        format!(
                            "{} files · +{} −{}",
                            turn.files.len(),
                            turn.additions,
                            turn.deletions
                        )
                    } else {
                        format!(
                            "{} · {} files · +{} −{}",
                            status_label(turn.status),
                            turn.files.len(),
                            turn.additions,
                            turn.deletions
                        )
                    })),
            )
            .into_any_element()
    }

    fn render_diff_turn_picker(
        &self,
        turns: &[TurnDiff],
        active_preview: Option<&TurnDiff>,
        selected_turn_id: Option<u64>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.diff_turn_dropdown_open;
        let selected = selected_turn_id.and_then(|id| {
            active_preview
                .filter(|preview| preview.id == id)
                .or_else(|| turns.iter().find(|turn| turn.id == id))
        });
        let label = selected
            .map(|turn| format!("Turn {}", turn.id))
            .unwrap_or_else(|| "No turns".into());

        div()
            .relative()
            .flex_none()
            .child(
                div()
                    .id("diff-turn-picker")
                    .h(px(24.0))
                    .w_auto()
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .whitespace_nowrap()
                    .text_xs()
                    .text_color(rgb(theme_text()))
                    .when(!turns.is_empty() || active_preview.is_some(), |element| {
                        element
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(surface_hover())))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.diff_turn_dropdown_open = !open;
                                cx.notify();
                                cx.stop_propagation();
                            }))
                    })
                    .child(label)
                    .when(!turns.is_empty() || active_preview.is_some(), |element| {
                        element.child(dropdown_arrow(open))
                    }),
            )
            .when(open, |element| {
                element.child(
                    deferred(
                        div()
                            .id("diff-turn-dropdown")
                            .absolute()
                            .top(px(32.0))
                            .left_0()
                            .w(px(310.0))
                            .max_h(px(320.0))
                            .p_1()
                            .overflow_y_scroll()
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(border()))
                            .bg(rgb(crate::theme::menu_bg()))
                            .shadow_lg()
                            .occlude()
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|_, _, _, cx| cx.stop_propagation()),
                            )
                            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                            .when_some(active_preview, |element, preview| {
                                element.child(self.render_diff_turn_row(
                                    preview,
                                    Some(preview.id) == selected_turn_id,
                                    true,
                                    cx,
                                ))
                            })
                            .children(turns.iter().rev().map(|turn| {
                                self.render_diff_turn_row(
                                    turn,
                                    Some(turn.id) == selected_turn_id,
                                    false,
                                    cx,
                                )
                            })),
                    )
                    .priority(3),
                )
            })
            .into_any_element()
    }

    fn render_diff_code(
        &self,
        id: String,
        block: &DiffBlock,
        reference: &DiffSelectionReference,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_styled_selectable_text_with_reference(
            id,
            block.text.clone(),
            &block.highlights,
            &[],
            &[],
            false,
            Some(reference.clone()),
            cx,
        )
    }

    fn diff_code_scroll(&self, key: &str) -> ScrollHandle {
        self.diff_code_scrolls
            .borrow_mut()
            .entry(key.to_string())
            .or_default()
            .clone()
    }

    fn diff_file_code_width(file: &FileDiff) -> f32 {
        let columns = file
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.rows)
            .flat_map(|row| [row.old_text.as_deref(), row.new_text.as_deref()])
            .flatten()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0);
        (columns as f32 * 7.4 + 20.0).max(120.0)
    }

    fn render_diff_list_scrollbar(&self) -> AnyElement {
        let max_offset = self.diff_list.max_offset_for_scrollbar().y.as_f32();
        let viewport = self.diff_list.viewport_bounds().size.height.as_f32();
        let thumb_fraction = if viewport > 0.0 && max_offset > 0.0 {
            let minimum = (10.0 / viewport).clamp(0.08, 1.0);
            (viewport / (viewport + max_offset)).clamp(minimum, 1.0)
        } else {
            1.0
        };
        let scroll_fraction = if max_offset > 0.0 {
            (-self.diff_list.scroll_px_offset_for_scrollbar().y.as_f32() / max_offset)
                .clamp(0.0, 1.0)
        } else {
            0.0
        };
        let thumb_top = (1.0 - thumb_fraction) * scroll_fraction;

        div()
            .id("diff-list-scrollbar")
            .absolute()
            .top(px(3.0))
            .bottom(px(3.0))
            .right(px(2.0))
            .w(px(2.0))
            .rounded_full()
            .when(max_offset > 0.0, |element| {
                element.bg(gpui::rgba(0xffffff16)).child(
                    div()
                        .absolute()
                        .top(gpui::relative(thumb_top))
                        .h(gpui::relative(thumb_fraction))
                        .min_h(px(10.0))
                        .w_full()
                        .rounded_full()
                        .bg(rgb(muted())),
                )
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_diff_block(
        &self,
        id: String,
        block: &DiffBlock,
        gutter_width: f32,
        content_width: f32,
        reference: &DiffSelectionReference,
        scroll: &ScrollHandle,
        show_scrollbar: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let scrollbar_id = format!("{id}-scrollbar");
        let scroll_id = format!("{id}-scroll");
        let border_column = div().w(px(4.0)).flex_none().overflow_hidden().child(
            StyledText::new(block.gutter_border.text.clone())
                .with_highlights(block.gutter_border.highlights.iter().cloned()),
        );
        let gutter_content = if let Some(columns) = block.gutter_columns.as_ref() {
            div()
                .w_full()
                .flex()
                .child(
                    div().w(px(38.0)).flex_none().pr_1().text_right().child(
                        StyledText::new(columns.old.text.clone())
                            .with_highlights(columns.old.highlights.iter().cloned()),
                    ),
                )
                .child(
                    div().w(px(38.0)).flex_none().pr_1().text_right().child(
                        StyledText::new(columns.new.text.clone())
                            .with_highlights(columns.new.highlights.iter().cloned()),
                    ),
                )
                .child(
                    div().w(px(14.0)).flex_none().text_center().child(
                        StyledText::new(columns.sign.text.clone())
                            .with_highlights(columns.sign.highlights.iter().cloned()),
                    ),
                )
                .child(border_column)
                .into_any_element()
        } else {
            div()
                .w_full()
                .flex()
                .child(
                    div().min_w_0().flex_1().pr_1().text_right().child(
                        StyledText::new(block.gutter_text.clone())
                            .with_highlights(block.gutter_highlights.iter().cloned()),
                    ),
                )
                .child(border_column)
                .into_any_element()
        };
        let gutter = div()
            .relative()
            .w(px(gutter_width))
            .flex_none()
            .whitespace_nowrap()
            .text_xs()
            .line_height(px(18.0))
            .child(gutter_content);
        let code_padding = div()
            .w(px(4.0))
            .flex_none()
            .overflow_hidden()
            .text_xs()
            .line_height(px(18.0))
            .child(
                StyledText::new(block.code_padding.text.clone())
                    .with_highlights(block.code_padding.highlights.iter().cloned()),
            );
        let mut code_scroll = div()
            .id(scroll_id)
            .w_full()
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(scroll)
            .child(
                div()
                    .min_w(px(content_width))
                    .whitespace_nowrap()
                    .line_height(px(18.0))
                    .child(self.render_diff_code(id, block, reference, cx)),
            );
        // Keep vertical wheel input on the virtualized diff list instead of converting it
        // into horizontal movement for this nested x-only scroll area.
        code_scroll.style().restrict_scroll_to_axis = Some(true);
        div()
            .w_full()
            .min_w_0()
            .flex()
            .items_start()
            .child(gutter)
            .child(code_padding)
            .child(div().relative().flex_1().min_w_0().child(code_scroll).when(
                show_scrollbar,
                |element| {
                    element.child(self.render_thin_horizontal_scrollbar(scrollbar_id, scroll))
                },
            ))
            .into_any_element()
    }

    fn render_diff_file_header(&self, file_index: usize, last_in_file: bool) -> AnyElement {
        let Some(file) = self
            .diff_display
            .as_ref()
            .and_then(|turn| turn.files.get(file_index))
        else {
            return div().into_any_element();
        };
        div()
            .w_full()
            .when(last_in_file, |element| {
                element.border_b_1().border_color(rgb(border()))
            })
            .child(
                div()
                    .h(px(34.0))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .bg(rgb(surface()))
                    .text_xs()
                    .child(
                        div()
                            .w(px(14.0))
                            .text_color(rgb(match file.kind {
                                FileDiffKind::Added => green(),
                                FileDiffKind::Deleted => red(),
                                _ => orange(),
                            }))
                            .child(file_kind_label(file.kind)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(rgb(theme_text()))
                            .child(file.path.clone()),
                    )
                    .child(render_diff_stats(file.additions, file.deletions)),
            )
            .when_some(file.message.clone(), |element, message| {
                element.child(
                    div()
                        .px_3()
                        .py_3()
                        .text_xs()
                        .text_color(rgb(muted()))
                        .child(message),
                )
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_unified_diff_chunk(
        &self,
        file_index: usize,
        hunk_index: usize,
        chunk_index: usize,
        block: &DiffBlock,
        reference: &DiffSelectionReference,
        top_gap: bool,
        last_in_file: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prepared = &self.diff_render_cache.files[file_index];
        let scroll =
            self.diff_code_scroll(&format!("diff-{}-{file_index}-unified", reference.turn_id));
        div()
            .w_full()
            .overflow_hidden()
            .when(top_gap, |element| element.mt_2())
            .when(last_in_file, |element| {
                element.border_b_1().border_color(rgb(border()))
            })
            .child(self.render_diff_block(
                format!(
                    "diff-{}-{file_index}-{hunk_index}-{chunk_index}-unified",
                    reference.turn_id
                ),
                block,
                94.0,
                prepared.content_width,
                reference,
                &scroll,
                last_in_file,
                cx,
            ))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_split_diff_chunk(
        &self,
        file_index: usize,
        hunk_index: usize,
        chunk_index: usize,
        old_block: &DiffBlock,
        new_block: &DiffBlock,
        old_reference: &DiffSelectionReference,
        new_reference: &DiffSelectionReference,
        top_gap: bool,
        last_in_file: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prepared = &self.diff_render_cache.files[file_index];
        let scroll = self.diff_code_scroll(&format!(
            "diff-{}-{file_index}-split",
            new_reference.turn_id
        ));
        div()
            .w_full()
            .min_w_0()
            .flex()
            .overflow_hidden()
            .when(top_gap, |element| element.mt_2())
            .when(last_in_file, |element| {
                element.border_b_1().border_color(rgb(border()))
            })
            .child(
                div()
                    .w_1_2()
                    .min_w_0()
                    .overflow_hidden()
                    .border_r_1()
                    .border_color(rgb(border()))
                    .child(self.render_diff_block(
                        format!(
                            "diff-{}-{file_index}-{hunk_index}-{chunk_index}-old",
                            old_reference.turn_id
                        ),
                        old_block,
                        48.0,
                        prepared.content_width,
                        old_reference,
                        &scroll,
                        false,
                        cx,
                    )),
            )
            .child(
                div()
                    .w_1_2()
                    .min_w_0()
                    .overflow_hidden()
                    .child(self.render_diff_block(
                        format!(
                            "diff-{}-{file_index}-{hunk_index}-{chunk_index}-new",
                            new_reference.turn_id
                        ),
                        new_block,
                        48.0,
                        prepared.content_width,
                        new_reference,
                        &scroll,
                        last_in_file,
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_diff_list_item(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(item) = self.diff_render_cache.items.get(index) else {
            return div().into_any_element();
        };
        match item {
            DiffRenderItem::FileHeader {
                file_index,
                last_in_file,
            } => self.render_diff_file_header(*file_index, *last_in_file),
            DiffRenderItem::UnifiedChunk {
                file_index,
                hunk_index,
                chunk_index,
                block,
                reference,
                top_gap,
                last_in_file,
            } => self.render_unified_diff_chunk(
                *file_index,
                *hunk_index,
                *chunk_index,
                block,
                reference,
                *top_gap,
                *last_in_file,
                cx,
            ),
            DiffRenderItem::SplitChunk {
                file_index,
                hunk_index,
                chunk_index,
                old_block,
                new_block,
                old_reference,
                new_reference,
                top_gap,
                last_in_file,
            } => self.render_split_diff_chunk(
                *file_index,
                *hunk_index,
                *chunk_index,
                old_block,
                new_block,
                old_reference,
                new_reference,
                *top_gap,
                *last_in_file,
                cx,
            ),
        }
    }

    pub(crate) fn rebuild_diff_render_cache(&mut self) {
        self.diff_render_cache = self
            .diff_display
            .as_ref()
            .map(|turn| DiffRenderCache::build(turn, self.diff_view_mode))
            .unwrap_or_default();
        self.diff_list.reset_with_uniform_height(
            self.diff_render_cache.items.len(),
            px(self.diff_render_cache.item_height_hint()),
        );
    }

    pub(crate) fn diff_sidebar_replaces_thread(&self, window: &Window) -> bool {
        self.diff_sidebar_open
            && self.selected_harness.is_some()
            && diff_sidebar_replaces_thread(
                window.viewport_size().width.as_f32(),
                self.sidebar_width,
                self.diff_sidebar_width,
            )
    }

    pub(crate) fn render_diff_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.diff_sidebar_open || self.selected_harness.is_none() {
            return div().into_any_element();
        }
        let replaces_thread = self.diff_sidebar_replaces_thread(window);
        self.sync_diff_display();
        let harness = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
            .expect("selected harness exists");
        let selected_turn_id = self.selected_diff_turn_id();
        let display_empty = self
            .diff_display
            .as_ref()
            .is_none_or(|turn| turn.files.is_empty());
        let empty_message = self.diff_display.as_ref().and_then(|turn| {
            display_empty.then(|| {
                turn.error.clone().unwrap_or_else(|| match self.diff_scope {
                    DiffScope::Cumulative => "No net file changes through this turn.".into(),
                    DiffScope::Turn => "No file changes in this turn.".into(),
                })
            })
        });
        let combined_stats = self
            .diff_display
            .as_ref()
            .map(|turn| (turn.additions, turn.deletions));
        let no_turns = harness.turn_diffs.is_empty() && harness.active_turn_preview.is_none();
        let turn_picker = self.render_diff_turn_picker(
            &harness.turn_diffs,
            harness.active_turn_preview.as_ref(),
            selected_turn_id,
            cx,
        );
        let resize_drag = DiffSidebarResizeDrag {
            width: self.diff_sidebar_width,
            mouse_x: window.mouse_position().x,
        };
        let viewport_width = window.viewport_size().width.as_f32();
        let rendered_width = if replaces_thread {
            (viewport_width - self.sidebar_width).max(0.0)
        } else {
            self.diff_sidebar_width
        };
        let minimum_width = if replaces_thread {
            0.0
        } else {
            MIN_DIFF_SIDEBAR_WIDTH
        };
        let maximum_width = if replaces_thread {
            rendered_width
        } else {
            viewport_width * 0.85
        };
        let has_diff_selection = self
            .thread_text_selection
            .as_ref()
            .is_some_and(|selection| {
                !selection.range.is_empty() && selection.diff_reference.is_some()
            });

        div()
            .relative()
            .w(px(rendered_width))
            .min_w(px(minimum_width))
            .max_w(px(maximum_width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(border()))
            .bg(rgb(bg()))
            .child(
                div()
                    .h(px(36.0))
                    .px_1()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1()
                    .border_b_1()
                    .border_color(rgb(border()))
                    .child(turn_picker)
                    .child(self.render_diff_scope_toggle(cx))
                    .child(self.render_diff_mode_toggle(cx))
                    .child(div().flex_1())
                    .when_some(combined_stats, |element, (additions, deletions)| {
                        element.child(render_diff_stats(additions, deletions))
                    })
                    .child(
                        div()
                            .id("close-diff-sidebar")
                            .size(px(28.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(muted()))
                            .hover(|style| style.text_color(rgb(theme_text())))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_diff_sidebar();
                                cx.notify();
                            }))
                            .child(
                                svg()
                                    .path("icon/panel-right.svg")
                                    .size(px(18.0))
                                    .text_color(rgb(theme_text())),
                            ),
                    ),
            )
            .child(
                div()
                    .id("diff-content-list")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .whitespace_nowrap()
                    .text_xs()
                    .text_color(rgb(code_text()))
                    .when(!display_empty, |element| {
                        element.child(
                            list(
                                self.diff_list.clone(),
                                cx.processor(Self::render_diff_list_item),
                            )
                            .size_full(),
                        )
                    })
                    .when(display_empty, |element| {
                        let message = empty_message.unwrap_or_else(|| {
                            if no_turns {
                                "Completed turn diffs will appear here.".into()
                            } else {
                                "No file changes to display.".into()
                            }
                        });
                        element.child(
                            div()
                                .absolute()
                                .inset_0()
                                .p_6()
                                .text_center()
                                .whitespace_normal()
                                .text_color(rgb(muted()))
                                .child(message),
                        )
                    })
                    .child(self.render_diff_list_scrollbar()),
            )
            .when(has_diff_selection, |element| {
                element.child(
                    div()
                        .h(px(42.0))
                        .px_3()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_2()
                        .border_t_1()
                        .border_color(rgb(border()))
                        .bg(rgb(surface()))
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .text_color(rgb(muted()))
                                .child("Diff text selected"),
                        )
                        .child(
                            div()
                                .id("add-diff-reference")
                                .h(px(28.0))
                                .px_3()
                                .flex()
                                .items_center()
                                .rounded_md()
                                .bg(rgb(blue()))
                                .text_xs()
                                .text_color(rgb(bg()))
                                .hover(|style| style.bg(rgb(crate::theme::accent_hover())))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.add_diff_selection_to_composer(cx);
                                    cx.stop_propagation();
                                }))
                                .child("Add reference to composer"),
                        ),
                )
            })
            .when(!replaces_thread, |element| {
                element.child(
                    div()
                        .id("diff-sidebar-resize-handle")
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(-3.0))
                        .w(px(7.0))
                        .cursor(CursorStyle::ResizeColumn)
                        .hover(|style| style.bg(rgb(blue())))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        )
                        .on_drag(resize_drag, |_, _, _, cx| {
                            cx.new(|_| DiffSidebarResizePreview)
                        })
                        .on_drag_move::<DiffSidebarResizeDrag>(cx.listener(
                            |this, event: &DragMoveEvent<DiffSidebarResizeDrag>, _, cx| {
                                let drag = event.drag(cx);
                                let delta = drag.mouse_x - event.event.position.x;
                                this.resize_diff_sidebar(drag.width + delta.as_f32());
                                cx.notify();
                            },
                        )),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffRow, FileDiffKind};

    #[test]
    fn narrow_windows_replace_the_thread_with_the_diff() {
        assert!(diff_sidebar_replaces_thread(1_267.0, 288.0, 560.0));
        assert!(!diff_sidebar_replaces_thread(1_268.0, 288.0, 560.0));
    }

    fn replaced_file_and_hunk() -> (FileDiff, DiffHunk) {
        let hunk = DiffHunk {
            old_start: 1,
            old_len: 2,
            new_start: 1,
            new_len: 2,
            rows: vec![
                DiffRow {
                    old_number: Some(1),
                    new_number: Some(1),
                    old_text: Some("old one".into()),
                    new_text: Some("new one".into()),
                    kind: DiffRowKind::Replaced,
                },
                DiffRow {
                    old_number: Some(2),
                    new_number: Some(2),
                    old_text: Some("old two".into()),
                    new_text: Some("new two".into()),
                    kind: DiffRowKind::Replaced,
                },
            ],
        };
        let file = FileDiff {
            path: "src/main.rs".into(),
            old_path: None,
            kind: FileDiffKind::Modified,
            old_text: Some("old one\nold two\n".into()),
            new_text: Some("new one\nnew two\n".into()),
            old_mode: 0,
            new_mode: 0,
            old_exists: Some(true),
            new_exists: Some(true),
            hunks: vec![hunk.clone()],
            additions: 2,
            deletions: 2,
            message: None,
            old_highlights: Vec::new(),
            new_highlights: Vec::new(),
        };
        (file, hunk)
    }

    #[test]
    fn unified_replacements_group_removals_before_additions() {
        let (file, hunk) = replaced_file_and_hunk();
        let block = unified_hunk_block(
            &file,
            &hunk,
            &line_offsets(file.old_text.as_deref()),
            &line_offsets(file.new_text.as_deref()),
        );

        assert_eq!(block.text, "old one\nold two\nnew one\nnew two");
        assert_eq!(
            block
                .gutter
                .iter()
                .map(|line| line.label.as_ref())
                .collect::<Vec<_>>(),
            [
                "    1       -",
                "    2       -",
                "          1 +",
                "          2 +"
            ]
        );
    }

    #[test]
    fn gutter_columns_keep_context_and_changed_rows_aligned() {
        let block = DiffBlock::new(
            "".into(),
            Vec::new(),
            vec![
                DiffGutterLine {
                    label: DiffGutterContent::Unified {
                        sign: None,
                        old_number: Some(390),
                        new_number: Some(450),
                    }
                    .label(),
                    color: border(),
                    background: None,
                },
                DiffGutterLine {
                    label: DiffGutterContent::Unified {
                        sign: Some('+'),
                        old_number: None,
                        new_number: Some(451),
                    }
                    .label(),
                    color: green(),
                    background: Some(green()),
                },
            ],
        );
        let columns = block.gutter_columns.as_ref().unwrap();

        assert_eq!(columns.old.text.as_ref(), "390\n\u{00a0}");
        assert_eq!(columns.new.text.as_ref(), "450\n451");
        assert_eq!(columns.sign.text.as_ref(), "\u{00a0}\n+");
        assert_eq!(block.code_padding.text.as_ref(), "\u{00a0}\n\u{00a0}");
        assert_eq!(block.code_padding.highlights.len(), 1);
        assert_eq!(block.code_padding.highlights[0].0, 3..5);
        assert!(
            block.code_padding.highlights[0]
                .1
                .background_color
                .is_some()
        );
    }

    #[test]
    fn changed_line_highlights_are_ordered_and_non_overlapping() {
        let (mut file, hunk) = replaced_file_and_hunk();
        file.old_highlights = vec![SyntaxSpan {
            range: 0..3,
            color: 0xff0000,
        }];
        file.new_highlights = vec![SyntaxSpan {
            range: 0..3,
            color: 0x00ff00,
        }];
        let block = unified_hunk_block(
            &file,
            &hunk,
            &line_offsets(file.old_text.as_deref()),
            &line_offsets(file.new_text.as_deref()),
        );

        assert!(
            block
                .highlights
                .windows(2)
                .all(|pair| pair[0].0.end <= pair[1].0.start)
        );
    }

    #[test]
    fn split_blocks_keep_placeholder_rows_aligned() {
        let (file, hunk) = replaced_file_and_hunk();
        let old = split_hunk_block(&file, &hunk, true, &line_offsets(file.old_text.as_deref()));
        let new = split_hunk_block(&file, &hunk, false, &line_offsets(file.new_text.as_deref()));

        assert_eq!(old.text.lines().count(), new.text.lines().count());
        assert_eq!(old.gutter.len(), new.gutter.len());
    }

    #[test]
    fn render_cache_splits_large_hunks_into_virtualized_chunks() {
        let line_count = DIFF_CHUNK_LINES * 2 + 7;
        let text = (1..=line_count)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let hunk = DiffHunk {
            old_start: 1,
            old_len: line_count as u32,
            new_start: 1,
            new_len: line_count as u32,
            rows: (1..=line_count)
                .map(|line| DiffRow {
                    old_number: Some(line as u32),
                    new_number: Some(line as u32),
                    old_text: Some(format!("line {line}")),
                    new_text: Some(format!("line {line}")),
                    kind: DiffRowKind::Context,
                })
                .collect(),
        };
        let file = FileDiff {
            path: "src/large.rs".into(),
            old_path: None,
            kind: FileDiffKind::Modified,
            old_text: Some(text.clone()),
            new_text: Some(text),
            old_mode: 0,
            new_mode: 0,
            old_exists: Some(true),
            new_exists: Some(true),
            hunks: vec![hunk],
            additions: 0,
            deletions: 0,
            message: None,
            old_highlights: Vec::new(),
            new_highlights: Vec::new(),
        };
        let turn = TurnDiff {
            id: 1,
            prompt: String::new(),
            started_at: 0,
            finished_at: 0,
            status: TurnDiffStatus::Completed,
            files: vec![file],
            additions: 0,
            deletions: 0,
            error: None,
        };

        let cache = DiffRenderCache::build(&turn, DiffViewMode::Unified);
        let chunk_line_counts = cache
            .items
            .iter()
            .filter_map(|item| match item {
                DiffRenderItem::UnifiedChunk { block, .. } => Some(block.line_count()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(cache.items.len(), 4); // One header and three independently rendered chunks.
        assert_eq!(chunk_line_counts, [DIFF_CHUNK_LINES, DIFF_CHUNK_LINES, 7]);
    }
}
