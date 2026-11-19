use crate::select::{Block, Key, Keys};
use crate::theme::{self, text, u};
use dirigent_protocol::images::image_targets;
use dirigent_protocol::visuals::FENCE;
use gpui::{
    AnyElement, Div, ElementId, FontStyle, FontWeight, HighlightStyle, SharedString, StrikethroughStyle, StyledText, UnderlineStyle, div,
    prelude::*, px, rgba,
};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd, TextMergeStream};
use std::collections::BTreeMap;
use std::iter::Peekable;
use std::ops::Range;

pub type ImageFn<'a> = Option<&'a mut (dyn FnMut(&str) -> AnyElement + 'a)>;

pub type VisualFn<'a> = Option<&'a mut (dyn FnMut(&str) -> Option<AnyElement> + 'a)>;

pub type CopyFn<'a> = Option<&'a dyn Fn(ElementId, SharedString) -> AnyElement>;

pub const CODE_GROUP: &str = "code";

pub type Tokens<'a> = &'a [(String, SharedString)];

const OPTS: Options = Options::ENABLE_TABLES.union(Options::ENABLE_STRIKETHROUGH).union(Options::ENABLE_TASKLISTS);

pub fn render<'a>(
    text: &str,
    mono: &SharedString,
    keys: &mut Keys,
    image: ImageFn<'a>,
    visual: VisualFn<'a>,
    copy: CopyFn,
    tokens: Tokens,
) -> Div {
    // Merged so a bare URL (or a token) the parser split into several text events is found whole.
    let mut events = TextMergeStream::new(Parser::new_ext(text, OPTS)).peekable();
    let open_fence = visual.as_ref().and_then(|_| open_fence(text));
    let mut r = Renderer { mono, keys, image, visual, open_fence, fences: 0, copy, tokens };
    div().flex().flex_col().gap_2().children(r.blocks(&mut events))
}

pub fn link_blobs(text: &str, blobs: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut done = 0;
    for (event, source) in Parser::new_ext(text, OPTS).into_offset_iter() {
        let Event::Start(Tag::Image { .. }) = event else { continue };
        // The image's source is its `![alt](target)`.
        let Some(target) = image_targets(&text[source.clone()]).into_iter().next() else { continue };
        let target = source.start + target.start..source.start + target.end;
        let already_replaced = target.start < done;
        if !already_replaced && let Some(id) = blobs.get(&text[target.clone()]) {
            // MUST KILL: rearchitect: link_blobs' trim_end serves blob_links matching "](blob:"; share one blob link format
            out.push_str(text[done..target.start].trim_end());
            out.push_str("blob:");
            out.push_str(id);
            done = target.end;
        }
    }
    out.push_str(&text[done..]);
    out
}

pub fn blob_links(text: &str) -> impl Iterator<Item = &str> {
    text.match_indices("](blob:").filter_map(|(i, m)| {
        let rest = &text[i + m.len()..];
        let n = rest.bytes().take_while(u8::is_ascii_hexdigit).count();
        (n == 64).then(|| &rest[..n])
    })
}

fn open_fence(text: &str) -> Option<usize> {
    let fenced = |(event, source)| matches!(event, Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_)))).then_some(source);
    let (ix, last) = Parser::new_ext(text, OPTS).into_offset_iter().filter_map(fenced).enumerate().last()?;
    let block: &str = text[last].trim_end();
    let mark = block.chars().next()?;
    let opening = block.len() - block.trim_start_matches(mark).len();
    let closing = block.rsplit_once('\n').map_or("", |(_, line)| line.trim_start_matches([' ', '\t', '>']));
    let closed = closing.len() >= opening && closing.chars().all(|c| c == mark);
    (!closed).then_some(ix)
}

pub fn starts_with_code(text: &str) -> bool {
    let container = |e: &Event| matches!(e, Event::Start(Tag::List(_) | Tag::Item | Tag::BlockQuote(_)));
    matches!(Parser::new_ext(text, OPTS).find(|e| !container(e)), Some(Event::Start(Tag::CodeBlock(_) | Tag::HtmlBlock)))
}

