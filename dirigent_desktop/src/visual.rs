use crate::theme::{self, Fonts, Theme};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{DocumentConfig, FontContext, LocalName, Node, QualName, UserAgentStylesheet, ns};
use blitz_html::HtmlDocument;
use blitz_traits::shell::{ColorScheme, Viewport};
use dirigent_protocol::visuals::WIDTH;
use gpui::{RenderImage, SharedString};
use parley::GenericFamily;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

const MAX_HEIGHT: f32 = 4096.;
const MAX_SOURCE: usize = 64 << 10;
const MAX_DEPTH: usize = 256;
const MAX_EXTENT: f32 = 16384.;
const STACK: usize = 32 << 20;

#[derive(Clone, PartialEq)]
pub struct Spec {
    scale: f32,
    theme: Theme,
    prose: SharedString,
    mono: SharedString,
}

impl Spec {
    pub fn new(window_scale: f32, fonts: &Fonts) -> Self {
        Self { scale: window_scale * theme::zoom(), theme: theme::t(), prose: fonts.prose.clone(), mono: fonts.mono.clone() }
    }

    fn colors(&self) -> [(&'static str, String); 9] {
        let t = &self.theme;
        [
            ("--fg", t.fg),
            ("--muted", t.fg_1),
            ("--border", t.line),
            ("--surface", t.raised),
            ("--accent", t.accent_ink),
            ("--info", t.working),
            ("--success", t.done),
            ("--warning", t.warn),
            ("--danger", t.error),
        ]
        .map(|(name, color)| (name, format!("#{color:08x}")))
    }

    fn stylesheet(&self) -> String {
        let colors: String = self.colors().iter().map(|(name, color)| format!("{name}: {color};")).collect();
        // Blitz draws a collapsed table's borders only when its first cell has a top border, so
        // the usual `border-bottom` under every row would draw nothing.
        format!(
            ":root {{ {colors} color: var(--fg); font: 14px/22px \"{}\", sans-serif; }}
            body {{ margin: 0; }}
            pre, code, kbd, samp {{ font-family: \"{}\", monospace; }}
            :link {{ color: var(--accent); }}
            table {{ border-collapse: separate !important; border-spacing: 0; }}",
            self.prose, self.mono
        )
    }

    fn scheme(&self) -> ColorScheme {
        let luma = |color: u32| {
            let [r, g, b, _] = color.to_be_bytes().map(u32::from);
            r * 299 + g * 587 + b * 114
        };
        if luma(self.theme.fg) > luma(self.theme.canvas) { ColorScheme::Dark } else { ColorScheme::Light }
    }

    /// Blitz hands every inline `<svg>` to usvg as a document of its own. usvg loads what an
    /// `href` names: a file (an `<image>` of `/dev/zero`, or of a share on another machine) or a
    /// `data:` image, which it decodes whatever its size. It knows no CSS variables, fills black
    /// what has no `fill`, and drops text whose font it can't find among the system's: its
    /// fallback is Times New Roman, and the app's own font is not a system one.
    fn prepare_svg(&self, doc: &mut HtmlDocument) {
        let named = |name: &str| QualName::new(None, ns!(), LocalName::from(name));
        let (fill, font_family) = (named("fill"), named("font-family"));
        let fonts = FONTS.sans.as_ref().map(|sans| format!("'{}', '{sans}'", self.prose));
        let colors = self.colors();
        let mut changes = vec![];
        let nested = doc.query_selector_all("svg svg").unwrap_or_default();
        for id in doc.query_selector_all("svg, svg *").unwrap_or_default() {
            let Some(node) = doc.get_node(id) else { continue };
            let root = node.data.is_element_with_tag_name(&LocalName::from("svg")) && !nested.contains(&id);
            for attr in node.attrs().unwrap_or_default() {
                if &*attr.name.local == "href" && !fragment(&attr.value) {
                    changes.push((id, attr.name.clone(), String::new()));
                } else if attr.value.contains("var(--") {
                    let value =
                        colors.iter().fold(attr.value.to_string(), |value, (name, color)| value.replace(&format!("var({name})"), color));
                    changes.push((id, attr.name.clone(), value));
                }
            }
            if root && node.attr(fill.local.clone()).is_none() {
                changes.push((id, fill.clone(), "currentColor".to_owned()));
            }
            match (node.attr(font_family.local.clone()), &fonts) {
                (Some(own), Some(fonts)) => changes.push((id, font_family.clone(), format!("{own}, {fonts}"))),
                (None, Some(fonts)) if root => changes.push((id, font_family.clone(), fonts.clone())),
                _ => {}
            }
        }
        let mut mutator = doc.mutate();
        for (id, name, value) in changes {
            mutator.set_attribute(id, name, &value);
        }
    }
}

/// usvg takes the `href` of an `<image>` for the path of a file, one that starts with `#` too
/// (`#/../x`), and its XML parser makes a `/` of an `&#47;`.
fn fragment(href: &str) -> bool {
    href.strip_prefix('#').is_some_and(|id| !id.contains(['/', '\\', '&']))
}

/// usvg parses the text Blitz writes an `<svg>` out as, and that text can hold an `href` the
/// document has not: Blitz writes the text of a `<plaintext>` in a `<title>` unescaped, markup
/// included, and an attribute `x:href` is an `href` to usvg when the source declares `x` as xlink.
fn svg_loads(doc: &HtmlDocument) -> bool {
    let kept = |value: &str| {
        let quote = value.chars().next().filter(|quote| ['"', '\''].contains(quote));
        let href = quote.and_then(|quote| value[1..].split(quote).next());
        href.is_some_and(|href| href.is_empty() || fragment(href))
    };
    let loads = |svg: &str| {
        svg.match_indices("href").any(|(at, _)| {
            let attribute = svg[..at].ends_with(|before: char| before.is_whitespace() || before == ':');
            let value = svg[at + 4..].trim_start().strip_prefix('=').map(str::trim_start);
            attribute && value.is_some_and(|value| !kept(value))
        })
    };
    let svgs = doc.query_selector_all("svg").unwrap_or_default();
    svgs.into_iter().filter_map(|id| doc.get_node(id)).any(|svg| loads(&svg.outer_html()))
}

struct SystemFonts {
    context: Mutex<FontContext>,
    sans: Option<String>,
}

static FONTS: LazyLock<SystemFonts> = LazyLock::new(|| {
    let mut context = FontContext::new();
    for face in crate::config::LILEX.iter().copied().chain([blitz_dom::BULLET_FONT]) {
        context.collection.register_fonts(face.to_vec().into(), None);
    }
    let sans = context.collection.generic_families(GenericFamily::SansSerif).next();
    let sans = sans.and_then(|id| context.collection.family_name(id).map(str::to_owned));
    SystemFonts { context: Mutex::new(context), sans }
});

#[derive(Clone)]
pub struct Picture {
    pub image: Arc<RenderImage>,
    pub aspect: f32,
}

pub fn draw(html: &str, spec: &Spec) -> Option<Picture> {
    if html.len() > MAX_SOURCE {
        return None;
    }
    let start = Instant::now();
    // Blitz is a beta fed whatever a model wrote. An `<ol start="0">` panics a debug build of it,
    // and it recurses into every level of the document: a level of nested grids takes 34 KB of
    // stack in a debug build, so 60 of them overflow the 2 MB of a thread.
    let thread = std::thread::Builder::new().stack_size(STACK);
    let picture = std::thread::scope(|scope| thread.spawn_scoped(scope, || paint(html, spec)).ok()?.join().ok().flatten());
    tracing::debug!(ms = start.elapsed().as_millis(), drawn = picture.is_some(), "drew a visual");
    picture
}

fn paint(html: &str, spec: &Spec) -> Option<Picture> {
    let width = (WIDTH as f32 * spec.scale).round() as u32;
    let config = DocumentConfig {
        viewport: Some(Viewport::new(width, 0, spec.scale, spec.scheme())),
        ua_stylesheets: vec![UserAgentStylesheet::default_sheet(), UserAgentStylesheet::parse(&spec.stylesheet())],
        font_ctx: Some(FONTS.context.lock().unwrap().clone()),
        // Blitz panics on a URL it can't resolve, and no relative one (`<img src="logo.png">`)
        // resolves against its default base, a `data:` URL.
        base_url: Some("http://visual.invalid/".to_owned()),
        ..Default::default()
    };
    let mut doc = HtmlDocument::from_html(html, config);
    let ancestors = |node: &Node| std::iter::successors(node.parent, |&id| doc.get_node(id)?.parent);
    if doc.tree().iter().any(|(_, node)| ancestors(node).nth(MAX_DEPTH).is_some()) {
        return None;
    }
    spec.prepare_svg(&mut doc);
    if svg_loads(&doc) {
        return None;
    }
    doc.resolve(0.);
    // Blitz draws a dashed, dotted or wavy line along its whole length, on the canvas or not: the
    // dotted border of a box 1e6 px wide takes 325 MB.
    let extents = |node: &Node| {
        let layout = node.final_layout();
        let reach = layout.scrollable_overflow_rect;
        [layout.size.width, layout.size.height, reach.right - reach.left, reach.bottom - reach.top]
    };
    let boxes = doc.tree().iter().filter(|(_, node)| node.element_data().is_some());
    if boxes.flat_map(|(_, node)| extents(node)).any(|extent| extent > MAX_EXTENT) {
        return None;
    }
    let height = (doc.root_element().scroll_height() * spec.scale).ceil().min(MAX_HEIGHT) as u32;
    if height == 0 {
        return None;
    }
    let scale = f64::from(spec.scale);
    let mut pixels = anyrender::render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| blitz_paint::paint_scene(scene, &mut doc, scale, width, height, 0, 0),
        width,
        height,
    );
    for px in pixels.as_chunks_mut::<4>().0 {
        gpui::swap_rgba_pa_to_bgra(px);
    }
    let frame = image::Frame::new(image::RgbaImage::from_raw(width, height, pixels)?);
    Some(Picture { image: Arc::new(RenderImage::new([frame])), aspect: width as f32 / height as f32 })
}

#[must_use]
pub struct StaleImage(pub Arc<RenderImage>);

#[derive(Default)]
pub struct Visuals {
    pub spec: Option<Spec>,
    asked: HashMap<SharedString, Spec>,
    pictures: HashMap<SharedString, Picture>,
    wanted: RefCell<Vec<SharedString>>,
}

impl Visuals {
    pub fn want(&self, html: &str) -> Option<Picture> {
        if self.asked.get(html) != self.spec.as_ref() {
            self.wanted.borrow_mut().push(html.to_owned().into());
        }
        self.pictures.get(html).cloned()
    }

    pub fn due(&mut self) -> Vec<(SharedString, Spec)> {
        let wanted = self.wanted.take();
        let Some(spec) = &self.spec else { return vec![] };
        let newly_asked = wanted.into_iter().filter(|html| self.asked.insert(html.clone(), spec.clone()).as_ref() != Some(spec));
        newly_asked.map(|html| (html, spec.clone())).collect()
    }

    pub fn show(&mut self, html: SharedString, spec: &Spec, picture: Option<Picture>) -> Option<StaleImage> {
        if self.asked.get(&html) != Some(spec) {
            return None;
        }
        self.pictures.insert(html, picture?).map(|replaced| StaleImage(replaced.image))
    }

    pub fn clear(&mut self) -> Vec<StaleImage> {
        self.asked.clear();
        self.pictures.drain().map(|(_, picture)| StaleImage(picture.image)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(scale: f32) -> Spec {
        Spec { scale, theme: theme::t(), prose: "Lilex".into(), mono: "Lilex".into() }
    }

    fn pixel(picture: &Picture, x: u32, y: u32) -> [u8; 4] {
        let width = picture.image.size(0).width.0 as usize;
        let at = (y as usize * width + x as usize) * 4;
        picture.image.as_bytes(0).unwrap()[at..at + 4].try_into().unwrap()
    }

    fn bgra(color: u32) -> [u8; 4] {
        let [r, g, b, a] = color.to_be_bytes();
        [b, g, r, a]
    }

    #[test]
    fn a_picture_is_the_canvas_wide_and_as_tall_as_its_content() {
        let picture = draw("<div style='height: 30px; background: #ff8000'></div>", &spec(2.)).unwrap();
        let size = picture.image.size(0);
        assert_eq!((size.width.0, size.height.0), (WIDTH as i32 * 2, 60));
        assert_eq!(picture.aspect, WIDTH as f32 / 30.);
        assert_eq!(pixel(&picture, 700, 30), [0x00, 0x80, 0xff, 0xff]);
        assert!(draw("", &spec(1.)).is_none());
        assert!(draw(&"<p>a</p>".repeat(MAX_SOURCE / 8 + 1), &spec(1.)).is_none());
    }

    #[test]
    fn content_past_its_box_is_in_the_picture() {
        let html = "<div style='position: relative'>
            <p style='position: absolute; top: 40px; margin: 0; width: 10px; height: 20px; background: #ff8000'></p></div>";
        let picture = draw(html, &spec(1.)).unwrap();
        assert_eq!(picture.image.size(0).height.0, 60);
        assert_eq!(pixel(&picture, 5, 50), [0x00, 0x80, 0xff, 0xff]);
    }

    #[test]
    fn the_theme_is_the_default_look() {
        let spec = spec(1.);
        let html = "<p style='margin: 0; width: 10px; height: 10px; background: var(--accent)'></p>
            <svg width='20' height='10' style='display: block'><rect width='10' height='10' fill='var(--danger)'/><rect x='10' width='10' height='10'/></svg>
            <table style='border-collapse: collapse'><tr><td style='border-bottom: 4px solid var(--border); padding: 0; width: 9px; height: 9px'></td></tr></table>";
        let picture = draw(html, &spec).unwrap();
        assert_eq!(pixel(&picture, 5, 5), bgra(spec.theme.accent_ink));
        assert_eq!(pixel(&picture, 20, 5)[3], 0);
        assert_eq!(pixel(&picture, 5, 15), bgra(spec.theme.error));
        assert_eq!(pixel(&picture, 15, 15), bgra(spec.theme.fg));
        let size = picture.image.size(0);
        assert_eq!(pixel(&picture, 5, size.height.0 as u32 - 2), bgra(spec.theme.line));

        let nested = "<svg width='20' height='10' style='display: block'><g fill='#ff8000'>
            <svg><rect id='r' width='10' height='10'/><use href='#r' x='10'/></svg></g></svg>";
        let picture = draw(nested, &spec).unwrap();
        assert_eq!(pixel(&picture, 5, 5), [0x00, 0x80, 0xff, 0xff]);
        assert_eq!(pixel(&picture, 15, 5), [0x00, 0x80, 0xff, 0xff]);
    }

    #[test]
    fn what_a_visual_cannot_load_leaves_the_rest_drawn() {
        let html = "<link rel='stylesheet' href='style.css'><img src='logo.png'>
            <p style='margin: 0; width: 10px; height: 10px; background: #ff8000'></p>";
        let picture = draw(html, &spec(1.)).unwrap();
        assert_eq!(pixel(&picture, 5, picture.image.size(0).height.0 as u32 - 5), [0x00, 0x80, 0xff, 0xff]);
    }

    #[test]
    fn a_line_far_longer_than_the_canvas_is_no_picture() {
        assert!(draw("<p style='border: 1px dotted'>a</p>", &spec(1.)).is_some());
        assert!(draw("<p style='border: 1px dotted; width: 1e5px'>a</p>", &spec(1.)).is_none());
        assert!(
            draw("<div style='overflow: hidden'><u style='text-decoration-style: wavy; letter-spacing: 1e5px'>ab</u></div>", &spec(1.))
                .is_none()
        );
        assert!(draw("<p style='position: absolute; left: -9999px'>a</p>b", &spec(1.)).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_visual_opens_no_file() {
        let fifo = std::env::temp_dir().join(format!("dirigent-visual-{}", std::process::id()));
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let file = fifo.display();
        let image = format!("<image href='{file}' width='10' height='10'/>");
        let sources = [
            format!(
                "<svg width='20' height='10'>{image}<rect data-href='x' width='10' height='10'/><filter><feImage xlink:href=' {file}'/></filter></svg>"
            ),
            format!(
                "<svg xmlns:x='http://www.w3.org/1999/xlink' width='20' height='10'><image x:href='{file}' width='10' height='10'/></svg>"
            ),
            format!("<svg width='20' height='10'><title><plaintext></plaintext></title>{image}<title><plaintext>"),
            "<svg width='20' height='10'><title><plaintext></plaintext></title><image href='#/../x'/><title><plaintext>".to_owned(),
        ];
        let drawn = sources.map(|html| {
            let (drawn, picture) = std::sync::mpsc::channel();
            std::thread::spawn(move || drawn.send(draw(&html, &spec(1.)).is_some()));
            picture.recv_timeout(std::time::Duration::from_secs(10)).ok()
        });
        std::fs::remove_file(&fifo).unwrap();
        assert_eq!(drawn, [Some(true), Some(false), Some(false), Some(false)]);
    }

    #[test]
    fn nesting_blitz_would_overflow_its_stack_on_is_no_picture() {
        let nested = |open: &str, levels: usize| format!("{}x{}", open.repeat(levels), "</div>".repeat(levels));
        let grid = "<div style='display: grid'>";
        assert!(draw(&nested(grid, 200), &spec(1.)).is_some());
        assert!(draw(&nested(grid, 1500), &spec(1.)).is_none());
    }

    #[test]
    fn a_document_blitz_panics_on_is_no_picture() {
        let picture = draw("<ol start='0'><li>one</li></ol>", &spec(1.));
        assert!(picture.is_none() || !cfg!(debug_assertions));
    }

    #[test]
    fn a_picture_is_drawn_once_per_spec() {
        let mut visuals = Visuals::default();
        assert!(visuals.want("<p>a</p>").is_none());
        assert!(visuals.due().is_empty());

        visuals.spec = Some(spec(1.));
        visuals.want("<p>a</p>");
        visuals.want("<p>a</p>");
        let due = visuals.due();
        assert_eq!(due.len(), 1);
        visuals.want("<p>a</p>");
        assert!(visuals.due().is_empty());

        let (html, first) = due.into_iter().next().unwrap();
        let drawn = draw(&html, &first).unwrap();
        assert!(visuals.show(html.clone(), &first, Some(drawn.clone())).is_none());
        assert!(Arc::ptr_eq(&visuals.want(&html).unwrap().image, &drawn.image));

        visuals.spec = Some(spec(2.));
        assert!(Arc::ptr_eq(&visuals.want(&html).unwrap().image, &drawn.image));
        let (_, second) = visuals.due().into_iter().next().unwrap();
        assert!(visuals.show(html.clone(), &first, draw(&html, &first)).is_none());
        let replaced = visuals.show(html.clone(), &second, draw(&html, &second)).unwrap();
        assert!(Arc::ptr_eq(&replaced.0, &drawn.image));
        assert_eq!(visuals.clear().len(), 1);
        assert!(visuals.want(&html).is_none());
    }
}
