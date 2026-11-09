//! The Settings modal, behind the sidebar's gear: tabs down the left, the open tab's
//! sections on the right. General, Thread titles and Agent tools are the UI over
//! `config.toml` (`config.rs`); Hosts over `hosts.toml`, Providers over a host's pi
//! credentials and Forge providers over the forge sign-in have their own flows
//! (`hosts_tab.rs`, `providers.rs`, `account.rs`). Everything else saves
//! as it is changed: the text fields (fonts, title prompt) 300ms after the last keystroke,
//! with a warning mark on a font field whose family isn't installed; theme, text size,
//! background, title model, reasoning, the agent tools and telemetry are chips, a stepper
//! and switches that save at once. `Test` opens a sample first message, runs it through
//! the title settings on the local host, and shows the title with what it cost.

use crate::ambient::{Pattern, Reach};
use crate::app::{Dirigent, Menu};
use crate::hosts_tab;
use crate::icons::icon;
use crate::input::{self, TextInput};
use crate::lists::scroll;
use crate::menu::{self, Side};
use crate::modal;
use crate::net::HostStatus;
use crate::theme::{self, dollars, kilo, text, u};
use crate::thread::duration;
use crate::{config, hosts};
use gpui::{AnyElement, Context, Div, Entity, FontWeight, SharedString, Stateful, div, prelude::*, rgba};
use std::path::Path;
use std::time::Instant;

/// The tab list's width.
const NAV_W: f32 = 180.;

/// The modal's tabs, in nav order.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    General,
    Providers,
    Titles,
    Tools,
    Hosts,
    /// The forge sign-in (`account.rs`).
    Forge,
}

impl Tab {
    const ALL: [Tab; 6] = [Tab::General, Tab::Providers, Tab::Titles, Tab::Tools, Tab::Hosts, Tab::Forge];

    fn label(self) -> &'static str {
        match self {
            Tab::General => "General",
            Tab::Providers => "Providers",
            Tab::Titles => "Thread titles",
            Tab::Tools => "Agent tools",
            Tab::Hosts => "Hosts",
            Tab::Forge => "Forge providers",
        }
    }
}

/// The sample first message `Test` starts with.
const SAMPLE: &str = "The websocket client in net.rs reconnects with a fixed 1s delay, which hammers the service when a host is \
down for a while. Switch it to exponential backoff starting at 1s and capped at 30s, reset the delay once a connection \
succeeds, and make sure the status shown in the sidebar still flips to connecting on every attempt. Add a unit test for \
the backoff schedule.";

/// A finished title test (see `ServerMessage::Title`).
#[derive(Clone)]
pub struct TitleResult {
    pub title: String,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cost: f64,
    pub duration_ms: i64,
}

/// The open tab (the gear reopens the last one), the inputs and the title test's state.
#[derive(Clone)]
pub struct State {
    pub tab: Tab,
    pub font_ui: Entity<TextInput>,
    pub font_prose: Entity<TextInput>,
    pub font_code: Entity<TextInput>,
    pub prompt: Entity<TextInput>,
    pub sample: Entity<TextInput>,
    /// The Test form (sample + result) is open.
    pub testing: bool,
    /// Request id and start time of the test in flight.
    pub test_request: Option<(u64, Instant)>,
    pub test_result: Option<Result<TitleResult, String>>,
    /// Font families in the config that fontconfig doesn't know (a warning on their field).
    pub missing_fonts: Vec<String>,
}

impl State {
    pub fn new(cx: &mut Context<Dirigent>) -> Self {
        let field = |cx: &mut Context<Dirigent>, placeholder: &str, single: bool| cx.new(|cx| TextInput::new(cx, placeholder, single));
        Self {
            tab: Tab::default(),
            font_ui: field(cx, "Font family", true),
            font_prose: field(cx, "Font family", true),
            font_code: field(cx, "Font family", true),
            prompt: field(cx, "System prompt for the title model", false),
            sample: field(cx, "A first message to name", false),
            testing: false,
            test_request: None,
            test_result: None,
            missing_fonts: vec![],
        }
    }

    /// Put the config's values into the fields (on open, and when the file changes).
    pub fn fill(&self, config: &config::Config, window: &gpui::Window, cx: &mut gpui::App) {
        for (input, text) in [
            (&self.font_ui, &config.fonts.ui),
            (&self.font_prose, &config.fonts.prose),
            (&self.font_code, &config.fonts.code),
            (&self.prompt, &config.title.prompt),
        ] {
            input.update(cx, |input, cx| {
                if input.text() != text {
                    input.set_text(text, window, cx);
                }
            });
        }
        self.sample.update(cx, |input, cx| {
            if input.text().is_empty() {
                input.set_text(SAMPLE, window, cx);
            }
        });
    }
}

