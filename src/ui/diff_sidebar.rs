use std::ops::Range;

use gpui::{
    AnyElement, Context, CursorStyle, DragMoveEvent, HighlightStyle, IntoElement, Pixels,
    ScrollHandle, SharedString, Window, deferred, div, list, prelude::*, px, svg,
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

fn line_offsets(text: Option<&str>) -> Vec<usize> {
    let Some(text) = text else {
        return Vec::new();
    };
    let mut offsets = vec![0];
    for (index, character) in text.char_indices() {
        if character == '\n' && index + 1 < text.len() {
            offsets.push(index + 1);
        }
    }
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

struct DiffGutterLine {
    content: DiffGutterContent,
    color: u32,
}

struct DiffBlock {
    text: String,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    gutter: Vec<DiffGutterLine>,
}

impl DiffBlock {
    fn new() -> Self {
        Self {
            text: String::new(),
            highlights: Vec::new(),
            gutter: Vec::new(),
        }
    }

    fn finish(mut self) -> Self {
        if self.text.ends_with('\n') {
            self.text.pop();
            let text_len = self.text.len();
            for (range, _) in &mut self.highlights {
                range.end = range.end.min(text_len);
            }
            self.highlights.retain(|(range, _)| !range.is_empty());
        }
        self
    }
}

fn append_syntax(
    block: &mut DiffBlock,
    text: &str,
    line_number: Option<u32>,
    source_offsets: &[usize],
    source_spans: &[SyntaxSpan],
    changed_color: Option<u32>,
) {
    let line_start = block.text.len();
    block.text.push_str(text);
    block.text.push('\n');
    let changed_background = changed_color.map(|color| rgb(color).opacity(0.10).into());
    if let Some(line_number) = line_number
        && let Some(&file_start) = source_offsets.get(line_number.saturating_sub(1) as usize)
    {
        let file_end = file_start + text.len();
        for span in source_spans {
            let start = span.range.start.max(file_start);
            let end = span.range.end.min(file_end);
            if start < end {
                block.highlights.push((
                    line_start + start - file_start..line_start + end - file_start,
                    HighlightStyle {
                        color: Some(rgb(span.color).into()),
                        background_color: changed_background,
                        ..Default::default()
                    },
                ));
            }
        }
    }
    if changed_background.is_some() {
        block.highlights.push((
            line_start..block.text.len(),
            HighlightStyle {
                background_color: changed_background,
                ..Default::default()
            },
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn append_diff_line(
    block: &mut DiffBlock,
    gutter: DiffGutterContent,
    text: &str,
    line_number: Option<u32>,
    source_offsets: &[usize],
    source_spans: &[SyntaxSpan],
    changed_color: Option<u32>,
) {
    block.gutter.push(DiffGutterLine {
        content: gutter,
        color: changed_color.unwrap_or_else(border),
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

fn unified_hunk_block(file: &FileDiff, hunk: &DiffHunk) -> DiffBlock {
    let mut block = DiffBlock::new();
    let old_offsets = line_offsets(file.old_text.as_deref());
    let new_offsets = line_offsets(file.new_text.as_deref());
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
                &new_offsets,
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
                    &old_offsets,
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
                    &new_offsets,
                    &file.new_highlights,
                    Some(green()),
                );
            }
        }
    }
    block.finish()
}

fn split_hunk_block(file: &FileDiff, hunk: &DiffHunk, old_side: bool) -> DiffBlock {
    let mut block = DiffBlock::new();
    let offsets = line_offsets(if old_side {
        file.old_text.as_deref()
    } else {
        file.new_text.as_deref()
    });
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
            &offsets,
            spans,
            changed_color,
        );
    }
    block.finish()
}

impl Dirigent {
    pub(super) fn render_changes_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.diff_sidebar_open {
            return div().into_any_element();
        }
        div()
            .id("changes-toggle")
            .absolute()
            .top(px(8.0))
            .right(px(8.0))
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
        text: String,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
        reference: DiffSelectionReference,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_styled_selectable_text_with_reference(
            id,
            SharedString::from(text),
            &highlights,
            &[],
            &[],
            false,
            Some(reference),
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
        block: DiffBlock,
        gutter_width: f32,
        content_width: f32,
        reference: DiffSelectionReference,
        scroll: ScrollHandle,
        show_scrollbar: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let scrollbar_id = format!("{id}-scrollbar");
        let scroll_id = format!("{id}-scroll");
        let DiffBlock {
            text,
            highlights,
            gutter,
        } = block;
        let gap_colors = gutter.iter().map(|line| line.color).collect::<Vec<_>>();
        let mut code_scroll = div()
            .id(scroll_id)
            .w_full()
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(&scroll)
            .child(
                div()
                    .min_w(px(content_width))
                    .whitespace_nowrap()
                    .line_height(px(18.0))
                    .child(self.render_diff_code(id, text, highlights, reference, cx)),
            );
        // Keep vertical wheel input on the virtualized diff list instead of converting it
        // into horizontal movement for this nested x-only scroll area.
        code_scroll.style().restrict_scroll_to_axis = Some(true);
        div()
            .w_full()
            .min_w_0()
            .flex()
            .items_start()
            .child(
                div()
                    .w(px(gutter_width))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .text_xs()
                    .children(gutter.into_iter().map(|line| {
                        let content = match line.content {
                            DiffGutterContent::Unified {
                                sign,
                                old_number,
                                new_number,
                            } => div()
                                .size_full()
                                .flex()
                                .items_center()
                                .child(
                                    div().min_w_0().flex_1().flex().justify_end().px_1().child(
                                        old_number
                                            .map_or_else(String::new, |number| number.to_string()),
                                    ),
                                )
                                .child(
                                    div().min_w_0().flex_1().flex().justify_end().px_1().child(
                                        new_number
                                            .map_or_else(String::new, |number| number.to_string()),
                                    ),
                                )
                                .child(
                                    div().w(px(14.0)).flex_none().flex().justify_center().child(
                                        sign.map_or_else(String::new, |sign| sign.to_string()),
                                    ),
                                ),
                            DiffGutterContent::Single(number) => div()
                                .size_full()
                                .pr_2()
                                .flex()
                                .items_center()
                                .justify_end()
                                .child(
                                    number.map_or_else(String::new, |number| number.to_string()),
                                ),
                        };
                        div()
                            .relative()
                            .h(px(18.0))
                            .flex_none()
                            .border_r_4()
                            .border_color(rgb(line.color))
                            .text_color(rgb(if line.color == border() {
                                muted()
                            } else {
                                line.color
                            }))
                            .child(content)
                    })),
            )
            .child(div().w(px(4.0)).flex_none().flex().flex_col().children(
                gap_colors.into_iter().map(|color| {
                    div()
                        .h(px(18.0))
                        .flex_none()
                        .when(color != border(), |element| {
                            element.bg(rgb(color).opacity(0.10))
                        })
                }),
            ))
            .child(div().relative().flex_1().min_w_0().child(code_scroll).when(
                show_scrollbar,
                |element| {
                    element.child(self.render_thin_horizontal_scrollbar(scrollbar_id, &scroll))
                },
            ))
            .into_any_element()
    }

    fn render_diff_file(
        &self,
        turn_id: u64,
        file_index: usize,
        file: &FileDiff,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let language = language_for_path(&file.path);
        let mode_label = match self.diff_view_mode {
            DiffViewMode::Unified => "unified",
            DiffViewMode::Split => "split",
        };
        let file_scroll =
            self.diff_code_scroll(&format!("diff-{turn_id}-{file_index}-{mode_label}"));
        let content_width = Self::diff_file_code_width(file);
        let last_hunk_index = file.hunks.len().saturating_sub(1);
        div()
            .w_full()
            .border_b_1()
            .border_color(rgb(border()))
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
                    .child(
                        div()
                            .text_color(rgb(muted()))
                            .child(format!("+{} −{}", file.additions, file.deletions)),
                    ),
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
            .children(file.hunks.iter().enumerate().map(|(hunk_index, hunk)| {
                div()
                    .w_full()
                    .overflow_hidden()
                    .when(hunk_index > 0, |element| element.mt_2())
                    .child(match self.diff_view_mode {
                        DiffViewMode::Unified => {
                            let id = format!("diff-{turn_id}-{file_index}-{hunk_index}-unified");
                            let block = unified_hunk_block(file, hunk);
                            self.render_diff_block(
                                id,
                                block,
                                94.0,
                                content_width,
                                DiffSelectionReference {
                                    turn_id,
                                    path: file.path.clone(),
                                    side: "unified",
                                    lines: format!(
                                        "-{},{} +{},{}",
                                        hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
                                    ),
                                    language: language.clone(),
                                },
                                file_scroll.clone(),
                                hunk_index == last_hunk_index,
                                cx,
                            )
                        }
                        DiffViewMode::Split => {
                            let old_id = format!("diff-{turn_id}-{file_index}-{hunk_index}-old");
                            let new_id = format!("diff-{turn_id}-{file_index}-{hunk_index}-new");
                            let old_block = split_hunk_block(file, hunk, true);
                            let new_block = split_hunk_block(file, hunk, false);
                            div()
                                .w_full()
                                .min_w_0()
                                .flex()
                                .overflow_hidden()
                                .child(
                                    div()
                                        .w_1_2()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .border_r_1()
                                        .border_color(rgb(border()))
                                        .child(
                                            self.render_diff_block(
                                                old_id,
                                                old_block,
                                                48.0,
                                                content_width,
                                                DiffSelectionReference {
                                                    turn_id,
                                                    path: file
                                                        .old_path
                                                        .clone()
                                                        .unwrap_or_else(|| file.path.clone()),
                                                    side: "old",
                                                    lines: format!(
                                                        "{}–{}",
                                                        hunk.old_start,
                                                        hunk.old_start
                                                            + hunk.old_len.saturating_sub(1)
                                                    ),
                                                    language: language.clone(),
                                                },
                                                file_scroll.clone(),
                                                false,
                                                cx,
                                            ),
                                        ),
                                )
                                .child(div().w_1_2().min_w_0().overflow_hidden().child(
                                    self.render_diff_block(
                                        new_id,
                                        new_block,
                                        48.0,
                                        content_width,
                                        DiffSelectionReference {
                                            turn_id,
                                            path: file.path.clone(),
                                            side: "new",
                                            lines: format!(
                                                "{}–{}",
                                                hunk.new_start,
                                                hunk.new_start + hunk.new_len.saturating_sub(1)
                                            ),
                                            language: language.clone(),
                                        },
                                        file_scroll.clone(),
                                        hunk_index == last_hunk_index,
                                        cx,
                                    ),
                                ))
                                .into_any_element()
                        }
                    })
                    .into_any_element()
            }))
            .into_any_element()
    }

    fn render_diff_file_item(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(turn) = self.diff_display.as_ref() else {
            return div().into_any_element();
        };
        let Some(file) = turn.files.get(index) else {
            return div().into_any_element();
        };
        self.render_diff_file(turn.id, index, file, cx)
    }

    pub(crate) fn render_diff_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.diff_sidebar_open || self.selected_harness.is_none() {
            return div().into_any_element();
        }
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
        let overlay = window.viewport_size().width.as_f32()
            < self.sidebar_width + 420.0 + self.diff_sidebar_width;
        let has_diff_selection = self
            .thread_text_selection
            .as_ref()
            .is_some_and(|selection| {
                !selection.range.is_empty() && selection.diff_reference.is_some()
            });

        div()
            .relative()
            .w(px(self.diff_sidebar_width))
            .min_w(px(420.0))
            .max_w(window.viewport_size().width * 0.85)
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(border()))
            .bg(rgb(bg()))
            .when(overlay, |element| {
                element.absolute().top_0().right_0().shadow_lg()
            })
            .child(
                div()
                    .h(px(44.0))
                    .px_2()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(border()))
                    .child(turn_picker)
                    .child(self.render_diff_scope_toggle(cx))
                    .child(self.render_diff_mode_toggle(cx))
                    .child(div().flex_1())
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
                                cx.processor(Self::render_diff_file_item),
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
            .child(
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
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffRow, FileDiffKind};

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
        let block = unified_hunk_block(&file, &hunk);

        assert_eq!(block.text, "old one\nold two\nnew one\nnew two");
        assert_eq!(
            block
                .gutter
                .iter()
                .map(|line| line.content)
                .collect::<Vec<_>>(),
            [
                DiffGutterContent::Unified {
                    sign: Some('-'),
                    old_number: Some(1),
                    new_number: None,
                },
                DiffGutterContent::Unified {
                    sign: Some('-'),
                    old_number: Some(2),
                    new_number: None,
                },
                DiffGutterContent::Unified {
                    sign: Some('+'),
                    old_number: None,
                    new_number: Some(1),
                },
                DiffGutterContent::Unified {
                    sign: Some('+'),
                    old_number: None,
                    new_number: Some(2),
                },
            ]
        );
    }

    #[test]
    fn split_blocks_keep_placeholder_rows_aligned() {
        let (file, hunk) = replaced_file_and_hunk();
        let old = split_hunk_block(&file, &hunk, true);
        let new = split_hunk_block(&file, &hunk, false);

        assert_eq!(old.text.lines().count(), new.text.lines().count());
        assert_eq!(old.gutter.len(), new.gutter.len());
    }
}
