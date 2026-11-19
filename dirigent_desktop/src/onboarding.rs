use crate::ambient::{Pattern, Reach};
use crate::app::{Dirigent, Target};
use crate::cache::Cache;
use crate::hosts;
use crate::icons::icon;
use crate::lists::scroll;
use crate::projects::Group;
use crate::theme::{self, Theme, text, u};
use crate::{add_project, proxy, settings, titlebar};
use gpui::{
    Animation, AnimationExt, AnyElement, Context, Div, Font, FontWeight, Image, ImageFormat, Pixels, SharedString, Stateful, Window,
    actions, div, font, img, prelude::*, rgba,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::warn;

actions!(onboarding, [Continue]);

pub const KEY_CONTEXT: &str = "Onboarding";
const DONE_KEY: &str = "onboarded";
const COLUMN_W: f32 = 560.;
const TITLE_MS: f32 = 2250.;
const CURTAIN_MS: f32 = 1800.;
const STEP_MS: f32 = 520.;
const STEP_FROM: f32 = 36.;
const STEP_STAGGER: f32 = 70.;
const LEAVE_MS: f32 = 900.;

pub fn needed(cache: &Cache) -> bool {
    cache.get(DONE_KEY).is_none()
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Welcome,
    Look,
    Proxy,
    Project,
}

impl Step {
    const ALL: [Step; 4] = [Step::Welcome, Step::Look, Step::Proxy, Step::Project];

    fn next(self) -> Option<Step> {
        Self::ALL.get(self as usize + 1).copied()
    }

    fn prev(self) -> Option<Step> {
        (self as usize).checked_sub(1).map(|i| Self::ALL[i])
    }
}

pub struct State {
    pub step: Step,
    themes: Vec<(String, Theme)>,
    back: bool,
    titled: bool,
    pub leaving: Option<Instant>,
}

impl State {
    fn leave(&self) -> Option<f32> {
        self.leaving.map(|at| (at.elapsed().as_secs_f32() * 1000. / LEAVE_MS).min(1.))
    }
}

impl Dirigent {
    pub fn open_onboarding(&mut self, cx: &mut Context<Self>) {
        let themes = theme::available()
            .into_iter()
            .filter_map(|name| match theme::load(&name) {
                Ok(t) => Some((name, t)),
                Err(e) => {
                    warn!("{e:#}");
                    None
                }
            })
            .collect();
        self.onboarding = Some(State { step: Step::Welcome, themes, back: false, titled: false, leaving: None });
        self.focus_root(cx);
        cx.notify();
    }

    pub fn onboarding_taking_input(&self) -> bool {
        self.onboarding.as_ref().is_some_and(|s| s.leaving.is_none())
    }

    pub fn app_opacity(&self) -> f32 {
        match &self.onboarding {
            None => 1.,
            Some(s) => s.leave().map_or(0., |k| ease_out(span(k, 0.28, 0.72))),
        }
    }

    fn set_onboarding_step(&mut self, step: Step, cx: &mut Context<Self>) {
        let Some(s) = self.onboarding.as_mut().filter(|s| s.step != step && s.leaving.is_none()) else { return };
        s.back = step < s.step;
        s.titled |= s.step == Step::Welcome;
        s.step = step;
        self.leave_onboarding_step(cx);
        match step {
            Step::Proxy => self.enter_proxy_settings(cx),
            Step::Project if self.groups().0.is_empty() => self.open_add_project(cx),
            _ => self.focus_root(cx),
        }
        cx.notify();
    }

    fn leave_onboarding_step(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        if self.add_project.is_some() {
            self.close_add_project(cx);
        }
    }

    pub(crate) fn onboarding_next(&mut self, cx: &mut Context<Self>) {
        let Some(s) = self.onboarding.as_ref().filter(|s| s.leaving.is_none()) else { return };
        match s.step.next() {
            Some(step) => self.set_onboarding_step(step, cx),
            None => self.finish_onboarding(!self.groups().0.is_empty(), cx),
        }
    }

    pub fn onboarding_continue(&mut self, _: &Continue, _: &mut Window, cx: &mut Context<Self>) {
        self.onboarding_next(cx);
    }

    fn finish_onboarding(&mut self, start_thread: bool, cx: &mut Context<Self>) {
        if !self.onboarding_taking_input() {
            return;
        }
        if let Err(e) = self.cache.lock().unwrap().set(DONE_KEY, "1") {
            warn!("keep the setup as done: {e:#}");
        }
        if cx.reduce_motion() {
            self.onboarding = None;
        } else if let Some(s) = &mut self.onboarding {
            s.leaving = Some(Instant::now());
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(LEAVE_MS as u64)).await;
                this.update(cx, |this, cx| {
                    this.onboarding = None;
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        self.leave_onboarding_step(cx);
        if start_thread {
            let used = self.last_target().is_some_and(|t| self.target_valid(&t));
            let newest = || {
                let projects = self.hosts.iter().flat_map(|h| h.projects.iter().map(move |p| (h, p)));
                projects.max_by_key(|(_, p)| p.created_at).map(|(h, p)| Target { host: h.name.clone(), project_id: p.id.clone() })
            };
            let target = if used { None } else { newest() };
            self.new_draft(target, cx);
        } else {
            self.focus_root(cx);
        }
        cx.notify();
    }

    fn has_models(&self) -> bool {
        self.hosts.iter().any(|h| !h.models.is_empty())
    }
}

pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let Some(s) = &this.onboarding else { return div().into_any() };
    let skip = (s.step != Step::Welcome && s.step.next().is_some()).then(|| {
        quiet("onboarding-skip", "Skip setup").occlude().on_click(cx.listener(|this, _, _, cx| this.finish_onboarding(false, cx)))
    });
    let top = div()
        .relative()
        .flex_shrink_0()
        .h(u(titlebar::HEIGHT))
        .flex()
        .items_center()
        .justify_end()
        .px_3()
        .when(titlebar::CUSTOM, |d| d.child(titlebar::grab(div().absolute().inset_0(), cx)).pr(u(titlebar::CONTROLS_WIDTH)))
        .children(skip);
    let step = match s.step {
        Step::Welcome => welcome(this, s, cx),
        Step::Look => column("Choose a look", scroll("onboarding-look", look(this, s, cx)), this, s, cx),
        Step::Proxy => column("Connect your proxy", scroll("onboarding-proxy", proxy_step(this, cx)), this, s, cx),
        Step::Project => column("Add a project", project(this, cx), this, s, cx),
    };
    let body = div().flex_1().min_h_0().flex().flex_row().justify_center().px_8().child(step);
    let gone = s.leave().map_or(0., |k| smooth(span(k, 0., 0.45)));
    let lift = u(24. * gone);
    div()
        .id("onboarding")
        .occlude()
        .absolute()
        .inset_0()
        .flex()
        .flex_col()
        .child(top)
        .child(body)
        .when(gone > 0., |d| d.opacity(1. - gone).top(-lift).bottom(lift))
        .when(s.leaving.is_some(), |d| d.child(div().absolute().inset_0().occlude()))
        .into_any()
}

pub fn curtain(this: &Dirigent, layer: impl IntoElement + 'static) -> AnyElement {
    let edge = this.config.background.reach == Reach::Edge;
    let keep = if edge { 1. - this.app_opacity() } else { 1. };
    let curtain = Animation::new(ms(CURTAIN_MS));
    div()
        .absolute()
        .inset_0()
        .child(layer)
        .with_animation("onboarding-curtain", curtain, move |d, p| d.opacity(smooth(p) * keep))
        .into_any_element()
}

fn logo_svg() -> &'static str {
    if dirigent_protocol::release::nightly() {
        include_str!("../assets/dirigent_nightly.svg")
    } else {
        include_str!("../assets/dirigent.svg")
    }
}
const LOGO: f32 = 176.;

/// The mark as an image `size` px across. gpui draws an svg image at twice the size it
/// declares and scales that to the element, so it's declared at the size it shows at: the
/// file's 1000 px would be a 2000 px texture shrunk 20 times (no mipmaps: jagged). The last
/// one is kept: the size changes only with the text size.
fn logo(size: u32) -> Arc<Image> {
    static LAST: Mutex<Option<(u32, Arc<Image>)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    match &*last {
        Some((s, image)) if *s == size => image.clone(),
        _ => {
            let image = Arc::new(Image::from_bytes(ImageFormat::Svg, sized(logo_svg(), size).into_bytes()));
            *last = Some((size, image.clone()));
            image
        }
    }
}

fn sized(svg: &str, size: u32) -> String {
    let (tag, rest) = svg.split_at(svg.find('>').unwrap_or(0));
    let set = |tag: String, name: &str| match tag.find(&format!(" {name}=\"")) {
        Some(i) => {
            let from = i + name.len() + 3;
            let to = from + tag[from..].find('"').unwrap_or(0);
            format!("{}{size}{}", &tag[..from], &tag[to..])
        }
        None => tag,
    };
    set(set(tag.to_owned(), "width"), "height") + rest
}

fn welcome(this: &Dirigent, s: &State, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let (title_size, title_line, version_size, version_line) = (u(64.), u(72.), u(18.), u(24.));
    let card = !s.titled;
    let letters = "Dirigent".chars().enumerate().map(|(i, c)| {
        let letter = div().child(c.to_string());
        if !card {
            return letter.into_any_element();
        }
        let n = i as f32;
        letter
            .relative()
            .with_animation(("onboarding-letter", i), Animation::new(ms(TITLE_MS)), move |d, p| {
                let shown = ease_out(at(p, TITLE_MS, 350. + 80. * n, 650.));
                let spread = 1. - ease_out(at(p, TITLE_MS, 150., 1900.));
                d.opacity(shown).left(u(10. * n * spread)).top(u(10. * (1. - shown)))
            })
            .into_any_element()
    });
    let after = |d: Div, id: &'static str, from: f32, len: f32, rise: f32| {
        if !card {
            return d.into_any_element();
        }
        d.relative()
            .with_animation(id, Animation::new(ms(TITLE_MS)), move |d, p| {
                let shown = ease_out(at(p, TITLE_MS, from, len));
                d.opacity(shown).top(u(rise * (1. - shown)))
            })
            .into_any_element()
    };
    let set_up = primary("onboarding-start", "Set up").on_click(cx.listener(|this, _, _, cx| this.onboarding_next(cx)));
    let skip =
        quiet("onboarding-skip", "Fuck this, just let me in").on_click(cx.listener(|this, _, _, cx| this.finish_onboarding(false, cx)));
    let name = div()
        .flex()
        .flex_row()
        .text_size(title_size)
        .line_height(title_line)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgba(t.fg))
        .children(letters);
    // On the name's baseline. gpui's layout knows no baselines (`items_baseline` lines up the
    // boxes' bottoms), so the two line boxes start level and the version drops by the
    // difference of where each font size puts its baseline in its box: as a line is painted,
    // the leftover height split above and below ascent + descent. Not gpui's
    // `baseline_offset`, which takes the descent as given: negative on Linux.
    let baseline = |weight: FontWeight, size: Pixels, line: Pixels| {
        let ts = cx.text_system();
        let id = ts.resolve_font(&Font { weight, ..font(this.fonts.ui.clone()) });
        let (ascent, descent) = (ts.ascent(id, size), ts.descent(id, size).abs());
        (line - ascent - descent) / 2. + ascent
    };
    let drop = baseline(FontWeight::SEMIBOLD, title_size, title_line) - baseline(FontWeight::NORMAL, version_size, version_line);
    let version = div()
        .mt(drop)
        .text_size(version_size)
        .line_height(version_line)
        .text_color(rgba(t.fg_2))
        .child(dirigent_protocol::release::VERSION);
    let tagline = div().mt_4().text_size(u(16.)).text_color(rgba(t.fg_1)).child("Best way to combobulate your codebase");
    let buttons = div().mt_8().flex().flex_row().items_center().gap_3().child(set_up).child(skip);
    let mark = div().size(u(LOGO)).flex_shrink_0().child(img(logo((LOGO * theme::zoom()).round() as u32)).size_full());
    let words = div()
        .flex()
        .flex_col()
        .child(div().flex().flex_row().items_start().gap_3().child(name).child(after(version, "onboarding-version", 1250., 500., 0.)))
        .child(after(tagline, "onboarding-tagline", 1400., 600., 8.))
        .child(after(buttons, "onboarding-buttons", 1700., 550., 0.));
    let stack = div().flex().flex_row().items_center().gap(u(48.)).child(after(mark, "onboarding-mark", 0., 700., 14.)).child(words);
    div().flex_shrink_1().min_w_0().h_full().flex().flex_col().justify_center().pb(u(titlebar::HEIGHT)).child(if card {
        stack.into_any_element()
    } else {
        enter(stack, "onboarding-enter", s, 0.)
    })
}

