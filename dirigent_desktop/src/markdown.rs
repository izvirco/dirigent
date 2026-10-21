//! Markdown → gpui elements, hand-rolled on top of pulldown-cmark. Blocks become `div`s,
//! inline styling becomes highlight runs on a `StyledText`, and every paragraph / code
//! block is a selectable `select::Block`. Text size / colour / font are inherited from the
//! caller, so the same renderer serves replies, user bubbles, and the mono work log.
//! Links are coloured but not clickable; images show their alt text.

use crate::select::{Block, Keys};
use crate::theme;
use gpui::{
    AnyElement, Div, FontStyle, FontWeight, HighlightStyle, SharedString, StrikethroughStyle, StyledText, UnderlineStyle, div, prelude::*,
    px, rgb,
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::iter::Peekable;
use std::ops::Range;

/// Render `text` as a column of blocks. `mono` is the font for code; `keys` places the
/// text blocks in their document for selection.
pub fn render(text: &str, mono: &SharedString, keys: &mut Keys) -> Div {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut events = Parser::new_ext(text, opts).peekable();
    let mut r = Renderer { mono, keys };
    div().flex().flex_col().gap_2().children(r.blocks(&mut events))
}

/// The prefix of `text` that ends at its last newline — what streams get rendered as, so
/// blocks appear whole rather than word by word.
pub fn committed(text: &str) -> &str {
    text.rfind('\n').map_or("", |i| &text[..=i])
}

type Events<'a> = Peekable<Parser<'a>>;

struct Renderer<'m> {
    mono: &'m SharedString,
    keys: &'m mut Keys,
}

impl Renderer<'_> {
    /// Blocks until the `End` that closes the enclosing container (or the input ends).
    /// Inline events found here (tight list items, table cells) form an implicit paragraph.
    fn blocks(&mut self, ev: &mut Events) -> Vec<AnyElement> {
        let mut out = vec![];
        while let Some(e) = ev.next() {
            match e {
                Event::End(_) => break,
                Event::Start(tag) => out.push(self.block(tag, ev)),
                Event::Rule => out.push(div().h(px(1.)).w_full().my_1().bg(rgb(theme::LINE)).into_any()),
                inline => {
                    let mut p = Inline::default();
                    p.event(inline, self.mono);
                    self.inline_until_block(&mut p, ev);
                    out.push(p.block(self.keys).into_any());
                }
            }
        }
        out
    }

    /// One block from its `Start` tag through its `End`.
    fn block(&mut self, tag: Tag, ev: &mut Events) -> AnyElement {
        match tag {
            Tag::Paragraph => self.paragraph(ev).into_any(),
            Tag::Heading { level, .. } => {
                let size = match level as u8 {
                    1 => 20.,
                    2 => 17.,
                    _ => 15.,
                };
                div().mt_1().text_size(px(size)).font_weight(FontWeight::SEMIBOLD).child(self.paragraph(ev)).into_any()
            }
            Tag::CodeBlock(_) => {
                let mut code = String::new();
                for e in ev.by_ref() {
                    match e {
                        Event::Text(t) => code.push_str(&t),
                        Event::End(TagEnd::CodeBlock) => break,
                        _ => {}
                    }
                }
                self.code_block(code.trim_end().to_owned()).into_any()
            }
            Tag::HtmlBlock => {
                let mut html = String::new();
                for e in ev.by_ref() {
                    match e {
                        Event::Html(t) | Event::Text(t) => html.push_str(&t),
                        Event::End(TagEnd::HtmlBlock) => break,
                        _ => {}
                    }
                }
                self.code_block(html.trim_end().to_owned()).into_any()
            }
            Tag::BlockQuote(_) => div()
                .border_l_2()
                .border_color(rgb(theme::LINE))
                .pl_3()
                .text_color(rgb(theme::FG_1))
                .flex()
                .flex_col()
                .gap_2()
                .children(self.blocks(ev))
                .into_any(),
            Tag::List(start) => self.list(start, ev),
            Tag::Table(_) => self.table(ev),
            // Anything else (footnotes, definition lists, math, stray inline tags): just its content.
            _ => div().flex().flex_col().gap_2().children(self.blocks(ev)).into_any(),
        }
    }

    /// Items of a list; nested lists come through `blocks` recursively.
    fn list(&mut self, start: Option<u64>, ev: &mut Events) -> AnyElement {
        let mut items = vec![];
        let mut n = start.unwrap_or(1);
        while let Some(e) = ev.next() {
            match e {
                Event::Start(Tag::Item) => {
                    let marker = if start.is_some() { format!("{n}.") } else { "•".to_owned() };
                    n += 1;
                    items.push(
                        div()
                            .flex()
                            .gap_2()
                            .child(div().flex_shrink_0().min_w(px(16.)).text_color(rgb(theme::FG_1)).child(marker))
                            .child(div().flex_1().min_w_0().flex().flex_col().gap_1().children(self.blocks(ev)))
                            .into_any(),
                    );
                }
                Event::End(_) => break,
                _ => {}
            }
        }
        div().flex().flex_col().gap_1().children(items).into_any()
    }

    /// A simple grid: rows of equal-width cells, the header row bold with a rule under it.
    fn table(&mut self, ev: &mut Events) -> AnyElement {
        let mut rows = vec![];
        while let Some(e) = ev.next() {
            let head = match e {
                Event::Start(Tag::TableHead) => true,
                Event::Start(Tag::TableRow) => false,
                Event::End(_) => break,
                _ => continue,
            };
            let mut cells = vec![];
            while let Some(e) = ev.next() {
                match e {
                    Event::Start(Tag::TableCell) => cells.push(div().flex_1().min_w_0().px_2().py_1().child(self.paragraph(ev))),
                    Event::End(_) => break,
                    _ => {}
                }
            }
            let row = div().flex().w_full().border_b_1().border_color(rgb(theme::LINE_SOFT)).children(cells);
            rows.push(if head { row.font_weight(FontWeight::SEMIBOLD).border_color(rgb(theme::LINE)) } else { row });
        }
        div().flex().flex_col().w_full().my_1().children(rows).into_any()
    }

    /// Inline content up to the `End` of the enclosing paragraph / heading / cell.
    fn paragraph(&mut self, ev: &mut Events) -> Block {
        let mut p = Inline::default();
        let mut depth = 0usize;
        for e in ev.by_ref() {
            match e {
                Event::Start(_) => depth += 1,
                Event::End(_) if depth == 0 => break,
                Event::End(_) => depth -= 1,
                _ => {}
            }
            p.event(e, self.mono);
        }
        p.block(self.keys)
    }

    /// Inline content of a tight list item: stops before the next block-level event.
    fn inline_until_block(&mut self, p: &mut Inline, ev: &mut Events) {
        let mut depth = 0usize;
        while let Some(e) = ev.peek() {
            match e {
                Event::Start(t) if is_inline(t) => depth += 1,
                Event::Start(_) => break,
                Event::End(_) if depth == 0 => break,
                Event::End(_) => depth -= 1,
                _ => {}
            }
            let e = ev.next().unwrap();
            p.event(e, self.mono);
        }
    }

    fn code_block(&mut self, code: String) -> Div {
        div()
            .w_full()
            .rounded_md()
            .bg(rgb(theme::RAISED))
            .border_1()
            .border_color(rgb(theme::LINE_SOFT))
            .px_3()
            .py_2()
            .font_family(self.mono.clone())
            .text_size(px(12.5))
            .line_height(px(19.))
            .child(self.keys.plain(code))
    }
}