/// The modal: the tab list on the left (on the canvas, like the app's sidebar), the open
/// tab's sections (scrolling) on the right.
pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let t = theme::t();
    let open = this.settings.tab;
    let nav = div()
        .w(u(NAV_W))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap_0p5()
        .p_2()
        .rounded_l_lg()
        .bg(rgba(t.canvas))
        .border_r_1()
        .border_color(rgba(t.line_soft))
        .child(div().h(u(32.)).px_2().flex().items_center().font_weight(FontWeight::MEDIUM).child("Settings"))
        .children(Tab::ALL.map(|tab| nav_item(tab, tab == open, cx)));
    let sections = match open {
        Tab::General => vec![appearance(this, cx), telemetry(this, cx), saved_to(&config::path())],
        Tab::Providers => crate::providers::render(this, cx),
        Tab::Titles => vec![titles(this, cx), saved_to(&config::path())],
        Tab::Tools => vec![tools(this, cx), saved_to(&config::path())],
        Tab::Hosts => hosts_tab::render(this, cx),
        Tab::Forge => crate::account::render(this, cx),
    };
    let column = div().flex().flex_col().gap_8().px_8().py_6().children(sections);
    let body = div().flex().flex_row().child(nav).child(div().flex_1().min_w_0().flex().flex_col().child(scroll("settings", column)));
    modal::frame(modal::large(body), cx)
}

/// A tab in the list: the open one bright on the hover colour.
fn nav_item(tab: Tab, on: bool, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let t = theme::t();
    div()
        .id(SharedString::from(format!("settings-tab/{}", tab.label())))
        .h(u(28.))
        .px_2()
        .flex()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .text_color(rgba(if on { t.fg } else { t.fg_1 }))
        .when(on, |d| d.bg(rgba(t.hover)))
        .hover(|d| d.bg(rgba(t.hover)).text_color(rgba(t.fg)))
        .on_click(cx.listener(move |this, _, _, cx| this.set_settings_tab(tab, cx)))
        .child(tab.label())
}

/// A section: title over its content.
pub fn section(title: &'static str, body: impl IntoElement) -> Div {
    div().flex().flex_col().gap_2().child(div().font_weight(FontWeight::MEDIUM).text_color(rgba(theme::t().fg)).child(title)).child(body)
}

/// The file a tab's settings live in (also editable by hand), in small print at its bottom.
pub fn saved_to(path: &Path) -> Div {
    div().text_size(text::sm()).text_color(rgba(theme::t().fg_2)).child(format!("Saved to {}", path.display()))
}

/// `label        control` on one line.
fn row(label: &'static str, control: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .min_h(u(28.))
        .py_2()
        .child(div().w(u(130.)).flex_shrink_0().text_color(rgba(theme::t().fg_1)).child(label))
        .child(div().flex_1().min_w_0().flex().flex_row().items_center().justify_end().child(control))
}

/// An option of the row above it: indented, its label in small print.
fn sub_row(label: &'static str, control: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .min_h(u(28.))
        .pb_2()
        .pl(u(16.))
        .child(div().flex_1().min_w_0().text_size(text::sm()).text_color(rgba(theme::t().fg_2)).child(label))
        .child(control)
}

/// A bordered text field.
pub fn field(input: Entity<TextInput>) -> Div {
    let t = theme::t();
    div().w_full().px_3().py_1p5().rounded_md().bg(rgba(t.raised)).border_1().border_color(rgba(t.line)).text_size(text::sm()).child(input)
}

/// A font field: `field` with an amber warning at its right edge while the family the
/// user typed (= what the config holds, once applied) isn't installed.
fn font_field(s: &State, input: &Entity<TextInput>, family: &str) -> Div {
    let missing = s.missing_fonts.iter().any(|m| m == family);
    field(input.clone()).flex().flex_row().items_center().gap_2().when(missing, |d| d.child(icon("triangle-alert", theme::t().warn)))
}

fn appearance(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let theme_open = this.menu == Some(Menu::Theme);
    let theme_menu = theme_open.then(|| {
        let items = theme::available()
            .into_iter()
            .map(|name| {
                let selected = name == this.config.theme;
                let pick = name.clone();
                menu::item(format!("theme/{name}"), name, "", selected, move |this, cx| this.set_theme(pick.clone(), cx), cx)
            })
            .collect();
        menu::popup(Side::BelowRight(26.), items, cx)
    });
    let theme_chip = theme::labeled_chip("theme-chip", "Theme", this.config.theme.clone(), false, theme_open)
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::Theme, cx)))
        .children(theme_menu);
    let s = &this.settings;
    let f = &this.config.fonts;
    let bg = this.config.background;
    section(
        "Appearance",
        div()
            .flex()
            .flex_col()
            .child(row("Theme", theme_chip))
            .child(row("Background", background_chip(this, cx)))
            .when(bg.pattern != Pattern::Off, |d| {
                let on = bg.reach == Reach::Full;
                d.child(row("In main pane", switch("bg-reach", on, cx, |this, cx| this.toggle_background_reach(cx))))
            })
            .child(row("Text size", text_size(f.size(), cx)))
            .child(row("UI font", font_field(s, &s.font_ui, &f.ui)))
            .child(row("Prose font", font_field(s, &s.font_prose, &f.prose)))
            .child(row("Code font", font_field(s, &s.font_code, &f.code))),
    )
}

