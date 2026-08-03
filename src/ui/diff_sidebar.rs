use std::ops::Range;

use gpui::{
    AnyElement, Context, CursorStyle, DragMoveEvent, HighlightStyle, IntoElement, Pixels,
    SharedString, Window, div, prelude::*, px,
};

use crate::{
    app::Dirigent,
    diff::{
        DiffHunk, DiffRowKind, DiffSelectionReference, DiffViewMode, FileDiff, FileDiffKind,
        SyntaxSpan, TurnDiff, TurnDiffStatus,
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

#[allow(clippy::too_many_arguments)]
fn append_syntax(
    output: &mut String,
    highlights: &mut Vec<(Range<usize>, HighlightStyle)>,
    prefix: &str,
    text: &str,
    line_number: Option<u32>,
    source_offsets: &[usize],
    source_spans: &[SyntaxSpan],
    marker_color: Option<u32>,
) {
    let line_start = output.len();
    output.push_str(prefix);
    let source_start = output.len();
    output.push_str(text);
    output.push('\n');
    let changed_background = marker_color.map(|color| rgb(color).opacity(0.10).into());
    if let Some(color) = marker_color {
        highlights.push((
            line_start..source_start,
            HighlightStyle {
                color: Some(rgb(color).into()),
                background_color: changed_background,
                ..Default::default()
            },
        ));
    }
    let Some(line_number) = line_number else {
        return;
    };
    let Some(&file_start) = source_offsets.get(line_number.saturating_sub(1) as usize) else {
        return;
    };
    let file_end = file_start + text.len();
    for span in source_spans {
        let start = span.range.start.max(file_start);
        let end = span.range.end.min(file_end);
        if start < end {
            highlights.push((
                source_start + start - file_start..source_start + end - file_start,
                HighlightStyle {
                    color: Some(rgb(span.color).into()),
                    background_color: changed_background,
                    ..Default::default()
                },
            ));
        }
    }
    if changed_background.is_some() {
        highlights.push((
            line_start..output.len(),
            HighlightStyle {
                background_color: changed_background,
                ..Default::default()
            },
        ));
    }
}

fn unified_hunk_text(
    file: &FileDiff,
    hunk: &DiffHunk,
) -> (String, Vec<(Range<usize>, HighlightStyle)>) {
    let mut output = String::new();
    let mut highlights = Vec::new();
    let old_offsets = line_offsets(file.old_text.as_deref());
    let new_offsets = line_offsets(file.new_text.as_deref());
    for row in &hunk.rows {
        match row.kind {
            DiffRowKind::Context => {
                let prefix = format!(
                    "  {:>4} {:>4} │ ",
                    row.old_number.unwrap_or(0),
                    row.new_number.unwrap_or(0)
                );
                append_syntax(
                    &mut output,
                    &mut highlights,
                    &prefix,
                    row.new_text.as_deref().unwrap_or_default(),
                    row.new_number,
                    &new_offsets,
                    &file.new_highlights,
                    None,
                );
            }
            DiffRowKind::Removed => {
                let prefix = format!("- {:>4}      │ ", row.old_number.unwrap_or(0));
                append_syntax(
                    &mut output,
                    &mut highlights,
                    &prefix,
                    row.old_text.as_deref().unwrap_or_default(),
                    row.old_number,
                    &old_offsets,
                    &file.old_highlights,
                    Some(red()),
                );
            }
            DiffRowKind::Added => {
                let prefix = format!("+      {:>4} │ ", row.new_number.unwrap_or(0));
                append_syntax(
                    &mut output,
                    &mut highlights,
                    &prefix,
                    row.new_text.as_deref().unwrap_or_default(),
                    row.new_number,
                    &new_offsets,
                    &file.new_highlights,
                    Some(green()),
                );
            }
            DiffRowKind::Replaced => {
                if let Some(text) = row.old_text.as_deref() {
                    let prefix = format!("- {:>4}      │ ", row.old_number.unwrap_or(0));
                    append_syntax(
                        &mut output,
                        &mut highlights,
                        &prefix,
                        text,
                        row.old_number,
                        &old_offsets,
                        &file.old_highlights,
                        Some(red()),
                    );
                }
                if let Some(text) = row.new_text.as_deref() {
                    let prefix = format!("+      {:>4} │ ", row.new_number.unwrap_or(0));
                    append_syntax(
                        &mut output,
                        &mut highlights,
                        &prefix,
                        text,
                        row.new_number,
                        &new_offsets,
                        &file.new_highlights,
                        Some(green()),
                    );
                }
            }
        }
    }
    (output, highlights)
}

fn split_hunk_text(
    file: &FileDiff,
    hunk: &DiffHunk,
    old_side: bool,
) -> (String, Vec<(Range<usize>, HighlightStyle)>) {
    let mut output = String::new();
    let mut highlights = Vec::new();
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
        let prefix = number.map_or_else(
            || "       │ ".to_string(),
            |number| format!("{:>6} │ ", number),
        );
        append_syntax(
            &mut output,
            &mut highlights,
            &prefix,
            text.unwrap_or_default(),
            number,
            &offsets,
            spans,
            changed.then_some(if old_side { red() } else { green() }),
        );
    }
    (output, highlights)
}

impl Dirigent {
    pub(super) fn render_changes_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let count = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
            .map_or(0, |harness| harness.turn_diffs.len());
        div()
            .id("changes-toggle")
            .absolute()
            .top(px(10.0))
            .right(px(14.0))
            .h(px(28.0))
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(border()))
            .bg(rgb(bg()).opacity(0.92))
            .text_xs()
            .text_color(rgb(if self.diff_sidebar_open {
                blue()
            } else {
                muted()
            }))
            .hover(|style| style.bg(rgb(surface_hover())).text_color(rgb(theme_text())))
            .on_click(cx.listener(|this, _, _, cx| {
                this.toggle_diff_sidebar();
                cx.notify();
            }))
            .child("Changes")
            .when(count > 0, |element| element.child(count.to_string()))
            .into_any_element()
    }

    fn render_diff_mode_button(
        &self,
        label: &'static str,
        mode: DiffViewMode,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.diff_view_mode == mode;
        div()
            .id(format!("diff-mode-{label}"))
            .h(px(24.0))
            .px_2()
            .flex()
            .items_center()
            .rounded_md()
            .text_xs()
            .text_color(rgb(if selected { theme_text() } else { muted() }))
            .when(selected, |element| element.bg(rgb(surface_hover())))
            .hover(|style| style.bg(rgb(surface_hover())))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_diff_view_mode(mode);
                cx.notify();
            }))
            .child(label)
            .into_any_element()
    }

    fn render_diff_turn_row(
        &self,
        turn: &TurnDiff,
        selected: bool,
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
                    .child(div().text_xs().text_color(rgb(muted())).child(format!(
                        "{} · {} files · +{} −{}",
                        status_label(turn.status),
                        turn.files.len(),
                        turn.additions,
                        turn.deletions
                    ))),
            )
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

    fn render_diff_file(
        &self,
        turn_id: u64,
        file_index: usize,
        file: &FileDiff,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let language = language_for_path(&file.path);
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
                let header = format!(
                    "@@ -{},{} +{},{} @@",
                    hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
                );
                div()
                    .w_full()
                    .child(
                        div()
                            .h(px(26.0))
                            .px_3()
                            .flex()
                            .items_center()
                            .bg(rgb(blue()).opacity(0.08))
                            .text_xs()
                            .text_color(rgb(blue()))
                            .child(header),
                    )
                    .child(match self.diff_view_mode {
                        DiffViewMode::Unified => {
                            let (text, highlights) = unified_hunk_text(file, hunk);
                            self.render_diff_code(
                                format!("diff-{turn_id}-{file_index}-{hunk_index}-unified"),
                                text,
                                highlights,
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
                                cx,
                            )
                        }
                        DiffViewMode::Split => {
                            let (old_text, old_highlights) = split_hunk_text(file, hunk, true);
                            let (new_text, new_highlights) = split_hunk_text(file, hunk, false);
                            div()
                                .w_full()
                                .flex()
                                .child(
                                    div()
                                        .w_1_2()
                                        .min_w_0()
                                        .pr_2()
                                        .border_r_1()
                                        .border_color(rgb(border()))
                                        .child(
                                            self.render_diff_code(
                                                format!(
                                                    "diff-{turn_id}-{file_index}-{hunk_index}-old"
                                                ),
                                                old_text,
                                                old_highlights,
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
                                                cx,
                                            ),
                                        ),
                                )
                                .child(div().w_1_2().min_w_0().pl_2().child(self.render_diff_code(
                                    format!("diff-{turn_id}-{file_index}-{hunk_index}-new"),
                                    new_text,
                                    new_highlights,
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
                                    cx,
                                )))
                                .into_any_element()
                        }
                    })
                    .into_any_element()
            }))
            .into_any_element()
    }

    pub(crate) fn render_diff_sidebar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.diff_sidebar_open || self.selected_harness.is_none() {
            return div().into_any_element();
        }
        let harness = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
            .expect("selected harness exists");
        let selected_turn_id = self.selected_diff_turn_id();
        let selected_turn = selected_turn_id
            .and_then(|turn_id| harness.turn_diffs.iter().find(|turn| turn.id == turn_id));
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
            .max_w(window.viewport_size().width * 0.65)
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
                    .px_3()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(border()))
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Changes"),
                    )
                    .child(self.render_diff_mode_button("Unified", DiffViewMode::Unified, cx))
                    .child(self.render_diff_mode_button("Split", DiffViewMode::Split, cx))
                    .child(
                        div()
                            .id("close-diff-sidebar")
                            .size(px(26.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .text_color(rgb(muted()))
                            .hover(|style| {
                                style.bg(rgb(surface_hover())).text_color(rgb(theme_text()))
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_diff_sidebar();
                                cx.notify();
                            }))
                            .child("×"),
                    ),
            )
            .child(
                div()
                    .id("diff-turn-scroll")
                    .max_h(px(190.0))
                    .flex_none()
                    .overflow_y_scroll()
                    .track_scroll(&self.diff_turn_scroll)
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .when(harness.active_turn_diff.is_some(), |element| {
                        element.child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(rgb(orange()).opacity(0.08))
                                .text_xs()
                                .text_color(rgb(orange()))
                                .child("Turn in progress · diff freezes when complete"),
                        )
                    })
                    .when(
                        harness.turn_diffs.is_empty() && harness.active_turn_diff.is_none(),
                        |element| {
                            element.child(
                                div()
                                    .px_2()
                                    .py_3()
                                    .text_xs()
                                    .text_color(rgb(muted()))
                                    .child("Completed turn diffs will appear here."),
                            )
                        },
                    )
                    .children(harness.turn_diffs.iter().rev().map(|turn| {
                        self.render_diff_turn_row(turn, Some(turn.id) == selected_turn_id, cx)
                    })),
            )
            .child(
                div()
                    .id("diff-content-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.diff_scroll)
                    .whitespace_nowrap()
                    .text_xs()
                    .text_color(rgb(code_text()))
                    .when_some(selected_turn, |element, turn| {
                        element
                            .when(turn.files.is_empty(), |element| {
                                element.child(
                                    div().p_6().text_center().text_color(rgb(muted())).child(
                                        turn.error.clone().unwrap_or_else(|| {
                                            "No file changes in this turn.".into()
                                        }),
                                    ),
                                )
                            })
                            .children(turn.files.iter().enumerate().map(|(index, file)| {
                                self.render_diff_file(turn.id, index, file, cx)
                            }))
                    }),
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
            .child(self.render_thin_scrollbar("diff-scrollbar", &self.diff_scroll))
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
