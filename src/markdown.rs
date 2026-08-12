//! Parses Markdown into a render-friendly document model.

use std::ops::Range;

use gpui::ScrollHandle;
use pulldown_cmark::{
    Alignment as CmarkAlignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MarkdownDocument {
    pub(crate) blocks: Vec<MarkdownBlock>,
}

impl MarkdownDocument {
    /// Preserves table scroll handles across reparses of a streaming message.
    pub(crate) fn reuse_table_scrolls(&mut self, previous: &Self) {
        reuse_table_scrolls(&mut self.blocks, &previous.blocks);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MarkdownBlock {
    Paragraph(MarkdownText),
    Heading {
        level: u8,
        text: MarkdownText,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    BlockQuote(Vec<MarkdownBlock>),
    List {
        start: Option<u64>,
        items: Vec<Vec<MarkdownBlock>>,
    },
    Rule,
    Table(MarkdownTable),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct MarkdownTable {
    pub(crate) alignments: Vec<TableAlignment>,
    pub(crate) header: Vec<MarkdownText>,
    pub(crate) rows: Vec<Vec<MarkdownText>>,
    pub(crate) scroll: ScrollHandle,
}

impl PartialEq for MarkdownTable {
    fn eq(&self, other: &Self) -> bool {
        self.alignments == other.alignments
            && self.header == other.header
            && self.rows == other.rows
    }
}

impl Eq for MarkdownTable {}

fn reuse_table_scrolls(blocks: &mut [MarkdownBlock], previous: &[MarkdownBlock]) {
    for (block, previous) in blocks.iter_mut().zip(previous) {
        match (block, previous) {
            (MarkdownBlock::Table(table), MarkdownBlock::Table(previous)) => {
                table.scroll = previous.scroll.clone();
            }
            (MarkdownBlock::BlockQuote(blocks), MarkdownBlock::BlockQuote(previous)) => {
                reuse_table_scrolls(blocks, previous);
            }
            (
                MarkdownBlock::List { items, .. },
                MarkdownBlock::List {
                    items: previous, ..
                },
            ) => {
                for (blocks, previous) in items.iter_mut().zip(previous) {
                    reuse_table_scrolls(blocks, previous);
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TableAlignment {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MarkdownText {
    pub(crate) text: String,
    pub(crate) spans: Vec<MarkdownSpan>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MarkdownSpan {
    pub(crate) range: Range<usize>,
    pub(crate) style: MarkdownSpanStyle,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MarkdownSpanStyle {
    pub(crate) strong: bool,
    pub(crate) emphasis: bool,
    pub(crate) strikethrough: bool,
    pub(crate) code: bool,
    pub(crate) link: Option<String>,
}

/// Flattens visible Markdown leaves into the text used by cross-block selection.
pub(crate) fn markdown_selection_text(blocks: &[MarkdownBlock]) -> String {
    let mut leaves = Vec::new();
    for block in blocks {
        collect_selection_leaves(block, &mut leaves);
    }
    leaves.join("\n")
}

fn collect_selection_leaves<'a>(block: &'a MarkdownBlock, leaves: &mut Vec<&'a str>) {
    match block {
        MarkdownBlock::Paragraph(text) | MarkdownBlock::Heading { text, .. } => {
            if !text.text.is_empty() {
                leaves.push(&text.text);
            }
        }
        MarkdownBlock::CodeBlock { code, .. } => {
            let code = code.strip_suffix('\n').unwrap_or(code);
            if !code.is_empty() {
                leaves.push(code);
            }
        }
        MarkdownBlock::BlockQuote(blocks) => {
            for block in blocks {
                collect_selection_leaves(block, leaves);
            }
        }
        MarkdownBlock::List { items, .. } => {
            for blocks in items {
                for block in blocks {
                    collect_selection_leaves(block, leaves);
                }
            }
        }
        MarkdownBlock::Table(table) => {
            for text in table.header.iter().chain(table.rows.iter().flatten()) {
                if !text.text.is_empty() {
                    leaves.push(&text.text);
                }
            }
        }
        MarkdownBlock::Rule => {}
    }
}

#[derive(Default)]
struct InlineState {
    strong: usize,
    emphasis: usize,
    strikethrough: usize,
    links: Vec<String>,
}

#[derive(Default)]
struct InlineBuilder {
    value: MarkdownText,
    state: InlineState,
}

impl InlineBuilder {
    fn style(&self) -> MarkdownSpanStyle {
        MarkdownSpanStyle {
            strong: self.state.strong > 0,
            emphasis: self.state.emphasis > 0,
            strikethrough: self.state.strikethrough > 0,
            code: false,
            link: self.state.links.last().cloned(),
        }
    }

    fn push(&mut self, text: &str) {
        self.push_with_style(text, self.style());
    }

    fn push_code(&mut self, text: &str) {
        let mut style = self.style();
        style.code = true;
        self.push_with_style(text, style);
    }

    fn push_with_style(&mut self, text: &str, style: MarkdownSpanStyle) {
        if text.is_empty() {
            return;
        }
        let start = self.value.text.len();
        self.value.text.push_str(text);
        let end = self.value.text.len();
        if style == MarkdownSpanStyle::default() {
            return;
        }
        if let Some(last) = self.value.spans.last_mut()
            && last.range.end == start
            && last.style == style
        {
            last.range.end = end;
        } else {
            self.value.spans.push(MarkdownSpan {
                range: start..end,
                style,
            });
        }
    }

    fn finish(self) -> MarkdownText {
        self.value
    }
}

enum Frame {
    Root(Vec<MarkdownBlock>),
    Paragraph(InlineBuilder),
    Heading(u8, InlineBuilder),
    CodeBlock(Option<String>, String),
    BlockQuote(Vec<MarkdownBlock>),
    List {
        start: Option<u64>,
        items: Vec<Vec<MarkdownBlock>>,
    },
    Item {
        blocks: Vec<MarkdownBlock>,
        inline: Option<InlineBuilder>,
    },
    Table(MarkdownTable),
    TableHead(Vec<MarkdownText>),
    TableRow(Vec<MarkdownText>),
    TableCell(InlineBuilder),
    HtmlBlock(InlineBuilder),
    Footnote(Vec<MarkdownBlock>),
}

/// Converts pulldown-cmark's event stream into the nested block model consumed by the UI.
pub(crate) fn parse_markdown(source: &str) -> MarkdownDocument {
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let parser = Parser::new_ext(source, options);
    let mut frames = vec![Frame::Root(Vec::new())];

    for event in parser {
        match event {
            Event::Start(tag) => start_tag(tag, &mut frames),
            Event::End(tag) => end_tag(tag, &mut frames),
            Event::Text(text) => push_inline_or_code(&mut frames, &text),
            Event::Code(code) => {
                if let Some(inline) = current_inline(&mut frames) {
                    inline.push_code(&code);
                }
            }
            Event::SoftBreak => {
                if let Some(inline) = current_inline(&mut frames) {
                    inline.push(" ");
                }
            }
            Event::HardBreak => {
                if let Some(inline) = current_inline(&mut frames) {
                    inline.push("\n");
                }
            }
            Event::Rule => append_block(&mut frames, MarkdownBlock::Rule),
            Event::TaskListMarker(checked) => {
                if let Some(inline) = current_inline(&mut frames) {
                    inline.push(if checked { "☑ " } else { "☐ " });
                }
            }
            Event::FootnoteReference(name) => {
                if let Some(inline) = current_inline(&mut frames) {
                    inline.push(&format!("[{name}]"));
                }
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                if let Some(inline) = current_inline(&mut frames) {
                    inline.push(&html);
                }
            }
            Event::InlineMath(math) => {
                if let Some(inline) = current_inline(&mut frames) {
                    inline.push_code(&math);
                }
            }
            Event::DisplayMath(math) => append_block(
                &mut frames,
                MarkdownBlock::CodeBlock {
                    language: Some("math".to_string()),
                    code: math.into_string(),
                },
            ),
        }
    }

    while frames.len() > 1 {
        // pulldown-cmark normally balances all tags. This keeps malformed streamed
        // input useful if that behavior ever changes.
        finish_top_frame(&mut frames);
    }
    let Some(Frame::Root(blocks)) = frames.pop() else {
        unreachable!("markdown parser root frame must remain")
    };
    MarkdownDocument { blocks }
}

fn start_tag(tag: Tag<'_>, frames: &mut Vec<Frame>) {
    match tag {
        Tag::Paragraph => {
            flush_item_inline(frames);
            frames.push(Frame::Paragraph(InlineBuilder::default()));
        }
        Tag::Heading { level, .. } => {
            flush_item_inline(frames);
            frames.push(Frame::Heading(
                heading_level(level),
                InlineBuilder::default(),
            ));
        }
        Tag::CodeBlock(kind) => {
            flush_item_inline(frames);
            let language = match kind {
                CodeBlockKind::Indented => None,
                CodeBlockKind::Fenced(info) => info
                    .split_whitespace()
                    .next()
                    .filter(|language| !language.is_empty())
                    .map(str::to_string),
            };
            frames.push(Frame::CodeBlock(language, String::new()));
        }
        Tag::BlockQuote(_) => {
            flush_item_inline(frames);
            frames.push(Frame::BlockQuote(Vec::new()));
        }
        Tag::List(start) => {
            flush_item_inline(frames);
            frames.push(Frame::List {
                start,
                items: Vec::new(),
            });
        }
        Tag::Item => frames.push(Frame::Item {
            blocks: Vec::new(),
            inline: None,
        }),
        Tag::Emphasis => {
            if let Some(inline) = current_inline(frames) {
                inline.state.emphasis += 1;
            }
        }
        Tag::Strong => {
            if let Some(inline) = current_inline(frames) {
                inline.state.strong += 1;
            }
        }
        Tag::Strikethrough => {
            if let Some(inline) = current_inline(frames) {
                inline.state.strikethrough += 1;
            }
        }
        Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
            if let Some(inline) = current_inline(frames) {
                inline.state.links.push(dest_url.into_string());
            }
        }
        Tag::Table(alignments) => {
            flush_item_inline(frames);
            frames.push(Frame::Table(MarkdownTable {
                alignments: alignments.into_iter().map(table_alignment).collect(),
                ..Default::default()
            }));
        }
        Tag::TableHead => frames.push(Frame::TableHead(Vec::new())),
        Tag::TableRow => frames.push(Frame::TableRow(Vec::new())),
        Tag::TableCell => frames.push(Frame::TableCell(InlineBuilder::default())),
        Tag::HtmlBlock => {
            flush_item_inline(frames);
            frames.push(Frame::HtmlBlock(InlineBuilder::default()));
        }
        Tag::FootnoteDefinition(_) => {
            flush_item_inline(frames);
            frames.push(Frame::Footnote(Vec::new()));
        }
        _ => {}
    }
}

fn end_tag(tag: TagEnd, frames: &mut Vec<Frame>) {
    match tag {
        TagEnd::Paragraph
        | TagEnd::Heading(_)
        | TagEnd::CodeBlock
        | TagEnd::BlockQuote(_)
        | TagEnd::List(_)
        | TagEnd::Item
        | TagEnd::Table
        | TagEnd::TableHead
        | TagEnd::TableRow
        | TagEnd::TableCell
        | TagEnd::HtmlBlock
        | TagEnd::FootnoteDefinition => finish_top_frame(frames),
        TagEnd::Emphasis => {
            if let Some(inline) = current_inline(frames) {
                inline.state.emphasis = inline.state.emphasis.saturating_sub(1);
            }
        }
        TagEnd::Strong => {
            if let Some(inline) = current_inline(frames) {
                inline.state.strong = inline.state.strong.saturating_sub(1);
            }
        }
        TagEnd::Strikethrough => {
            if let Some(inline) = current_inline(frames) {
                inline.state.strikethrough = inline.state.strikethrough.saturating_sub(1);
            }
        }
        TagEnd::Link | TagEnd::Image => {
            if let Some(inline) = current_inline(frames) {
                inline.state.links.pop();
            }
        }
        _ => {}
    }
}

fn finish_top_frame(frames: &mut Vec<Frame>) {
    flush_item_inline(frames);
    let Some(frame) = frames.pop() else {
        return;
    };
    match frame {
        Frame::Root(blocks) => frames.push(Frame::Root(blocks)),
        Frame::Paragraph(inline) => append_block(frames, MarkdownBlock::Paragraph(inline.finish())),
        Frame::Heading(level, inline) => append_block(
            frames,
            MarkdownBlock::Heading {
                level,
                text: inline.finish(),
            },
        ),
        Frame::CodeBlock(language, code) => {
            append_block(frames, MarkdownBlock::CodeBlock { language, code })
        }
        Frame::BlockQuote(blocks) => append_block(frames, MarkdownBlock::BlockQuote(blocks)),
        Frame::List { start, items } => append_block(frames, MarkdownBlock::List { start, items }),
        Frame::Item { blocks, .. } => {
            if let Some(Frame::List { items, .. }) = frames.last_mut() {
                items.push(blocks);
            }
        }
        Frame::Table(table) => append_block(frames, MarkdownBlock::Table(table)),
        Frame::TableHead(cells) => {
            if let Some(Frame::Table(table)) = frames.last_mut() {
                table.header = cells;
            }
        }
        Frame::TableRow(cells) => {
            if let Some(Frame::Table(table)) = frames.last_mut() {
                table.rows.push(cells);
            }
        }
        Frame::TableCell(inline) => {
            let cell = inline.finish();
            match frames.last_mut() {
                Some(Frame::TableHead(cells)) | Some(Frame::TableRow(cells)) => cells.push(cell),
                _ => {}
            }
        }
        Frame::HtmlBlock(inline) => append_block(frames, MarkdownBlock::Paragraph(inline.finish())),
        Frame::Footnote(blocks) => append_block(frames, MarkdownBlock::BlockQuote(blocks)),
    }
}

fn append_block(frames: &mut [Frame], block: MarkdownBlock) {
    for frame in frames.iter_mut().rev() {
        match frame {
            Frame::Root(blocks) | Frame::BlockQuote(blocks) | Frame::Footnote(blocks) => {
                blocks.push(block);
                return;
            }
            Frame::Item { blocks, .. } => {
                blocks.push(block);
                return;
            }
            _ => {}
        }
    }
}

fn current_inline(frames: &mut [Frame]) -> Option<&mut InlineBuilder> {
    match frames.last_mut()? {
        Frame::Paragraph(inline)
        | Frame::Heading(_, inline)
        | Frame::TableCell(inline)
        | Frame::HtmlBlock(inline) => Some(inline),
        Frame::Item { inline, .. } => Some(inline.get_or_insert_with(InlineBuilder::default)),
        _ => None,
    }
}

fn push_inline_or_code(frames: &mut [Frame], text: &str) {
    if let Some(Frame::CodeBlock(_, code)) = frames.last_mut() {
        code.push_str(text);
    } else if let Some(inline) = current_inline(frames) {
        inline.push(text);
    }
}

fn flush_item_inline(frames: &mut [Frame]) {
    let Some(Frame::Item { blocks, inline }) = frames.last_mut() else {
        return;
    };
    if let Some(inline) = inline.take()
        && !inline.value.text.is_empty()
    {
        blocks.push(MarkdownBlock::Paragraph(inline.finish()));
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn table_alignment(alignment: CmarkAlignment) -> TableAlignment {
    match alignment {
        CmarkAlignment::None | CmarkAlignment::Left => TableAlignment::Left,
        CmarkAlignment::Center => TableAlignment::Center,
        CmarkAlignment::Right => TableAlignment::Right,
    }
}