pub fn committed(text: &str) -> &str {
    text.rfind('\n').map_or("", |i| &text[..=i])
}

type Events<'a> = Peekable<TextMergeStream<'a, Parser<'a>>>;

struct Renderer<'m, 'i> {
    mono: &'m SharedString,
    keys: &'m mut Keys,
    image: ImageFn<'i>,
    visual: VisualFn<'i>,
    open_fence: Option<usize>,
    fences: usize,
    copy: CopyFn<'m>,
    tokens: Tokens<'m>,
}

impl<'m> Renderer<'m, '_> {
    fn blocks(&mut self, ev: &mut Events) -> Vec<AnyElement> {
        let mut out = vec![];
        while let Some(e) = ev.next() {
            match e {
                Event::End(_) => break,
                Event::Start(tag) if !is_inline(&tag) => out.push(self.block(tag, ev)),
                Event::Rule => out.push(div().h(px(1.)).w_full().my_1().bg(rgba(theme::t().line)).into_any()),
                inline => {
                    let mut p = Inline::new(self.image.is_some(), self.tokens);
                    let depth = matches!(inline, Event::Start(_)) as usize;
                    p.event(inline, self.mono);
                    self.inline_until_block(&mut p, ev, depth);
                    out.push(self.finish(p));
                }
            }
        }
        out
    }

    fn block(&mut self, tag: Tag, ev: &mut Events) -> AnyElement {
        match tag {
            Tag::Paragraph => self.paragraph(ev),
            Tag::Heading { level, .. } => {
                let size = match level as u8 {
                    1 => 20.,
                    2 => 17.,
                    _ => 15.,
                };
                div().mt_1().text_size(u(size)).font_weight(FontWeight::SEMIBOLD).child(self.paragraph(ev)).into_any()
            }
            Tag::CodeBlock(kind) => {
                let mut code = String::new();
                for e in ev.by_ref() {
                    match e {
                        Event::Text(t) => code.push_str(&t),
                        Event::End(TagEnd::CodeBlock) => break,
                        _ => {}
                    }
                }
                let code = code.trim_end();
                let visual = match &kind {
                    CodeBlockKind::Fenced(info) => {
                        let being_written = self.open_fence == Some(self.fences);
                        self.fences += 1;
                        !being_written && info.as_ref() == FENCE
                    }
                    CodeBlockKind::Indented => false,
                };
                let draw = self.visual.as_deref_mut().filter(|_| visual);
                draw.and_then(|draw| draw(code)).unwrap_or_else(|| self.code_block(code.to_owned()).into_any())
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
            _ => div().flex().flex_col().gap_2().children(self.blocks(ev)).into_any(),
        }
    }

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
                            .child(div().flex_shrink_0().min_w(u(16.)).text_color(rgba(theme::t().fg_1)).child(marker))
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

    fn paragraph(&mut self, ev: &mut Events) -> AnyElement {
        let mut p = Inline::new(self.image.is_some(), self.tokens);
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

    fn finish(&mut self, mut p: Inline<'m>) -> AnyElement {
        let images = std::mem::take(&mut p.images);
        if images.is_empty() {
            return p.block(self.keys).into_any();
        }
        let text = (!p.text.is_empty()).then(|| p.block(self.keys));
        let draw = self.image.as_deref_mut().expect("images are only collected with an ImageFn");
        div().flex().flex_col().gap_2().children(text).children(images.iter().map(|id| draw(id))).into_any()
    }

    fn inline_until_block(&mut self, p: &mut Inline<'m>, ev: &mut Events, mut depth: usize) {
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
        let code = SharedString::from(code);
        let block = self.keys.plain(code.clone());
        let copy = self.copy.map(|copy| {
            let Key { doc, item, seq } = block.key();
            let id = ElementId::Name(format!("code-copy/{doc:?}/{item}/{seq}").into());
            div().absolute().top_1().right_1().child(copy(id, code))
        });
        div()
            .group(CODE_GROUP)
            .relative()
            .w_full()
            .rounded_md()
            .bg(rgba(theme::t().raised))
            .border_1()
            .border_color(rgba(theme::t().line_soft))
            .px_3()
            .py_2()
            .font_family(self.mono.clone())
            .text_size(text::sm())
            .line_height(u(19.))
            .child(block)
            .children(copy)
    }
}

fn is_inline(t: &Tag) -> bool {
    matches!(t, Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } | Tag::Image { .. } | Tag::Superscript | Tag::Subscript)
}