/// The background pattern behind the sidebar (`ambient.rs`): a chip with the pattern's
/// name that opens the list of them.
fn background_chip(this: &Dirigent, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let open = this.menu == Some(Menu::Background);
    let current = this.config.background.pattern;
    let menu = open.then(|| {
        let items = Pattern::ALL
            .into_iter()
            .map(|p| menu::item(format!("bg/{}", p.name()), p.name(), "", p == current, move |this, cx| this.set_background(p, cx), cx))
            .collect();
        menu::popup(Side::BelowRight(26.), items, cx)
    });
    theme::labeled_chip("bg-chip", "Pattern", current.name(), current == Pattern::Off, open)
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::Background, cx)))
        .children(menu)
}

/// `−  13  +`: the UI's text size, a px at a time (also ctrl - / ctrl =, ctrl 0 resets; cmd
/// on macOS). Everything else scales with it. The setup's look step has it too.
pub fn text_size(size: f32, cx: &mut Context<Dirigent>) -> Div {
    let step = |id: &'static str, icon: &'static str, delta: f32, tip: String, cx: &mut Context<Dirigent>| {
        theme::icon_button(id, icon, cx, move |this, cx| this.step_text_size(delta, cx)).tooltip(theme::tooltip(tip))
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .child(step("text-smaller", "minus", -1., format!("Smaller ({} -)", input::MOD), cx))
        .child(div().w(u(32.)).text_center().child(format!("{size}")))
        .child(step("text-bigger", "plus", 1., format!("Bigger ({} =)", input::MOD), cx))
}

fn titles(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let s = &this.settings;
    let models = this.host(hosts::LOCAL).map(|h| h.models.clone()).unwrap_or_default();

    let model_open = this.menu == Some(Menu::TitleModel);
    let model_menu = model_open.then(|| {
        let mut items: Vec<AnyElement> = models
            .iter()
            .map(|m| {
                let key = m.key();
                let selected = key == this.config.title.model;
                let pick = key.clone();
                menu::item(format!("tm/{key}"), key, m.name.clone(), selected, move |this, cx| this.set_title_model(pick.clone(), cx), cx)
            })
            .collect();
        if items.is_empty() {
            let up = this.host(hosts::LOCAL).is_some_and(|h| h.status == HostStatus::Connected);
            items.push(menu::header(if up {
                "No models: sign in to a provider first (Providers)"
            } else {
                "No models yet: the local host is still connecting"
            }));
        }
        menu::popup(Side::BelowRight(26.), items, cx)
    });
    let model_chip =
        theme::labeled_chip("title-model", "Model", theme::short_model(&this.config.title.model).to_owned(), false, model_open)
            .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::TitleModel, cx)))
            .children(model_menu);

    let effort_open = this.menu == Some(Menu::TitleEffort);
    let effort_menu = effort_open.then(|| {
        let levels = models
            .iter()
            .find(|m| m.key() == this.config.title.model)
            .map(|m| m.thinking_levels.clone())
            .unwrap_or_else(|| ["off", "minimal", "low", "medium", "high"].map(String::from).to_vec());
        let items = levels
            .into_iter()
            .map(|l| {
                let selected = l == this.config.title.reasoning;
                let pick = l.clone();
                menu::item(format!("te/{l}"), l, "", selected, move |this, cx| this.set_title_reasoning(pick.clone(), cx), cx)
            })
            .collect();
        menu::popup(Side::BelowRight(26.), items, cx)
    });
    let effort_chip = theme::labeled_chip("title-effort", "Reasoning", this.config.title.reasoning.clone(), false, effort_open)
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::TitleEffort, cx)))
        .children(effort_menu);

    let test_button = theme::button("title-test", "play", if s.testing { "Close test" } else { "Test" })
        .on_click(cx.listener(|this, _, _, cx| this.toggle_title_test(cx)));

    let test = s.testing.then(|| {
        let running = s.test_request.is_some();
        let run = div()
            .id("title-run")
            .h(u(26.))
            .px_3()
            .flex()
            .items_center()
            .rounded_md()
            .cursor_pointer()
            .bg(rgba(if running { t.hover } else { t.accent }))
            .text_color(rgba(t.fg))
            .font_weight(FontWeight::MEDIUM)
            .hover(move |d| d.bg(rgba(t.accent_active)))
            .on_click(cx.listener(|this, _, _, cx| this.run_title_test(cx)))
            .child(if running { "Naming…" } else { "Name it" });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .pt_2()
            .child(field(s.sample.clone()))
            .child(div().flex().flex_row().items_center().gap_2().child(run).children(test_result(s)))
    });

    section(
        "Thread titles",
        div()
            .flex()
            .flex_col()
            .child(row("Model", model_chip))
            .child(row("Reasoning", effort_chip))
            .child(
                div().flex().flex_col().gap_1().py_2().child(div().text_color(rgba(t.fg_1)).child("Prompt")).child(field(s.prompt.clone())),
            )
            .child(div().flex().flex_row().pt_2().child(test_button))
            .children(test),
    )
}

