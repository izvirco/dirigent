use gpui::{
    AnyElement, Context, FontStyle, FontWeight, HighlightStyle, IntoElement, SharedString,
    StrikethroughStyle, UnderlineStyle, div, prelude::*, px, rgb, rgba,
};

use crate::{
    app::Dirigent,
    markdown::{MarkdownBlock, MarkdownDocument, MarkdownTable, MarkdownText, TableAlignment},
    theme::{ACCENT, BLUE, BORDER, MUTED, SURFACE, SURFACE_HOVER, TEXT},
};

const CODE_FONT: &str = "Lilex Nerd Font Mono";

impl Dirigent {
    pub(super) fn render_markdown(
        &self,
        document: &MarkdownDocument,
        message_index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_markdown_blocks(&document.blocks, &format!("markdown-{message_index}"), cx)
    }

    fn render_markdown_blocks(
        &self,
        blocks: &[MarkdownBlock],
        path: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let children = blocks
            .iter()
            .enumerate()
            .map(|(index, block)| self.render_markdown_block(block, &format!("{path}-{index}"), cx))
            .collect::<Vec<_>>();
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
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match block {
            MarkdownBlock::Paragraph(text) => self.render_markdown_text(path, text, cx),
            MarkdownBlock::Heading { level, text } => div()
                .w_full()
                .mt_1()
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(TEXT))
                .when(*level == 1, |element| {
                    element.text_xl().line_height(px(30.0))
                })
                .when(*level == 2, |element| {
                    element.text_lg().line_height(px(27.0))
                })
                .when(*level >= 3, |element| {
                    element.text_sm().line_height(px(23.0))
                })
                .child(self.render_markdown_text(path, text, cx))
                .into_any_element(),
            MarkdownBlock::CodeBlock { code, .. } => {
                self.render_markdown_code_block(path, code, cx)
            }
            MarkdownBlock::BlockQuote(blocks) => div()
                .w_full()
                .min_w(px(0.0))
                .pl_3()
                .py_1()
                .border_l_2()
                .border_color(rgb(MUTED))
                .text_color(rgb(MUTED))
                .child(self.render_markdown_blocks(blocks, &format!("{path}-quote"), cx))
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
                                    .text_color(rgb(MUTED))
                                    .child(marker),
                            )
                            .child(div().min_w(px(0.0)).flex_1().child(
                                self.render_markdown_blocks(
                                    blocks,
                                    &format!("{path}-item-{item_index}"),
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
                .bg(rgb(BORDER))
                .into_any_element(),
            MarkdownBlock::Table(table) => self.render_markdown_table(path, table, cx),
        }
    }

    fn render_markdown_text(
        &self,
        id: &str,
        text: &MarkdownText,
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
                    color: Some(rgb(MUTED).into()),
                });
            }
            if span.style.code {
                highlight.color = Some(rgb(TEXT).into());
                highlight.background_color = Some(rgba(0xffffff0f).into());
                font_overrides.push((span.range.clone(), SharedString::from(CODE_FONT)));
            }
            if let Some(url) = span.style.link.as_ref() {
                highlight.color = Some(rgb(ACCENT).into());
                highlight.underline = Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(rgb(ACCENT).into()),
                    wavy: false,
                });
                links.push((span.range.clone(), SharedString::from(url.clone())));
            }
            highlights.push((span.range.clone(), highlight));
        }
        self.render_styled_selectable_text(
            id.to_string(),
            SharedString::from(text.text.clone()),
            &highlights,
            &font_overrides,
            &links,
            false,
            cx,
        )
    }

    fn render_markdown_code_block(
        &self,
        path: &str,
        code: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let copied_code = code.to_string();
        let display_code = code.strip_suffix('\n').unwrap_or(code);
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
            .border_color(rgb(BORDER))
            .bg(rgb(SURFACE))
            .child(
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .p_2()
                    .font_family(CODE_FONT)
                    .text_xs()
                    .line_height(px(19.0))
                    .text_color(rgb(0xc7cbd4))
                    .child(self.render_selectable_text(
                        format!("{path}-code"),
                        SharedString::from(display_code.to_string()),
                        &[],
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
                    .bg(rgb(SURFACE))
                    .text_xs()
                    .text_color(rgb(if copied { BLUE } else { MUTED }))
                    .opacity(if copied { 1.0 } else { 0.0 })
                    .when(copied, |element| element.bg(rgba(0x77a7ff26)))
                    .when(!copied, |element| {
                        element
                            .group_hover(copy_group_id, |style| style.opacity(1.0))
                            .hover(|style| style.bg(rgb(SURFACE_HOVER)))
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
                            .border_color(rgb(BORDER))
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
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = text.map_or_else(
            || div().into_any_element(),
            |text| self.render_markdown_text(&format!("{path}-cell-{row}-{column}"), text, cx),
        );
        div()
            .min_w(px(0.0))
            .min_h(px(34.0))
            .px_3()
            .py_2()
            .when(!last_column, |element| element.border_r_1())
            .when(!last_row, |element| element.border_b_1())
            .border_color(rgb(BORDER))
            .when(header, |element| {
                element
                    .bg(rgb(SURFACE))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT))
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