#[derive(Default)]
struct Inline<'t> {
    tokens: Tokens<'t>,
    text: String,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    code_ranges: Vec<(Range<usize>, SharedString)>,
    links: Vec<(Range<usize>, SharedString)>,
    open_links: Vec<(usize, Option<SharedString>)>,
    bold: u32,
    italic: u32,
    strike: u32,
    link: u32,
    token: bool,
    show_images: bool,
    images: Vec<String>,
    in_collected_image_alt: u32,
}

impl<'t> Inline<'t> {
    fn new(show_images: bool, tokens: Tokens<'t>) -> Self {
        Self { show_images, tokens, ..Default::default() }
    }

    fn event(&mut self, e: Event, mono: &SharedString) {
        match e {
            Event::Start(Tag::Image { dest_url, .. }) if self.show_images && dest_url.starts_with("blob:") => {
                self.images.push(dest_url["blob:".len()..].to_owned());
                self.in_collected_image_alt += 1;
            }
            Event::End(TagEnd::Image) if self.in_collected_image_alt > 0 => self.in_collected_image_alt -= 1,
            Event::Text(t) if self.link == 0 => self.text_with_links(&t),
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

    fn text_with_links(&mut self, t: &str) {
        let mut spans: Vec<_> = bare_urls(t).into_iter().map(|r| (r.clone(), SharedString::from(t[r].to_owned()), false)).collect();
        for (token, url) in self.tokens {
            for (i, _) in t.match_indices(token.as_str()) {
                let r = i..i + token.len();
                if !spans.iter().any(|(s, ..)| s.start < r.end && r.start < s.end) {
                    spans.push((r, url.clone(), true));
                }
            }
        }
        spans.sort_by_key(|(r, ..)| r.start);
        let mut at = 0;
        for (r, url, token) in spans {
            self.push(&t[at..r.start], None);
            let start = self.text.len();
            self.link += 1;
            self.token = token;
            self.push(&t[r.clone()], None);
            self.link -= 1;
            self.token = false;
            self.links.push((start..self.text.len(), url));
            at = r.end;
        }
        self.push(&t[at..], None);
    }

    fn push(&mut self, s: &str, code: Option<&SharedString>) {
        if s.is_empty() || self.in_collected_image_alt > 0 {
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
            if !self.token {
                h.underline = Some(UnderlineStyle { thickness: px(1.), color: None, wavy: false });
            }
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

fn has_scheme(url: &str) -> bool {
    url.split_once(':').is_some_and(|(s, _)| {
        s.starts_with(|c: char| c.is_ascii_alphabetic()) && s.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
    })
}

const BARE_SCHEMES: [&str; 3] = ["http", "https", "dirigent"];

fn bare_urls(text: &str) -> Vec<Range<usize>> {
    let mut out = vec![];
    let mut i = 0;
    while let Some(sep) = text[i..].find("://").map(|a| a + i) {
        i = sep + 3;
        let at = text[..sep].trim_end_matches(char::is_alphanumeric).len();
        if !BARE_SCHEMES.contains(&&text[at..sep]) {
            continue;
        }
        let rest = &text[at..];
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
        if end > sep + 3 {
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
    fn images_link_their_blobs() {
        let text = "![a](shot.png) ![b]( /abs/b.png \"t\") ![c](other.png)\n\n```\n![a](shot.png)\n```\n\n`![a](shot.png)`";
        let blobs = [("shot.png", "aa"), ("/abs/b.png", "bb")].map(|(target, id)| (target.to_owned(), id.to_owned())).into();
        assert_eq!(
            link_blobs(text, &blobs),
            "![a](blob:aa) ![b](blob:bb \"t\") ![c](other.png)\n\n```\n![a](shot.png)\n```\n\n`![a](shot.png)`"
        );
        assert_eq!(link_blobs("![![a](shot.png)](other.png)", &blobs), "![![a](blob:aa)](other.png)");
    }

    #[test]
    fn blob_images_leave_the_text() {
        let mono = SharedString::from("Mono");
        let id = "b".repeat(64);
        let src = format!("see ![the home page](blob:{id}) and ![alt](https://x/y.png)");
        let mut p = Inline::new(true, &[]);
        for e in Parser::new(&src) {
            p.event(e, &mono);
        }
        assert_eq!(p.text, "see  and alt");
        assert_eq!(p.images, vec![id]);
        let mut p = Inline::new(false, &[]);
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
        render("- **Label:** text\n- [a](https://x.dev) *b* c\n", &"Mono".into(), &mut keys, None, None, None, &[]);
        assert_eq!(sel.texts(), ["Label: text", "a b c"]);
    }

    #[test]
    fn code_blocks_copy_what_they_show() {
        let sel = crate::select::Selection::default();
        let mut keys = Keys::new(&sel, crate::select::Doc::Thread, 0);
        let made = std::cell::RefCell::new(vec![]);
        let copy = |id: ElementId, code: SharedString| {
            made.borrow_mut().push((id, code));
            div().into_any()
        };
        render("```sh\nls -la  \n```\n\ntext\n\n```\nexit 0\n```\n", &"Mono".into(), &mut keys, None, None, Some(&copy), &[]);
        let made = made.into_inner();
        assert_eq!(made.iter().map(|(_, code)| code.to_string()).collect::<Vec<_>>(), ["ls -la", "exit 0"]);
        assert_ne!(made[0].0, made[1].0);
        assert_eq!(sel.texts(), ["ls -la", "text", "exit 0"]);
    }

    #[test]
    fn visual_blocks_are_drawn_or_stay_code() {
        let md = "```visual\n<b>a</b>\n```\n\n```visual\n<i>b</i>\n```\n\n```html\n<u>c</u>\n```\n";
        let sel = crate::select::Selection::default();
        let mut keys = Keys::new(&sel, crate::select::Doc::Thread, 0);
        let mut asked = vec![];
        let mut draw = |html: &str| {
            asked.push(html.to_owned());
            (html == "<b>a</b>").then(|| div().into_any())
        };
        render(md, &"Mono".into(), &mut keys, None, Some(&mut draw), None, &[]);
        assert_eq!(asked, ["<b>a</b>", "<i>b</i>"]);
        assert_eq!(sel.texts(), ["<i>b</i>", "<u>c</u>"]);

        let sel = crate::select::Selection::default();
        let mut keys = Keys::new(&sel, crate::select::Doc::Thread, 0);
        render(md, &"Mono".into(), &mut keys, None, None, None, &[]);
        assert_eq!(sel.texts(), ["<b>a</b>", "<i>b</i>", "<u>c</u>"]);
    }

    #[test]
    fn a_visual_is_drawn_once_its_fence_closes() {
        let drawn = |md: &str| {
            let sel = crate::select::Selection::default();
            let mut keys = Keys::new(&sel, crate::select::Doc::Thread, 0);
            let mut asked = vec![];
            let mut draw = |html: &str| {
                asked.push(html.to_owned());
                Some(div().into_any())
            };
            render(md, &"Mono".into(), &mut keys, None, Some(&mut draw), None, &[]);
            asked
        };
        assert!(drawn("```visual\n<b>a</b>\n").is_empty());
        assert_eq!(drawn("```visual\n<b>a</b>\n```\n"), ["<b>a</b>"]);
        assert_eq!(drawn("```visual\n<b>a</b>\n```\n\ntext\n\n~~~visual\n<i>b"), ["<b>a</b>"]);
        assert_eq!(drawn("- one\n  ```visual\n  <b>a</b>\n  ```\n- two\n\n> ~~~visual\n> <i>b</i>\n> ~~~\n"), ["<b>a</b>", "<i>b</i>"]);
        assert_eq!(drawn("- one\n  ```visual\n  <b>a</b>\n  ```\n- two\n\n```visual\n<i>b"), ["<b>a</b>"]);
        assert!(drawn("````visual\n<b>a</b>\n```\n").is_empty());
        assert!(drawn("~~~visual\n<b>a</b>\n```\n").is_empty());
    }

    #[test]
    fn opening_code_block() {
        assert!(starts_with_code("```sh\nls\n```\nthen"));
        assert!(starts_with_code("- ```\n  ls\n  ```\n"));
        assert!(!starts_with_code("Run:\n```\nls\n```\n"));
        assert!(!starts_with_code("`ls` inline"));
    }

    fn links(md: &str) -> Vec<(String, String)> {
        token_links(md, &[])
    }

    fn token_links(md: &str, tokens: Tokens) -> Vec<(String, String)> {
        let mono = SharedString::from("Mono");
        let mut p = Inline::new(false, tokens);
        for e in TextMergeStream::new(Parser::new(md)) {
            p.event(e, &mono);
        }
        p.links.iter().map(|(r, url)| (p.text[r.clone()].to_owned(), url.to_string())).collect()
    }

    #[test]
    fn tokens_link_in_plain_text_only() {
        let tokens = [("[image-1]".to_owned(), SharedString::from("dirigent://image/a"))];
        let md = "[image-1] then https://x.dev, `[image-1]` and [image-1]";
        let found = token_links(md, &tokens);
        let (img, url) = (("[image-1]".to_owned(), "dirigent://image/a".to_owned()), "https://x.dev".to_owned());
        assert_eq!(found, [img.clone(), (url.clone(), url), img]);
    }

    #[test]
    fn links_in_text() {
        let issue = "https://forge.sebba.dev/izvir/dirigent/issues/11";
        assert_eq!(links(&format!("I created issue #11: {issue}.")), [(issue.to_owned(), issue.to_owned())]);
        assert_eq!(links("[docs](https://x.dev/a) and [main](src/main.rs)"), [("docs".into(), "https://x.dev/a".into())]);
        assert_eq!(links("<https://x.dev>"), [("https://x.dev".into(), "https://x.dev".into())]);
        assert_eq!(links("see https://x.dev/a_b[1]"), [("https://x.dev/a_b[1]".into(), "https://x.dev/a_b[1]".into())]);
        let thread = "dirigent://thread/01a168ea-f693-737d-b2a3-60cefe482df3";
        assert_eq!(links(&format!("[fix the ruler]({thread})")), [("fix the ruler".into(), thread.into())]);
        assert_eq!(links(&format!("Spawned ({thread}).")), [(thread.into(), thread.into())]);
    }

    #[test]
    fn bare_url_bounds() {
        let found = |t: &str| bare_urls(t).into_iter().map(|r| t[r].to_owned()).collect::<Vec<_>>();
        assert_eq!(found("(see https://x.dev/a), then"), ["https://x.dev/a"]);
        assert_eq!(found("https://en.wikipedia.org/wiki/Rust_(language)!"), ["https://en.wikipedia.org/wiki/Rust_(language)"]);
        assert_eq!(found("\"http://a.b/c?d=1&e\" and https://f.g"), ["http://a.b/c?d=1&e", "https://f.g"]);
        assert!(found("xhttps://a.b https:// http ftp://a.b").is_empty());
    }
}
