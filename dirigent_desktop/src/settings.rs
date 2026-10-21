//! Settings > Config: the UI over `config.toml` (`config.rs`). Everything saves as it is
//! changed: the text fields (fonts, title prompt) 300ms after the last keystroke, with a
//! warning mark on a font field whose family isn't installed; theme, title model,
//! reasoning and telemetry are chips and a switch that save at once.
//! `Test` opens a sample first message, runs it through the title settings on the local
//! host, and shows the title with what it cost.

use crate::app::{Dirigent, Menu, Pane};
use crate::icons::icon;
use crate::input::TextInput;
use crate::lists::{scroll, settings_header, settings_section};
use crate::menu::{self, Side};
use crate::theme::{self, dollars, kilo};
use crate::thread::duration;
use crate::{config, hosts};
use gpui::{AnyElement, Context, Div, Entity, FontWeight, SharedString, div, prelude::*, px, rgba};
use std::time::Instant;

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

/// The page's inputs and the state of its title test.
#[derive(Clone)]
pub struct State {
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

pub fn render(this: &Dirigent, pane: Div, cx: &mut Context<Dirigent>) -> Div {
    let column = div()
        .w_full()
        .max_w(px(600.))
        .flex()
        .flex_col()
        .gap_8()
        .py_8()
        .px_6()
        .child(appearance(this, cx))
        .child(titles(this, cx))
        .child(telemetry(this, cx))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .text_size(px(12.))
                .text_color(rgba(theme::t().fg_2))
                .child(format!("Also editable by hand: {}", config::path().display()))
                .child(format!(
                    "Your themes: {}/<name>.toml (default.example.toml is the reference, rewritten on every start)",
                    theme::dir().display()
                )),
        );
    pane.child(settings_header(Pane::Config, cx)).child(scroll("config", div().flex().flex_col().items_center().child(column)))
}

/// `label        control` on one line, with an optional hint under it.
fn row(label: &'static str, control: impl IntoElement, hint: Option<&'static str>) -> Div {
    let t = theme::t();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .py_2()
        .border_b_1()
        .border_color(rgba(t.line_soft))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .min_h(px(28.))
                .child(div().w(px(130.)).flex_shrink_0().text_color(rgba(t.fg_1)).child(label))
                .child(div().flex_1().min_w_0().flex().flex_row().items_center().justify_end().child(control)),
        )
        .children(hint.map(|h| div().pl(px(142.)).text_size(px(11.5)).text_color(rgba(t.fg_2)).child(h)))
}

/// A bordered text field.
fn field(input: Entity<TextInput>) -> Div {
    let t = theme::t();
    div().w_full().px_3().py_1p5().rounded_md().bg(rgba(t.raised)).border_1().border_color(rgba(t.line)).text_size(px(12.5)).child(input)
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
    settings_section(
        "Appearance",
        div()
            .flex()
            .flex_col()
            .child(row("Theme", theme_chip, Some("Built-in, or a toml file in your theme directory.")))
            .child(row("UI font", font_field(s, &s.font_ui, &f.ui), None))
            .child(row("Prose font", font_field(s, &s.font_prose, &f.prose), None))
            .child(row("Code font", font_field(s, &s.font_code, &f.code), Some("Installed family names, as fc-match knows them."))),
    )
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
            items.push(menu::header("No models yet: the local host is still connecting"));
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
            .h(px(26.))
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
            .child(div().text_size(px(12.)).text_color(rgba(t.fg_1)).child("A first message to name. ctrl+Enter runs it."))
            .child(field(s.sample.clone()))
            .child(div().flex().flex_row().items_center().gap_2().child(run).children(test_result(s)))
    });

    settings_section(
        "Thread titles",
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .py_2()
                    .text_size(px(12.))
                    .text_color(rgba(t.fg_1))
                    .child("Every thread is named from its first message by one small model, so the titles read alike."),
            )
            .child(row("Model", model_chip, None))
            .child(row("Reasoning", effort_chip, None))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .py_2()
                    .child(div().text_color(rgba(t.fg_1)).child("Prompt"))
                    .child(field(s.prompt.clone()))
                    .child(div().text_size(px(11.5)).text_color(rgba(t.fg_2)).child("The system prompt; the message is the user turn.")),
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
            .child(div().truncate().text_size(px(11.5)).text_color(rgba(t.fg_2)).child(format!(
                "{} · {} in · {} out · {} · {}",
                theme::short_model(&r.model),
                kilo(r.input),
                kilo(r.output),
                // Titles cost fractions of a cent; `dollars` would round them to nothing.
                if r.cost < 0.01 { format!("${:.4}", r.cost) } else { dollars(r.cost) },
                duration(r.duration_ms)
            ))),
        Err(e) => div().flex_1().min_w_0().text_size(px(12.)).text_color(rgba(t.error)).child(e.clone()),
    })
}

fn telemetry(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let on = this.config.telemetry;
    // A switch: a pill with a knob at one end or the other.
    let switch = div()
        .id("telemetry")
        .w(px(34.))
        .h(px(18.))
        .rounded_full()
        .cursor_pointer()
        .bg(rgba(if on { t.accent_line } else { t.line }))
        .p(px(2.))
        .flex()
        .when(on, |d| d.justify_end())
        .on_click(cx.listener(|this, _, _, cx| this.toggle_telemetry(cx)))
        .child(div().size(px(14.)).rounded_full().bg(rgba(t.fg)));
    settings_section("Telemetry", row("Usage data", switch, Some("Nothing is sent yet; this only records your choice.")))
}