fn enter(d: Div, id: &'static str, s: &State, delay: f32) -> AnyElement {
    let from = if s.back { -STEP_FROM } else { STEP_FROM };
    let total = STEP_MS + delay;
    d.relative()
        .with_animation((id, s.step as usize), Animation::new(ms(total)), move |d, p| {
            let shown = ease_out(at(p, total, delay, STEP_MS));
            d.opacity(shown).left(u(from * (1. - shown)))
        })
        .into_any_element()
}

fn ms(ms: f32) -> Duration {
    Duration::from_millis(ms as u64)
}

fn at(p: f32, total: f32, from: f32, len: f32) -> f32 {
    span(p, from / total, len / total)
}

fn span(k: f32, from: f32, len: f32) -> f32 {
    ((k - from) / len).clamp(0., 1.)
}

fn ease_out(x: f32) -> f32 {
    1. - (1. - x).powi(4)
}

fn smooth(x: f32) -> f32 {
    x * x * (3. - 2. * x)
}

fn column(title: &'static str, content: impl IntoElement, this: &Dirigent, s: &State, cx: &mut Context<Dirigent>) -> Div {
    let header = div().text_size(u(26.)).line_height(u(34.)).font_weight(FontWeight::SEMIBOLD).text_color(rgba(theme::t().fg)).child(title);
    let body = div().flex_1().min_h_0().mt_7().flex().flex_col().child(content);
    div()
        .w(u(COLUMN_W))
        .flex_shrink_1()
        .min_w_0()
        .h_full()
        .flex()
        .flex_col()
        .pt(u(56.))
        .pb_8()
        .child(enter(header, "onboarding-title", s, 0.))
        .child(enter(body, "onboarding-body", s, STEP_STAGGER))
        .child(footer(this, s.step, cx))
}

