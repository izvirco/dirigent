//! Renders parsed Markdown documents as GPUI elements.

use std::ops::Range;

use gpui::{
    AnyElement, Context, FontStyle, FontWeight, HighlightStyle, IntoElement, SharedString,
    StrikethroughStyle, UnderlineStyle, div, prelude::*, px, rgba, svg,
};

use crate::{
    app::Dirigent,
    assets::insert_generated_svg,
    markdown::{
        MarkdownBlock, MarkdownDocument, MarkdownTable, MarkdownText, TableAlignment,
        markdown_selection_text,
    },
    math::{MathRenderResult, MathRenderState, MathRenderTask},
    theme::{accent, blue, border, muted, rgb, surface, surface_hover, theme_text},
};

struct MarkdownSelectionContext {
    id: String,
    text: SharedString,
    next_offset: usize,
    has_leaf: bool,
}

impl MarkdownSelectionContext {
    fn range_for(&mut self, text: &str) -> std::ops::Range<usize> {
        if text.is_empty() {
            return self.next_offset..self.next_offset;
        }
        if self.has_leaf {
            self.next_offset += 1;
        }
        self.has_leaf = true;
        let start = self.next_offset;
        self.next_offset += text.len();
        start..self.next_offset
    }
}

impl Dirigent {
    pub(super) fn render_markdown(
        &self,
        document: &MarkdownDocument,
        message_index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = format!("markdown-{message_index}");
        let text = SharedString::from(markdown_selection_text(&document.blocks));
        let mut selection = MarkdownSelectionContext {
            id: id.clone(),
            text,
            next_offset: 0,
            has_leaf: false,
        };
        self.render_markdown_blocks(&document.blocks, &id, message_index, &mut selection, cx)
    }

    fn render_markdown_blocks(
        &self,
        blocks: &[MarkdownBlock],
        path: &str,
        message_index: usize,
        selection: &mut MarkdownSelectionContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut children = Vec::with_capacity(blocks.len());
        for (index, block) in blocks.iter().enumerate() {
            children.push(self.render_markdown_block(
                block,
                &format!("{path}-{index}"),
                message_index,
                selection,
                cx,
            ));
        }
        div()
            .w_full()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .gap_2()
            .children(children)
            .into_any_element()
    }

