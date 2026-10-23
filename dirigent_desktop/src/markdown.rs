//! Markdown → gpui elements, hand-rolled on top of pulldown-cmark. Blocks become `div`s,
//! inline styling becomes highlight runs on a `StyledText`, and every paragraph / code
//! block is a selectable `select::Block`. Text size / colour / font are inherited from the
//! caller, so the same renderer serves replies, user bubbles, and the mono work log.
//! Links open in the browser on click: markdown links with a scheme (`https:`, `mailto:`,
//! ...) and bare `http(s)://` URLs in the text. An image whose target is `blob:<id>` (a
//! reply showing a local file; the host rewrote the link, see `view::blob_links`) is drawn
//! by the caller's `ImageFn` after its paragraph's text; any other image shows its alt text.

use crate::select::{Block, Keys};
use crate::theme;
use gpui::{
    AnyElement, Div, FontStyle, FontWeight, HighlightStyle, SharedString, StrikethroughStyle, StyledText, UnderlineStyle, div, prelude::*,
    px, rgba,
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, TextMergeStream};
use std::iter::Peekable;
use std::ops::Range;

/// How a document shows a `blob:` image, given the id. `None` leaves such links as alt text.
pub type ImageFn<'a> = Option<&'a mut (dyn FnMut(&str) -> AnyElement + 'a)>;

/// Render `text` as a column of blocks. `mono` is the font for code; `keys` places the
/// text blocks in their document for selection.
pub fn render(text: &str, mono: &SharedString, keys: &mut Keys, image: ImageFn) -> Div {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    // Merged so a bare URL the parser split into several text events is found whole.
    let mut events = TextMergeStream::new(Parser::new_ext(text, opts)).peekable();
    let mut r = Renderer { mono, keys, image };
    div().flex().flex_col().gap_2().children(r.blocks(&mut events))
}

/// The prefix of `text` that ends at its last newline — what streams get rendered as, so
/// blocks appear whole rather than word by word.
pub fn committed(text: &str) -> &str {
    text.rfind('\n').map_or("", |i| &text[..=i])
}

type Events<'a> = Peekable<TextMergeStream<'a, Parser<'a>>>;

struct Renderer<'m, 'i> {
    mono: &'m SharedString,
    keys: &'m mut Keys,
    image: ImageFn<'i>,
}

impl Renderer<'_, '_> {
    /// Blocks until the `End` that closes the enclosing container (or the input ends).
    /// Inline events found here (tight list items, table cells) form an implicit paragraph.
    fn blocks(&mut self, ev: &mut Events) -> Vec<AnyElement> {
        let mut out = vec![];
        while let Some(e) = ev.next() {
            match e {
                Event::End(_) => break,
                Event::Start(tag) if !is_inline(&tag) => out.push(self.block(tag, ev)),
                Event::Rule => out.push(div().h(px(1.)).w_full().my_1().bg(rgba(theme::t().line)).into_any()),
                inline => {
                    let mut p = Inline::new(self.image.is_some());
                    // The run may open with an inline tag (`- **Label:** text`), still open
                    // when `inline_until_block` takes over.
                    let depth = matches!(inline, Event::Start(_)) as usize;
                    p.event(inline, self.mono);
                    self.inline_until_block(&mut p, ev, depth);
                    out.push(self.finish(p));
                }
            }
        }
        out
    }

    /// One block from its `Start` tag through its `End`.
    fn block(&mut self, tag: Tag, ev: &mut Events) -> AnyElement {
        match tag {
            Tag::Paragraph => self.paragraph(ev),
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
                .border_color(rgba(theme::t().line))
                .pl_3()
                .text_color(rgba(theme::t().fg_1))
                .flex()
                .flex_col()
                .gap_2()
                .children(self.blocks(ev))
                .into_any(),
            Tag::List(start) => self.list(start, ev),
            Tag::Table(_) => self.table(ev),
            // Anything else (footnotes, definition lists, metadata blocks): just its content.
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
                            .child(div().flex_shrink_0().min_w(px(16.)).text_color(rgba(theme::t().fg_1)).child(marker))
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
            let row = div().flex().w_full().border_b_1().border_color(rgba(theme::t().line_soft)).children(cells);
            rows.push(if head { row.font_weight(FontWeight::SEMIBOLD).border_color(rgba(theme::t().line)) } else { row });
        }
        div().flex().flex_col().w_full().my_1().children(rows).into_any()
    }

    /// Inline content up to the `End` of the enclosing paragraph / heading / cell.
    fn paragraph(&mut self, ev: &mut Events) -> AnyElement {
        let mut p = Inline::new(self.image.is_some());
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
        self.finish(p)
    }

    /// The paragraph's text block, followed by the images it linked (each on its own row).
    fn finish(&mut self, mut p: Inline) -> AnyElement {
        let images = std::mem::take(&mut p.images);
        if images.is_empty() {
            return p.block(self.keys).into_any();
        }
        let text = (!p.text.is_empty()).then(|| p.block(self.keys));
        let draw = self.image.as_deref_mut().expect("images are only collected with an ImageFn");
        div().flex().flex_col().gap_2().children(text).children(images.iter().map(|id| draw(id))).into_any()
    }

    /// Inline content of a tight list item: stops before the next block-level event.
    /// `depth` counts the inline tags already open.
    fn inline_until_block(&mut self, p: &mut Inline, ev: &mut Events, mut depth: usize) {
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
            .bg(rgba(theme::t().raised))
            .border_1()
            .border_color(rgba(theme::t().line_soft))
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
/// the inline tags currently open. `blob:` images are collected in `images` (their alt
/// text dropped) when `show_images` is set, else kept as alt text like any other image.
#[derive(Default)]
struct Inline {
    text: String,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    code_ranges: Vec<(Range<usize>, SharedString)>,
    /// Clickable spans and their URLs.
    links: Vec<(Range<usize>, SharedString)>,
    /// Markdown links / images currently open: where their text starts, and their URL if
    /// it's one we can open.
    open_links: Vec<(usize, Option<SharedString>)>,
    bold: u32,
    italic: u32,
    strike: u32,
    link: u32,
    show_images: bool,
    images: Vec<String>,
    /// Inside a collected image's alt text.
    hidden: u32,
}

impl Inline {
    fn new(show_images: bool) -> Self {
        Self { show_images, ..Default::default() }
    }

    fn event(&mut self, e: Event, mono: &SharedString) {
        match e {
            Event::Start(Tag::Image { dest_url, .. }) if self.show_images && dest_url.starts_with("blob:") => {
                self.images.push(dest_url["blob:".len()..].to_owned());
                self.hidden += 1;
            }
            Event::End(TagEnd::Image) if self.hidden > 0 => self.hidden -= 1,
            Event::Text(t) if self.link == 0 => self.text_with_urls(&t),
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
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                self.link += 1;
                self.open_links.push((self.text.len(), has_scheme(&dest_url).then(|| dest_url.to_string().into())));
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                self.link -= 1;
                if let Some((start, Some(url))) = self.open_links.pop() {
                    self.links.push((start..self.text.len(), url));
                }
            }
            _ => {}
        }
    }

    /// Plain text, with any bare URLs in it styled and clickable as links.
    fn text_with_urls(&mut self, t: &str) {
        let mut at = 0;
        for url in bare_urls(t) {
            self.push(&t[at..url.start], None);
            let start = self.text.len();
            self.link += 1;
            self.push(&t[url.clone()], None);
            self.link -= 1;
            self.links.push((start..self.text.len(), t[url.clone()].to_owned().into()));
            at = url.end;
        }
        self.push(&t[at..], None);
    }

    fn push(&mut self, s: &str, code: Option<&SharedString>) {
        if s.is_empty() || self.hidden > 0 {
            return;
        }
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
            h.color = Some(rgba(theme::t().accent_ink).into());
            h.underline = Some(UnderlineStyle { thickness: px(1.), color: None, wavy: false });
        }
        if let Some(mono) = code {
            h.background_color = Some(rgba(theme::t().hover).into());
            self.code_ranges.push((range.clone(), mono.clone()));
        }
        if h != HighlightStyle::default() {
            self.highlights.push((range, h));
        }
    }

    fn block(self, keys: &mut Keys) -> Block {
        let text = SharedString::from(self.text);
        keys.styled(StyledText::new(text.clone()).with_highlights(self.highlights).with_font_family_overrides(self.code_ranges), text)
            .links(self.links)
    }
}

/// A link target we can hand to the OS: it starts with a URL scheme (`https:`, `mailto:`).
/// Relative paths and `#anchors` have nothing to open against.
fn has_scheme(url: &str) -> bool {
    url.split_once(':').is_some_and(|(s, _)| {
        s.starts_with(|c: char| c.is_ascii_alphabetic()) && s.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
    })
}

/// Byte ranges of the bare `http://` / `https://` URLs in `text`. A URL runs to the next
/// space or one of `<>"`; trailing punctuation is the sentence's, not the URL's, and so is
/// a closing bracket without an opening one in the URL (`(see https://x.dev/a)`).
fn bare_urls(text: &str) -> Vec<Range<usize>> {
    let mut out = vec![];
    let mut i = 0;
    while let Some(at) = text[i..].find("http").map(|a| a + i) {
        i = at + 4;
        let rest = &text[at..];
        let Some(scheme) = ["https://", "http://"].into_iter().find(|s| rest.starts_with(s)) else { continue };
        // Not glued to a preceding word (`xhttp://`).
        if text[..at].chars().next_back().is_some_and(|c| c.is_alphanumeric()) {
            continue;
        }
        let mut end = at + rest.find(|c: char| c.is_whitespace() || "<>\"".contains(c)).unwrap_or(rest.len());
        while let Some(c) = text[at..end].chars().next_back() {
            let url = &text[at..end];
            let unbalanced = |open: char| url.matches(open).count() < url.matches(c).count();
            let trailing = match c {
                '.' | ',' | ':' | ';' | '!' | '?' | '\'' => true,
                ')' => unbalanced('('),
                ']' => unbalanced('['),
                _ => false,
            };
            if !trailing {
                break;
            }
            end -= c.len_utf8();
        }
        if end > at + scheme.len() {
            out.push(at..end);
            i = end;
        }
    }
    out
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
    fn blob_images_leave_the_text() {
        let mono = SharedString::from("Mono");
        let id = "b".repeat(64);
        let src = format!("see ![the home page](blob:{id}) and ![alt](https://x/y.png)");
        let mut p = Inline::new(true);
        for e in Parser::new(&src) {
            p.event(e, &mono);
        }
        assert_eq!(p.text, "see  and alt");
        assert_eq!(p.images, vec![id]);
        // Without an ImageFn the alt text stays, like any other image.
        let mut p = Inline::new(false);
        for e in Parser::new(&src) {
            p.event(e, &mono);
        }
        assert_eq!(p.text, "see the home page and alt");
        assert!(p.images.is_empty());
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

    #[test]
    fn list_item_opening_with_inline_tag_is_one_paragraph() {
        let sel = crate::select::Selection::default();
        let mut keys = Keys::new(&sel, crate::select::Doc::Thread, 0);
        render("- **Label:** text\n- [a](https://x.dev) *b* c\n", &"Mono".into(), &mut keys, None);
        assert_eq!(sel.texts(), ["Label: text", "a b c"]);
    }

    /// The links of `md`'s first paragraph, as (link text, url).
    fn links(md: &str) -> Vec<(String, String)> {
        let mono = SharedString::from("Mono");
        let mut p = Inline::default();
        for e in TextMergeStream::new(Parser::new(md)) {
            p.event(e, &mono);
        }
        p.links.iter().map(|(r, url)| (p.text[r.clone()].to_owned(), url.to_string())).collect()
    }

    #[test]
    fn links_in_text() {
        let issue = "https://forge.sebba.dev/izvir/dirigent/issues/11";
        assert_eq!(links(&format!("I created issue #11: {issue}.")), [(issue.to_owned(), issue.to_owned())]);
        // Markdown links open their target; ones without a scheme (file paths) don't.
        assert_eq!(links("[docs](https://x.dev/a) and [main](src/main.rs)"), [("docs".into(), "https://x.dev/a".into())]);
        assert_eq!(links("<https://x.dev>"), [("https://x.dev".into(), "https://x.dev".into())]);
        // A URL the parser splits at `_` or `[` is still one link.
        assert_eq!(links("see https://x.dev/a_b[1]"), [("https://x.dev/a_b[1]".into(), "https://x.dev/a_b[1]".into())]);
    }

    #[test]
    fn bare_url_bounds() {
        let found = |t: &str| bare_urls(t).into_iter().map(|r| t[r].to_owned()).collect::<Vec<_>>();
        assert_eq!(found("(see https://x.dev/a), then"), ["https://x.dev/a"]);
        assert_eq!(found("https://en.wikipedia.org/wiki/Rust_(language)!"), ["https://en.wikipedia.org/wiki/Rust_(language)"]);
        assert_eq!(found("\"http://a.b/c?d=1&e\" and https://f.g"), ["http://a.b/c?d=1&e", "https://f.g"]);
        assert!(found("xhttps://a.b https:// http").is_empty());
    }
}