fn footer(this: &Dirigent, step: Step, cx: &mut Context<Dirigent>) -> Div {
    let back = step.prev().map(|prev| {
        quiet("onboarding-back", "Back").ml(-u(10.)).on_click(cx.listener(move |this, _, _, cx| this.set_onboarding_step(prev, cx)))
    });
    let note = match step {
        Step::Proxy if !this.has_models() => Some("Agents can't run until a host has a proxy with models."),
        Step::Project if this.groups().0.is_empty() => Some("Or add one later, from the sidebar."),
        _ => None,
    };
    let label = match step.next() {
        Some(_) => "Continue",
        None if this.groups().0.is_empty() => "Done",
        None => "Start a thread",
    };
    let next = primary("onboarding-next", label).on_click(cx.listener(|this, _, _, cx| this.onboarding_next(cx)));
    div()
        .flex_shrink_0()
        .pt_6()
        .flex()
        .flex_row()
        .items_center()
        .gap_4()
        .children(back)
        .child(div().flex_1().min_w_0().text_size(text::sm()).text_color(rgba(theme::t().fg_2)).children(note))
        .child(next)
}

fn look(this: &Dirigent, s: &State, cx: &mut Context<Dirigent>) -> Div {
    let swatches: Vec<_> = s.themes.iter().map(|(name, palette)| swatch(name, palette, *name == this.config.theme, cx)).collect();
    let current = this.config.background.pattern;
    let patterns = Pattern::ALL.map(|p| {
        segment(format!("onboarding-bg/{}", p.name()), p.name(), p == current)
            .on_click(cx.listener(move |this, _, _, cx| this.set_background(p, cx)))
    });
    let size = div().flex().flex_row().ml(-u(6.)).child(settings::text_size(this.config.fonts.size(), cx));
    div().flex().flex_col().gap_6().pb_2().child(labeled("Theme", div().flex().flex_row().flex_wrap().gap_3().children(swatches))).child(
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_x_8()
            .gap_y_6()
            .child(labeled("Background", div().flex().flex_row().gap_1().children(patterns)))
            .child(labeled("Text size", size)),
    )
}

