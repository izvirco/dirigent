use gpui::{
    AnyElement, Context, FontStyle, FontWeight, HighlightStyle, IntoElement, SharedString,
    StrikethroughStyle, UnderlineStyle, div, prelude::*, px, rgba,
};

use crate::{
    app::Dirigent,
    markdown::{
        MarkdownBlock, MarkdownDocument, MarkdownTable, MarkdownText, TableAlignment,
        markdown_selection_text,
    },
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
        self.render_markdown_blocks(&document.blocks, &id, &mut selection, cx)
    }

    fn render_markdown_blocks(
        &self,
        blocks: &[MarkdownBlock],
        path: &str,
        selection: &mut MarkdownSelectionContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut children = Vec::with_capacity(blocks.len());
        for (index, block) in blocks.iter().enumerate() {
            children.push(self.render_markdown_block(
                block,
                &format!("{path}-{index}"),
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
        selection: &mut MarkdownSelectionContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match block {
            MarkdownBlock::Paragraph(text) => self.render_markdown_text(path, text, selection, cx),
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
                .child(self.render_markdown_text(path, text, selection, cx))
                .into_any_element(),
            MarkdownBlock::CodeBlock { code, .. } => {
                self.render_markdown_code_block(path, code, selection, cx)
            }
            MarkdownBlock::BlockQuote(blocks) => div()
                .w_full()
                .min_w(px(0.0))
                .pl_3()
                .py_1()
                .border_l_2()
                .border_color(rgb(muted()))
                .text_color(rgb(muted()))
                .child(self.render_markdown_blocks(blocks, &format!("{path}-quote"), selection, cx))
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
            MarkdownBlock::Table(table) => self.render_markdown_table(path, table, selection, cx),
        }
    }

    fn render_markdown_text(
        &self,
        id: &str,
        text: &MarkdownText,
        selection: &mut MarkdownSelectionContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut highlights = Vec::with_capacity(text.spans.len());
        let mut font_overrides = Vec::new();
        let mut links = Vec::new();
        for span in &text.spans {
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
                font_overrides.push((span.range.clone(), self.font.clone()));
            }
            if let Some(url) = span.style.link.as_ref() {
                highlight.color = Some(rgb(accent()).into());
                highlight.underline = Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(rgb(accent()).into()),
                    wavy: false,
                });
                links.push((span.range.clone(), SharedString::from(url.clone())));
            }
            highlights.push((span.range.clone(), highlight));
        }
        let selection_range = selection.range_for(&text.text);
        self.render_grouped_styled_selectable_text(
            id.to_string(),
            SharedString::from(text.text.clone()),
            &highlights,
            &font_overrides,
            &links,
            selection.id.clone(),
            selection.text.clone(),
            selection_range,
            cx,
        )
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
            .child(self.render_thin_horizontal_scrollbar(scrollbar_id, &table.scroll))
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
        selection: &mut MarkdownSelectionContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = text.map_or_else(
            || div().into_any_element(),
            |text| {
                self.render_markdown_text(
                    &format!("{path}-cell-{row}-{column}"),
                    text,
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

fn table_alignment(table: &MarkdownTable, column: usize) -> TableAlignment {
    table
        .alignments
        .get(column)
        .copied()
        .unwrap_or(TableAlignment::Left)
}