/// The outcome of the last test: `“Title”  model · 1.2k in · 9 out · $0.001 · 0.8s`, or the error.
fn test_result(s: &State) -> Option<Div> {
    let t = theme::t();
    let result = s.test_result.as_ref()?;
    Some(match result {
        Ok(r) => div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_0p5()
            .child(div().truncate().font_weight(FontWeight::MEDIUM).child(SharedString::from(format!("“{}”", r.title))))
            .child(div().truncate().text_size(text::xs()).text_color(rgba(t.fg_2)).child(format!(
                "{} · {} in · {} out · {} · {}",
                theme::short_model(&r.model),
                kilo(r.input),
                kilo(r.output),
                // Titles cost fractions of a cent; `dollars` would round them to nothing.
                if r.cost < 0.01 { format!("${:.4}", r.cost) } else { dollars(r.cost) },
                duration(r.duration_ms)
            ))),
        Err(e) => div().flex_1().min_w_0().text_size(text::sm()).text_color(rgba(t.error)).child(e.clone()),
    })
}

/// A switch: a pill with a knob at one end or the other.
fn switch(
    id: &'static str,
    on: bool,
    cx: &mut Context<Dirigent>,
    toggle: impl Fn(&mut Dirigent, &mut Context<Dirigent>) + 'static,
) -> gpui::Stateful<Div> {
    let t = theme::t();
    div()
        .id(id)
        .w(u(34.))
        .h(u(18.))
        .rounded_full()
        .cursor_pointer()
        .bg(rgba(if on { t.accent_line } else { t.line }))
        .p(u(2.))
        .flex()
        .when(on, |d| d.justify_end())
        .on_click(cx.listener(move |this, _, _, cx| toggle(this, cx)))
        .child(div().size(u(14.)).rounded_full().bg(rgba(t.fg)))
}

/// The agent tools dirigent adds to pi's: each a switch; the set goes out with every prompt,
/// so a change reaches a thread with its next message. `async_bash` has an option under it,
/// shown while it is on.
fn tools(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let c = &this.config.tools;
    let bash_background = switch("tool-bash-background", c.bash_background, cx, |this, cx| this.toggle_tool("bash_background", cx));
    section(
        "Agent tools",
        div()
            .flex()
            .flex_col()
            .child(row("async_bash", switch("tool-async-bash", c.async_bash, cx, |this, cx| this.toggle_tool("async_bash", cx))))
            .when(c.async_bash, |d| d.child(sub_row("Move a bash command still running after 10 s to the background", bash_background)))
            .child(row("ask_user", switch("tool-ask-user", c.ask_user, cx, |this, cx| this.toggle_tool("ask_user", cx))))
            .child(row("dirigent", switch("tool-dirigent", c.dirigent, cx, |this, cx| this.toggle_tool("dirigent", cx)))),
    )
}

/// Nothing is sent yet; the switch only records the choice.
fn telemetry(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let on = this.config.telemetry;
    let switch = switch("telemetry", on, cx, |this, cx| this.toggle_telemetry(cx));
    section("Telemetry", row("Usage data", switch))
}

/// A small bordered button with a label only (a form's Cancel).
pub fn text_button(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Stateful<Div> {
    let t = theme::t();
    label_button(id, label, t.raised, t.line, t.hover)
}

/// `text_button` on the accent fill (as send): the one action a form is for (Save).
pub fn primary_button(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Stateful<Div> {
    let t = theme::t();
    label_button(id, label, t.accent, t.accent, t.accent_active)
}

fn label_button(id: impl Into<SharedString>, label: impl Into<SharedString>, bg: u32, border: u32, hover: u32) -> Stateful<Div> {
    div()
        .id(id.into())
        .h(u(26.))
        .px_2p5()
        .flex()
        .items_center()
        .flex_shrink_0()
        .rounded_md()
        .cursor_pointer()
        .bg(rgba(bg))
        .border_1()
        .border_color(rgba(border))
        .text_size(text::sm())
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgba(theme::t().fg))
        .hover(move |d| d.bg(rgba(hover)))
        .child(label.into())
}