fn proxy_step(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let about = div().text_color(rgba(theme::t().fg_1)).child(
        "Dirigent reaches its models through a CLIProxyAPI you run, with the accounts you signed in to there. Enter its \
         address and one of its API keys.",
    );
    div().flex().flex_col().gap_4().pb_2().child(about).child(proxy::body(this, cx))
}

fn swatch(name: &str, p: &Theme, selected: bool, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let t = theme::t();
    let bar = |w: f32, h: f32, color: u32| div().w(u(w)).h(u(h)).rounded_full().bg(rgba(color));
    let sidebar = div()
        .w(u(34.))
        .h_full()
        .flex()
        .flex_col()
        .gap(u(5.))
        .p(u(7.))
        .border_r_1()
        .border_color(rgba(p.line_soft))
        .child(bar(18., 3., p.fg_1))
        .child(bar(14., 3., p.fg_2))
        .child(bar(16., 3., p.fg_2));
    let pane = div()
        .flex_1()
        .h_full()
        .flex()
        .flex_col()
        .gap(u(5.))
        .p(u(8.))
        .bg(rgba(theme::alpha(p.panel, 0xff)))
        .child(bar(40., 4., p.fg))
        .child(bar(56., 3., p.fg_1))
        .child(bar(32., 3., p.fg_2))
        .child(div().flex_1().flex().items_end().justify_end().child(div().size(u(12.)).rounded_full().bg(rgba(p.accent))));
    let picture = div()
        .w_full()
        .h(u(58.))
        .flex()
        .flex_row()
        .rounded_md()
        .overflow_hidden()
        .border_1()
        .border_color(rgba(p.line))
        .bg(rgba(theme::alpha(p.canvas, 0xff)))
        .child(sidebar)
        .child(pane);
    let pick = name.to_owned();
    div()
        .id(SharedString::from(format!("onboarding-theme/{name}")))
        .w(u(124.))
        .flex()
        .flex_col()
        .gap_1p5()
        .cursor_pointer()
        .tooltip(theme::tooltip(name.to_owned()))
        .on_click(cx.listener(move |this, _, _, cx| this.set_theme(pick.clone(), cx)))
        .child(
            div()
                .p(u(3.))
                .rounded_lg()
                .border_2()
                .border_color(rgba(if selected { t.accent_line } else { theme::alpha(t.line, 0) }))
                .hover(move |d| if selected { d } else { d.border_color(rgba(t.line)) })
                .child(picture),
        )
        .child(
            div().px(u(3.)).truncate().text_size(text::sm()).text_color(rgba(if selected { t.fg } else { t.fg_1 })).child(name.to_owned()),
        )
}