    fn render_markdown_block(
        &self,
        block: &MarkdownBlock,
        path: &str,
        message_index: usize,
        selection: &mut MarkdownSelectionContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match block {
            MarkdownBlock::Paragraph(text) => {
                self.render_markdown_text(path, text, message_index, selection, cx)
            }
            MarkdownBlock::Heading { level, text } => div()
                .w_full()
                .mt_1()
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(theme_text()))
                .when(*level == 1, |element| {
                    element.text_xl().line_height(px(30.0))
                })
                .when(*level == 2, |element| {
                    element.text_lg().line_height(px(27.0))
                })
                .when(*level >= 3, |element| {
                    element.text_sm().line_height(px(23.0))
                })
                .child(self.render_markdown_text(path, text, message_index, selection, cx))
                .into_any_element(),
            MarkdownBlock::CodeBlock { code, .. } => {
                self.render_markdown_code_block(path, code, selection, cx)
            }
            MarkdownBlock::Math { source } => {
                self.render_markdown_math(path, source, message_index, selection, cx)
            }
            MarkdownBlock::BlockQuote(blocks) => div()
                .w_full()
                .min_w(px(0.0))
                .pl_3()
                .py_1()
                .border_l_2()
                .border_color(rgb(muted()))
                .text_color(rgb(muted()))
                .child(self.render_markdown_blocks(
                    blocks,
                    &format!("{path}-quote"),
                    message_index,
                    selection,
                    cx,
                ))
                .into_any_element(),
            MarkdownBlock::List { start, items } => {
                let children = items
                    .iter()
                    .enumerate()
                    .map(|(item_index, blocks)| {
                        let marker = start.map_or_else(
                            || "•".to_string(),
                            |start| format!("{}.", start + item_index as u64),
                        );
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .flex()
                            .items_start()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(22.0))
                                    .flex_none()
                                    .text_right()
                                    .text_color(rgb(muted()))
                                    .child(marker),
                            )
                            .child(div().min_w(px(0.0)).flex_1().child(
                                self.render_markdown_blocks(
                                    blocks,
                                    &format!("{path}-item-{item_index}"),
                                    message_index,
                                    selection,
                                    cx,
                                ),
                            ))
                            .into_any_element()
                    })
                    .collect::<Vec<_>>();
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(children)
                    .into_any_element()
            }
            MarkdownBlock::Rule => div()
                .w_full()
                .my_2()
                .h(px(1.0))
                .bg(rgb(border()))
                .into_any_element(),
            MarkdownBlock::Table(table) => {
                self.render_markdown_table(path, table, message_index, selection, cx)
            }
        }
    }

    fn render_markdown_text(
        &self,
        id: &str,
        text: &MarkdownText,
        message_index: usize,
        selection: &mut MarkdownSelectionContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selection_range = selection.range_for(&text.text);
        if text.inline_math.is_empty() {
            return self.render_markdown_text_fragment(
                id,
                text,
                0..text.text.len(),
                selection,
                selection_range,
                true,
                cx,
            );
        }

        let children = inline_fragments(text)
            .into_iter()
            .enumerate()
            .map(|(index, fragment)| match fragment {
                InlineFragment::Text(range) => self.render_markdown_text_fragment(
                    &format!("{id}-text-{index}"),
                    text,
                    range,
                    selection,
                    selection_range.clone(),
                    false,
                    cx,
                ),
                InlineFragment::Math(range) => self.render_markdown_inline_math(
                    &format!("{id}-inline-math-{index}"),
                    &text.text[range.clone()],
                    message_index,
                    selection,
                    selection_range.start + range.start..selection_range.start + range.end,
                    cx,
                ),
            })
            .collect::<Vec<_>>();

        div()
            .w_full()
            .min_w(px(0.0))
            .flex()
            .flex_wrap()
            .items_baseline()
            .children(children)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_markdown_text_fragment(
        &self,
        id: &str,
        text: &MarkdownText,
        range: Range<usize>,
        selection: &MarkdownSelectionContext,
        selection_range: Range<usize>,
        full_width: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut highlights = Vec::new();
        let mut font_overrides = Vec::new();
        let mut links = Vec::new();
        for span in &text.spans {
            let start = span.range.start.max(range.start);
            let end = span.range.end.min(range.end);
            if start >= end {
                continue;
            }
            let local_range = start - range.start..end - range.start;
            let mut highlight = HighlightStyle::default();
            if span.style.strong {
                highlight.font_weight = Some(FontWeight::BOLD);
            }
            if span.style.emphasis {
                highlight.font_style = Some(FontStyle::Italic);
            }
            if span.style.strikethrough {
                highlight.strikethrough = Some(StrikethroughStyle {
                    thickness: px(1.0),
                    color: Some(rgb(muted()).into()),
                });
            }
            if span.style.code {
                highlight.color = Some(rgb(theme_text()).into());
                highlight.background_color = Some(rgba(0xffffff0f).into());
                font_overrides.push((local_range.clone(), self.font.clone()));
            }
            if let Some(url) = span.style.link.as_ref() {
                highlight.color = Some(rgb(accent()).into());
                highlight.underline = Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(rgb(accent()).into()),
                    wavy: false,
                });
                links.push((local_range.clone(), SharedString::from(url.clone())));
            }
            highlights.push((local_range, highlight));
        }

        let source = SharedString::from(text.text[range.clone()].to_string());
        let fragment_selection =
            selection_range.start + range.start..selection_range.start + range.end;
        if full_width {
            self.render_grouped_styled_selectable_text(
                id.to_string(),
                source,
                &highlights,
                &font_overrides,
                &links,
                selection.id.clone(),
                selection.text.clone(),
                fragment_selection,
                cx,
            )
        } else {
            self.render_grouped_styled_selectable_text_inline(
                id.to_string(),
                source,
                &highlights,
                &font_overrides,
                &links,
                selection.id.clone(),
                selection.text.clone(),
                fragment_selection,
                cx,
            )
        }
    }

    fn math_render_state(&self, source: &str, message_index: usize) -> MathRenderState {
        let waiter = self
            .selected_harness
            .map(|harness_id| (harness_id, message_index));
        let (mut state, should_queue) = {
            let mut renders = self.math_renders.borrow_mut();
            match renders.get_mut(source) {
                Some(MathRenderState::Pending { messages }) => {
                    messages.extend(waiter);
                    (
                        MathRenderState::Pending {
                            messages: messages.clone(),
                        },
                        false,
                    )
                }
                Some(state) => (state.clone(), false),
                None => {
                    let messages = waiter.into_iter().collect();
                    let state = MathRenderState::Pending { messages };
                    renders.insert(source.to_string(), state.clone());
                    (state, true)
                }
            }
        };
        if should_queue
            && let Err(error) = self.math_render_tasks.try_send(MathRenderTask {
                source: source.to_string(),
            })
        {
            tracing::warn!(error = %error, "could not queue math rendering");
            state = MathRenderState::Failed;
            self.math_renders
                .borrow_mut()
                .insert(source.to_string(), state.clone());
        }
        state
    }

    fn render_markdown_math(
        &self,
        path: &str,
        source: &str,
        message_index: usize,
        selection: &mut MarkdownSelectionContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.math_render_state(source, message_index);
        let selection_range = selection.range_for(source);
        match state {
            MathRenderState::Ready {
                asset_path,
                width,
                height,
            } => div()
                .id(format!("{path}-math-scroll"))
                .w_full()
                .min_w(px(0.0))
                .overflow_x_scroll()
                .child(
                    svg()
                        .path(asset_path)
                        .w(px(width.ceil()))
                        .h(px(height.ceil()))
                        .flex_none()
                        .text_color(rgb(theme_text())),
                )
                .into_any_element(),
            MathRenderState::Pending { .. } | MathRenderState::Failed => {
                let font_overrides = [(0..source.len(), self.font.clone())];
                self.render_grouped_styled_selectable_text(
                    format!("{path}-math"),
                    SharedString::from(source.to_string()),
                    &[],
                    &font_overrides,
                    &[],
                    selection.id.clone(),
                    selection.text.clone(),
                    selection_range,
                    cx,
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_markdown_inline_math(
        &self,
        path: &str,
        source: &str,
        message_index: usize,
        selection: &MarkdownSelectionContext,
        selection_range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match self.math_render_state(source, message_index) {
            MathRenderState::Ready {
                asset_path,
                width,
                height,
            } => div()
                .id(path.to_string())
                .flex_none()
                .child(
                    svg()
                        .path(asset_path)
                        .w(px(width.ceil()))
                        .h(px(height.ceil()))
                        .flex_none()
                        .text_color(rgb(theme_text())),
                )
                .into_any_element(),
            MathRenderState::Pending { .. } | MathRenderState::Failed => {
                let font_overrides = [(0..source.len(), self.font.clone())];
                self.render_grouped_styled_selectable_text_inline(
                    path.to_string(),
                    SharedString::from(source.to_string()),
                    &[],
                    &font_overrides,
                    &[],
                    selection.id.clone(),
                    selection.text.clone(),
                    selection_range,
                    cx,
                )
            }
        }
    }

    pub(crate) fn handle_math_render_result(&mut self, result: MathRenderResult) {
        let messages = {
            let mut renders = self.math_renders.borrow_mut();
            let Some(MathRenderState::Pending { messages }) = renders.get(&result.source) else {
                return;
            };
            let messages = messages.clone();
            let state = match result.rendered {
                Ok(rendered) => {
                    let asset_path = format!(
                        "generated/math-{}.svg",
                        blake3::hash(result.source.as_bytes()).to_hex()
                    );
                    insert_generated_svg(asset_path.clone(), rendered.svg);
                    MathRenderState::Ready {
                        asset_path,
                        width: rendered.width,
                        height: rendered.height,
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "could not render math");
                    MathRenderState::Failed
                }
            };
            renders.insert(result.source, state);
            messages
        };

        let Some(selected_harness) = self.selected_harness else {
            return;
        };
        let render_items = messages
            .into_iter()
            .filter(|(harness_id, _)| *harness_id == selected_harness)
            .filter_map(|(_, message_index)| {
                self.conversation_render_cache
                    .message_render_item_index(message_index)
            })
            .collect::<std::collections::HashSet<_>>();
        for render_item in &render_items {
            self.conversation_list
                .remeasure_items(*render_item..*render_item + 1);
        }
        if !render_items.is_empty() {
            self.conversation_render_cache.invalidate_ruler_layout();
        }
    }

    fn render_markdown_code_block(
        &self,
        path: &str,
        code: &str,
        selection: &mut MarkdownSelectionContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_markdown_code_block_with_copy(path, code, code, selection, cx)
    }

    fn render_markdown_code_block_with_copy(
        &self,
        path: &str,
        code: &str,
        full_code: &str,
        selection: &mut MarkdownSelectionContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let copied_code = full_code.to_string();
        let display_code = code.strip_suffix('\n').unwrap_or(code);
        let selection_range = selection.range_for(display_code);
        let copy_button_id = format!("{path}-copy");
        let copy_group_id = format!("{path}-copy-group");
        let copied = self
            .copied_button
            .as_ref()
            .is_some_and(|(button_id, _)| button_id == &copy_button_id);
        let clicked_id = copy_button_id.clone();
        div()
            .relative()
            .group(copy_group_id.clone())
            .w_full()
            .min_w(px(0.0))
            .overflow_hidden()
            .rounded_md()
            .border_1()
            .border_color(rgb(border()))
            .bg(rgb(surface()))
            .child(
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .p_2()
                    .font_family(self.font.clone())
                    .text_xs()
                    .line_height(px(19.0))
                    .text_color(rgb(crate::theme::code_text()))
                    .child(self.render_grouped_styled_selectable_text(
                        format!("{path}-code"),
                        SharedString::from(display_code.to_string()),
                        &[],
                        &[],
                        &[],
                        selection.id.clone(),
                        selection.text.clone(),
                        selection_range,
                        cx,
                    )),
            )
            .child(
                div()
                    .id(copy_button_id)
                    .absolute()
                    .top(px(3.0))
                    .right(px(3.0))
                    .px_2()
                    .h(px(20.0))
                    .flex()
                    .items_center()
                    .rounded_md()
                    .cursor_default()
                    .bg(rgb(surface()))
                    .text_xs()
                    .text_color(rgb(if copied { blue() } else { muted() }))
                    .opacity(if copied { 1.0 } else { 0.0 })
                    .when(copied, |element| element.bg(rgb(blue()).opacity(0.15)))
                    .when(!copied, |element| {
                        element
                            .group_hover(copy_group_id, |style| style.opacity(1.0))
                            .hover(|style| style.bg(rgb(surface_hover())))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.copy_text_with_feedback(clicked_id.clone(), copied_code.clone(), cx);
                        cx.stop_propagation();
                    }))
                    .child("Copy"),
            )
            .into_any_element()
    }

    fn render_markdown_table(
        &self,
        path: &str,
        table: &MarkdownTable,
        message_index: usize,
        selection: &mut MarkdownSelectionContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let column_count = table
            .alignments
            .len()
            .max(table.header.len())
            .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
        if column_count == 0 {
            return div().into_any_element();
        }

        let row_count = usize::from(!table.header.is_empty()) + table.rows.len();
        let mut cells = Vec::new();
        if !table.header.is_empty() {
            for column in 0..column_count {
                cells.push(self.render_markdown_table_cell(
                    path,
                    0,
                    column,
                    table.header.get(column),
                    table_alignment(table, column),
                    true,
                    row_count == 1,
                    column + 1 == column_count,
                    message_index,
                    selection,
                    cx,
                ));
            }
        }
        let row_offset = usize::from(!table.header.is_empty());
        for (row, values) in table.rows.iter().enumerate() {
            for column in 0..column_count {
                cells.push(self.render_markdown_table_cell(
                    path,
                    row + row_offset,
                    column,
                    values.get(column),
                    table_alignment(table, column),
                    false,
                    row + row_offset + 1 == row_count,
                    column + 1 == column_count,
                    message_index,
                    selection,
                    cx,
                ));
            }
        }

        let scrollbar_id = format!("{path}-horizontal-scrollbar");
        div()
            .relative()
            .group(scrollbar_id.clone())
            .w_full()
            .min_w(px(0.0))
            .pb_2()
            .child(
                div()
                    .id(format!("{path}-scroll"))
                    .w_full()
                    .min_w(px(0.0))
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&table.scroll)
                    .child(
                        div()
                            .grid()
                            .grid_cols(column_count.min(u16::MAX as usize) as u16)
                            .w_full()
                            .min_w(px((column_count as f32 * 120.0).max(280.0)))
                            .overflow_hidden()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(border()))
                            .children(cells),
                    ),
            )
            .child(self.render_thin_horizontal_scrollbar(scrollbar_id, &table.scroll, cx))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_markdown_table_cell(
        &self,
        path: &str,
        row: usize,
        column: usize,
        text: Option<&MarkdownText>,
        alignment: TableAlignment,
        header: bool,
        last_row: bool,
        last_column: bool,
        message_index: usize,
        selection: &mut MarkdownSelectionContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = text.map_or_else(
            || div().into_any_element(),
            |text| {
                self.render_markdown_text(
                    &format!("{path}-cell-{row}-{column}"),
                    text,
                    message_index,
                    selection,
                    cx,
                )
            },
        );
        div()
            .min_w(px(0.0))
            .min_h(px(34.0))
            .px_3()
            .py_2()
            .when(!last_column, |element| element.border_r_1())
            .when(!last_row, |element| element.border_b_1())
            .border_color(rgb(border()))
            .when(header, |element| {
                element
                    .bg(rgb(surface()))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme_text()))
            })
            .when(alignment == TableAlignment::Center, |element| {
                element.text_center()
            })
            .when(alignment == TableAlignment::Right, |element| {
                element.text_right()
            })
            .child(content)
            .into_any_element()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum InlineFragment {
    Text(Range<usize>),
    Math(Range<usize>),
}

/// Splitting prose into word-sized flex children lets inline SVGs participate in normal wrapping.
fn inline_fragments(text: &MarkdownText) -> Vec<InlineFragment> {
    let mut fragments = Vec::new();
    let mut cursor = 0;
    for math in &text.inline_math {
        push_text_fragments(&text.text, cursor..math.start, &mut fragments);
        fragments.push(InlineFragment::Math(math.clone()));
        cursor = math.end;
    }
    push_text_fragments(&text.text, cursor..text.text.len(), &mut fragments);
    fragments
}

fn push_text_fragments(text: &str, range: Range<usize>, fragments: &mut Vec<InlineFragment>) {
    if range.is_empty() {
        return;
    }
    let mut start = range.start;
    let mut previous_was_whitespace = false;
    for (offset, character) in text[range.clone()].char_indices() {
        let index = range.start + offset;
        if previous_was_whitespace && !character.is_whitespace() {
            fragments.push(InlineFragment::Text(start..index));
            start = index;
        }
        previous_was_whitespace = character.is_whitespace();
    }
    fragments.push(InlineFragment::Text(start..range.end));
}

fn table_alignment(table: &MarkdownTable, column: usize) -> TableAlignment {
    table
        .alignments
        .get(column)
        .copied()
        .unwrap_or(TableAlignment::Left)
}