fn is_inline(t: &Tag) -> bool {
    matches!(t, Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } | Tag::Image { .. } | Tag::Superscript | Tag::Subscript)
}

/// Accumulates one paragraph's text plus the style of each span, as nesting counters for
/// the inline tags currently open.
#[derive(Default)]
struct Inline {
    text: String,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    code_ranges: Vec<(Range<usize>, SharedString)>,
    bold: u32,
    italic: u32,
    strike: u32,
    link: u32,
}

impl Inline {
    fn event(&mut self, e: Event, mono: &SharedString) {
        match e {
            Event::Text(t) | Event::InlineHtml(t) | Event::Html(t) => self.push(&t, None),
            Event::Code(t) | Event::InlineMath(t) | Event::DisplayMath(t) => self.push(&t, Some(mono)),
            Event::SoftBreak => self.push(" ", None),
            Event::HardBreak => self.push("\n", None),
            Event::TaskListMarker(done) => self.push(if done { "☑ " } else { "☐ " }, None),
            Event::FootnoteReference(t) => self.push(&format!("[{t}]"), None),
            Event::Start(Tag::Strong) => self.bold += 1,
            Event::End(TagEnd::Strong) => self.bold -= 1,
            Event::Start(Tag::Emphasis) => self.italic += 1,
            Event::End(TagEnd::Emphasis) => self.italic -= 1,
            Event::Start(Tag::Strikethrough) => self.strike += 1,
            Event::End(TagEnd::Strikethrough) => self.strike -= 1,
            Event::Start(Tag::Link { .. } | Tag::Image { .. }) => self.link += 1,
            Event::End(TagEnd::Link | TagEnd::Image) => self.link -= 1,
            _ => {}
        }
    }

    fn push(&mut self, s: &str, code: Option<&SharedString>) {
        let range = self.text.len()..self.text.len() + s.len();
        self.text.push_str(s);
        let mut h = HighlightStyle::default();
        if self.bold > 0 {
            h.font_weight = Some(FontWeight::SEMIBOLD);
        }
        if self.italic > 0 {
            h.font_style = Some(FontStyle::Italic);
        }
        if self.strike > 0 {
            h.strikethrough = Some(StrikethroughStyle { thickness: px(1.), color: None });
        }
        if self.link > 0 {
            h.color = Some(rgb(theme::NAVY_INK).into());
            h.underline = Some(UnderlineStyle { thickness: px(1.), color: None, wavy: false });
        }
        if let Some(mono) = code {
            h.background_color = Some(rgb(theme::HOVER).into());
            self.code_ranges.push((range.clone(), mono.clone()));
        }
        if h != HighlightStyle::default() {
            self.highlights.push((range, h));
        }
    }

    fn block(self, keys: &mut Keys) -> Block {
        let text = SharedString::from(self.text);
        keys.styled(StyledText::new(text.clone()).with_highlights(self.highlights).with_font_family_overrides(self.code_ranges), text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_stops_at_last_newline() {
        assert_eq!(committed("a\nb\nc"), "a\nb\n");
        assert_eq!(committed("abc"), "");
        assert_eq!(committed(""), "");
    }

    #[test]
    fn inline_runs() {
        let mono = SharedString::from("Mono");
        let mut p = Inline::default();
        for e in Parser::new("a **b** `c`") {
            p.event(e, &mono);
        }
        assert_eq!(p.text, "a b c");
        assert_eq!(p.highlights.len(), 2);
        assert_eq!(p.highlights[0].0, 2..3);
        assert_eq!(p.highlights[0].1.font_weight, Some(FontWeight::SEMIBOLD));
        assert_eq!(p.code_ranges, vec![(4..5, mono)]);
    }
}