fn project(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let groups = this.groups().0;
    let adding = this.add_project.as_ref().map(|s| {
        div()
            .flex_shrink_0()
            .rounded_lg()
            .border_1()
            .border_color(rgba(t.line))
            .bg(rgba(theme::alpha(t.raised, 0xff)))
            .child(add_project::page(this, s, cx))
    });
    let list = (!groups.is_empty()).then(|| {
        let title = if groups.len() == 1 { "Your project" } else { "Your projects" };
        labeled(title, div().flex().flex_col().children(groups.iter().map(project_row)))
    });
    let add = this.add_project.is_none().then(|| {
        let label = if groups.is_empty() { "Add a project" } else { "Add another" };
        div()
            .flex()
            .flex_row()
            .child(theme::button("onboarding-add-project", "plus", label).on_click(cx.listener(|this, _, _, cx| this.open_add_project(cx))))
    });
    let below = div().flex().flex_col().gap_4().children(list).children(add);
    div().flex_1().min_h_0().flex().flex_col().gap_6().children(adding).child(scroll("onboarding-projects", below))
}

fn project_row(g: &Group) -> Div {
    let t = theme::t();
    let m = &g.members[0];
    let hosts: Vec<&str> = g.members.iter().map(|m| m.host.as_str()).filter(|h| *h != hosts::LOCAL).collect();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(u(30.))
        .child(icon("folder", t.fg_2))
        .child(div().flex_shrink_0().font_weight(FontWeight::MEDIUM).child(g.name.clone()))
        .child(div().flex_1().min_w_0().truncate().text_size(text::sm()).text_color(rgba(t.fg_2)).child(m.project.path.clone()))
        .children((!hosts.is_empty()).then(|| div().flex_shrink_0().text_size(text::sm()).text_color(rgba(t.fg_2)).child(hosts.join(", "))))
}

fn labeled(label: &'static str, body: impl IntoElement) -> Div {
    let t = theme::t();
    div()
        .flex()
        .flex_col()
        .gap_2p5()
        .child(div().text_size(text::sm()).font_weight(FontWeight::MEDIUM).text_color(rgba(t.fg_1)).child(label))
        .child(body)
}

fn primary(id: &'static str, label: &'static str) -> Stateful<Div> {
    let t = theme::t();
    div()
        .id(id)
        .flex_shrink_0()
        .h(u(34.))
        .px(u(16.))
        .flex()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .bg(rgba(t.accent))
        .hover(move |d| d.bg(rgba(t.accent_active)))
        .text_color(rgba(t.fg))
        .font_weight(FontWeight::MEDIUM)
        .child(label)
}

fn quiet(id: &'static str, label: &'static str) -> Stateful<Div> {
    let t = theme::t();
    div()
        .id(id)
        .flex_shrink_0()
        .h(u(30.))
        .px_2p5()
        .flex()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .text_color(rgba(t.fg_1))
        .hover(move |d| d.bg(rgba(t.hover)).text_color(rgba(t.fg)))
        .child(label)
}

fn segment(id: String, label: &'static str, on: bool) -> Stateful<Div> {
    let t = theme::t();
    div()
        .id(SharedString::from(id))
        .h(u(28.))
        .px_3()
        .flex()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .border_1()
        .border_color(rgba(if on { t.accent_line } else { t.line }))
        .bg(rgba(if on { t.raised } else { theme::alpha(t.raised, 0) }))
        .text_size(text::sm())
        .text_color(rgba(if on { t.fg } else { t.fg_1 }))
        .hover(move |d| d.text_color(rgba(t.fg)))
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_is_sized() {
        assert!(sized(logo_svg(), 96).starts_with(r#"<svg width="96" height="96" viewBox="0 0 1000 1000""#));
    }
}
