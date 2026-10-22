//! Palette, type, and the handful of shared widgets every pane uses.
//!
//! Linear-style chrome: the window canvas is the darkest surface and carries the sidebar;
//! the main pane floats on it as a rounded, bordered panel. One accent is spent only on
//! live things (running agents, send, focus). Status has a fixed vocabulary, one glyph
//! per state, reused in the sidebar counts, list rows, and thread header.
//!
//! The palette is a `Theme` loaded from a toml file (`config.toml`'s `theme` names it):
//! built-ins are embedded from `assets/themes/`, and `~/.config/dirigent/theme/<name>.toml`
//! adds or overrides one. Read it anywhere with `theme::t()`.

use crate::app::Dirigent;
use crate::icons::icon;
use anyhow::{Context as _, Result, bail};
use dirigent_protocol::ThreadSummary;
use gpui::{
    Animation, AnimationExt, AnyElement, AnyView, App, Context, Div, ElementId, FontWeight, Render, SharedString, Stateful, Svg, Window,
    WindowBackgroundAppearance, div, prelude::*, pulsating_between, px, rgba,
};
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::Duration;

/// The built-in themes: `(name, toml)`. The first is the default.
pub const BUILTIN: &[(&str, &str)] = &[
    ("dirigent", include_str!("../assets/themes/dirigent.toml")),
    ("dirigent-transparent", include_str!("../assets/themes/dirigent-transparent.toml")),
    ("midnight", include_str!("../assets/themes/midnight.toml")),
    ("carbon", include_str!("../assets/themes/carbon.toml")),
    ("gruvbox-dark-hard", include_str!("../assets/themes/gruvbox-dark-hard.toml")),
    ("tokyo-night", include_str!("../assets/themes/tokyo-night.toml")),
    ("kanagawa", include_str!("../assets/themes/kanagawa.toml")),
    ("dirigent-light", include_str!("../assets/themes/dirigent-light.toml")),
    ("gruvbox-light", include_str!("../assets/themes/gruvbox-light.toml")),
    ("rose-pine-dawn", include_str!("../assets/themes/rose-pine-dawn.toml")),
];

/// One palette. Colours are `0xRRGGBBAA`; the toml spells them `"#rrggbb"` (opaque) or
/// `"#rrggbbaa"`. A translucent `canvas` makes the window itself see-through (`background`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Theme {
    // Surfaces, darkest to lightest (lightest to darkest in a light theme).
    #[serde(with = "hex")]
    pub canvas: u32,
    #[serde(with = "hex")]
    pub panel: u32,
    #[serde(with = "hex")]
    pub raised: u32,
    #[serde(with = "hex")]
    pub hover: u32,
    /// Menus, toasts and hover cards: floating over everything, so solid even in a
    /// translucent theme.
    #[serde(with = "hex")]
    pub popup: u32,
    #[serde(with = "hex")]
    pub line: u32,
    #[serde(with = "hex")]
    pub line_soft: u32,
    // Text: primary, secondary, tertiary.
    #[serde(with = "hex")]
    pub fg: u32,
    #[serde(with = "hex")]
    pub fg_1: u32,
    #[serde(with = "hex")]
    pub fg_2: u32,
    // The one accent: a fill, a brighter fill for the active menu item, a lighter line for
    // focus and glow, and a tint light enough to read as text.
    #[serde(with = "hex")]
    pub accent: u32,
    #[serde(with = "hex")]
    pub accent_active: u32,
    #[serde(with = "hex")]
    pub accent_line: u32,
    #[serde(with = "hex")]
    pub accent_ink: u32,
    /// Selected text, in the composer and the transcript (with alpha).
    #[serde(with = "hex")]
    pub selection: u32,
    /// The user's messages in the transcript: a shade under the send button's accent.
    #[serde(with = "hex")]
    pub bubble: u32,
    // The stop button: a deep fill, its hover, and a light icon (the same pairing as send).
    #[serde(with = "hex")]
    pub danger: u32,
    #[serde(with = "hex")]
    pub danger_active: u32,
    #[serde(with = "hex")]
    pub danger_ink: u32,
    // Status.
    #[serde(with = "hex")]
    pub working: u32,
    #[serde(with = "hex")]
    pub done: u32,
    #[serde(with = "hex")]
    pub error: u32,
    #[serde(with = "hex")]
    pub warn: u32,
}

/// `"#rrggbb"` or `"#rrggbbaa"` to `0xRRGGBBAA`.
mod hex {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
        let s = String::deserialize(d)?;
        let digits = s.strip_prefix('#').unwrap_or(&s);
        let value = u32::from_str_radix(digits, 16).map_err(|_| serde::de::Error::custom(format!("`{s}`: not a hex colour")))?;
        match digits.len() {
            6 => Ok(value << 8 | 0xff),
            8 => Ok(value),
            _ => Err(serde::de::Error::custom(format!("`{s}`: expected #rrggbb or #rrggbbaa"))),
        }
    }
}

/// `color` with its alpha replaced.
pub fn alpha(color: u32, alpha: u32) -> u32 {
    (color & !0xff) | (alpha & 0xff)
}

/// What the window should paint behind the canvas: nothing (blurred where the compositor
/// can) when the canvas is translucent, else an opaque surface.
pub fn background(theme: &Theme) -> WindowBackgroundAppearance {
    if theme.canvas & 0xff < 0xff { WindowBackgroundAppearance::Blurred } else { WindowBackgroundAppearance::Opaque }
}

static THEME: RwLock<Option<Theme>> = RwLock::new(None);

/// The current palette. The default theme until `set` is called.
pub fn t() -> Theme {
    match *THEME.read().unwrap() {
        Some(t) => t,
        None => parse(BUILTIN[0].1).expect("default theme parses"),
    }
}

pub fn set(theme: Theme) {
    *THEME.write().unwrap() = Some(theme);
}

pub fn parse(text: &str) -> Result<Theme> {
    Ok(toml::from_str(text)?)
}

/// `~/.config/dirigent/theme/`, where user themes live.
pub fn dir() -> PathBuf {
    crate::config::dir().join("theme")
}

/// The reference copy of the default theme in `dir()`: rewritten from the embedded one on
/// every start (`write_example`) so it always shows what a theme file supports. Never listed.
pub const EXAMPLE: &str = "default.example";

pub fn write_example() -> Result<()> {
    let dir = dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{EXAMPLE}.toml"));
    let text = format!("# Reference copy of the built-in default theme, rewritten on every start: edit a copy.\n{}", BUILTIN[0].1);
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))
}

/// Every theme name: the built-ins, then the files in `dir()` (a user file with a
/// built-in's name is that theme's replacement, so it is listed once; `EXAMPLE` never is).
pub fn available() -> Vec<String> {
    let mut names: Vec<String> = BUILTIN.iter().map(|(n, _)| (*n).to_owned()).collect();
    if let Ok(entries) = std::fs::read_dir(dir()) {
        let mut user: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                let stem = path.file_stem()?.to_str()?.to_owned();
                (path.extension()?.to_str()? == "toml" && stem != EXAMPLE && !names.contains(&stem)).then_some(stem)
            })
            .collect();
        user.sort();
        names.extend(user);
    }
    names
}

/// The theme called `name`: the user's file first, then the built-in.
pub fn load(name: &str) -> Result<Theme> {
    let path = dir().join(format!("{name}.toml"));
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).with_context(|| format!("theme {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match BUILTIN.iter().find(|(n, _)| *n == name) {
            Some((_, text)) => parse(text),
            None => bail!("no theme named `{name}` (built-in or in {})", dir().display()),
        },
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Gap between the window edge / sidebar and the main panel.
pub const GUTTER: f32 = 8.;

/// Width of the reading column in the thread view: a comfortable line length at 14px.
pub const COLUMN: f32 = 720.;

/// The composer runs a little wider than the column it sits under.
pub const COMPOSER: f32 = 760.;

/// The three font families from `config.toml`, resolved to installed names: `ui` for
/// chrome, `prose` for the transcript's messages, `mono` for code, tool calls and model ids.
#[derive(Clone)]
pub struct Fonts {
    pub ui: SharedString,
    pub prose: SharedString,
    pub mono: SharedString,
}

/// Variant order is list order: the Inbox and project pages sort rows by it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Status {
    Working,
    /// Finished its turn with `ask_user` questions open: the user is the blocker.
    Asking,
    Done,
    /// Done, but snoozed out of the inbox (desktop-side, see `Dirigent::status`).
    Snoozed,
    Archived,
}

impl Status {
    /// Status from the host's view of the thread; never `Snoozed`, that's desktop state.
    pub fn of(t: &ThreadSummary) -> Self {
        if t.running() {
            Status::Working
        } else if t.archived {
            Status::Archived
        } else if t.asking() {
            Status::Asking
        } else {
            Status::Done
        }
    }

    /// Waiting on the user, one way or the other: what the Inbox lists.
    pub fn finished(self) -> bool {
        matches!(self, Status::Asking | Status::Done)
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Working => "Working",
            Status::Asking => "Asking",
            Status::Done => "Done",
            Status::Snoozed => "Snoozed",
            Status::Archived => "Archived",
        }
    }

    /// The status glyph: a still accent dot while working (no spinners), an amber question
    /// mark while asking, a check when done, an amber moon while snoozed, an archive box
    /// when archived.
    pub fn glyph(self) -> AnyElement {
        let t = t();
        match self {
            Status::Working => div().size(px(16.)).flex().items_center().justify_center().child(dot(t.working)).into_any_element(),
            Status::Asking => icon("circle-help", t.warn).into_any_element(),
            Status::Done => icon("circle-check", t.done).into_any_element(),
            Status::Snoozed => icon("moon", t.warn).into_any_element(),
            Status::Archived => icon("archive", t.fg_2).into_any_element(),
        }
    }
}

/// The small amber pin shown before a pinned thread's name (sidebar cards, list rows,
/// thread header).
pub fn pin_mark() -> Svg {
    icon("pin", t().warn).size(px(12.))
}

/// A thread's title, truncated to its row. While `generating` (the name is on its way from
/// the title model; the text is the first message meanwhile) it breathes, so the
/// placeholder reads as provisional.
pub fn title_text(id: impl Into<ElementId>, title: impl Into<SharedString>, generating: bool) -> AnyElement {
    let text = div().min_w_0().truncate().child(title.into());
    if !generating {
        return text.into_any_element();
    }
    text.with_animation(id, Animation::new(Duration::from_millis(1600)).repeat().with_easing(pulsating_between(0.3, 1.)), |d, v| {
        d.opacity(v)
    })
    .into_any_element()
}

/// A 7px status dot.
pub fn dot(color: u32) -> Div {
    div().size(px(7.)).rounded_full().bg(rgba(color)).flex_shrink_0()
}

/// A square icon-only button, 26px.
pub fn icon_button(
    id: impl Into<ElementId>,
    name: &'static str,
    cx: &mut Context<Dirigent>,
    on_click: impl Fn(&mut Dirigent, &mut Context<Dirigent>) + 'static,
) -> Stateful<Div> {
    tinted_icon_button(id, name, t().fg_1, cx, on_click)
}

/// `icon_button` with the icon in a given colour (a pinned pin is amber).
pub fn tinted_icon_button(
    id: impl Into<ElementId>,
    name: &'static str,
    color: u32,
    cx: &mut Context<Dirigent>,
    on_click: impl Fn(&mut Dirigent, &mut Context<Dirigent>) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .size(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .hover(|d| d.bg(rgba(t().hover)))
        .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
        .child(icon(name, color))
}

/// A small raised button with an icon and a label (header actions).
pub fn button(id: impl Into<SharedString>, name: &'static str, label: impl Into<SharedString>) -> Stateful<Div> {
    let t = t();
    div()
        .id(ElementId::Name(id.into()))
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap_1p5()
        .h(px(26.))
        .px_2()
        .rounded_md()
        .cursor_pointer()
        .bg(rgba(t.raised))
        .border_1()
        .border_color(rgba(t.line))
        .text_color(rgba(t.fg))
        .font_weight(FontWeight::MEDIUM)
        .hover(move |d| d.bg(rgba(t.hover)))
        .child(icon(name, t.fg_1))
        .child(label.into())
}

/// A text chip that opens a menu: `label ▾`. Quiet until hovered; keeps the hover look
/// while its menu is `open`.
pub fn chip(id: impl Into<SharedString>, label: impl Into<SharedString>, dim: bool, open: bool) -> Stateful<Div> {
    let t = t();
    chip_in(id, label, if dim { t.fg_2 } else { t.fg_1 }, t.fg_2, open)
}

/// `chip` with its own text colour (kept on hover) and chevron colour (a faint chevron
/// marks a menu that can't open right now).
pub fn chip_in(id: impl Into<SharedString>, label: impl Into<SharedString>, color: u32, chevron: u32, open: bool) -> Stateful<Div> {
    let t = t();
    let hover_color = if color == t.fg_1 || color == t.fg_2 { t.fg } else { color };
    div()
        .id(ElementId::Name(id.into()))
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(px(24.))
        .px_1p5()
        .rounded_md()
        .cursor_pointer()
        .text_color(rgba(color))
        .when(open, |d| d.bg(rgba(t.hover)).text_color(rgba(hover_color)))
        .hover(move |d| d.bg(rgba(t.hover)).text_color(rgba(hover_color)))
        .child(label.into())
        .child(icon("chevron-down", chevron).size(px(12.)))
}

/// One line of text shown under the mouse after a pause; pass to `.tooltip(...)`.
pub fn tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_, cx| cx.new(|_| Tooltip(text.clone())).into()
}

struct Tooltip(SharedString);

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = t();
        div()
            .px_2()
            .py_1()
            .max_w(px(320.))
            .rounded_md()
            .bg(rgba(t.popup))
            .border_1()
            .border_color(rgba(t.line))
            .shadow_md()
            .text_size(px(12.))
            .text_color(rgba(t.fg))
            .child(self.0.clone())
    }
}

/// A filled chip with a dim prefix and a bright value (`Create thread in  dirigent`).
/// Keeps the hover look while its menu is `open`.
pub fn labeled_chip(
    id: impl Into<SharedString>,
    prefix: &'static str,
    value: impl Into<SharedString>,
    dim: bool,
    open: bool,
) -> Stateful<Div> {
    let t = t();
    div()
        .id(ElementId::Name(id.into()))
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap_1p5()
        .h(px(26.))
        .pl_2p5()
        .pr_2()
        .rounded_md()
        .cursor_pointer()
        .bg(rgba(t.raised))
        .border_1()
        .border_color(rgba(t.line))
        .text_size(px(12.))
        .when(open, |d| d.bg(rgba(t.hover)))
        .hover(move |d| d.bg(rgba(t.hover)))
        .child(div().text_color(rgba(t.fg_1)).child(prefix))
        .child(div().text_color(rgba(if dim { t.fg_2 } else { t.fg })).font_weight(FontWeight::MEDIUM).child(value.into()))
        .child(icon("chevron-down", t.fg_2).size(px(12.)).ml_0p5())
}

/// Centered empty state with a one-line hint.
pub fn empty(title: &'static str, hint: &'static str) -> Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_1()
        .child(div().text_color(rgba(t().fg_1)).child(title))
        .child(div().text_size(px(12.)).text_color(rgba(t().fg_2)).child(hint))
}

/// "now", "5m", "3h", "2d" for a millisecond age.
pub fn rel_time(age_ms: i64) -> String {
    let s = age_ms.max(0) / 1000;
    match s {
        0..60 => "now".into(),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// Token counts: `812`, `9.5k`, `138k`, `1M`, `1.2M`, `33M`.
pub fn kilo(n: u64) -> String {
    let (v, suffix) = if n >= 1_000_000 {
        (n as f64 / 1e6, "M")
    } else if n >= 1000 {
        (n as f64 / 1e3, "k")
    } else {
        return n.to_string();
    };
    let s = if v < 9.95 { format!("{v:.1}") } else { format!("{}", v.round()) };
    format!("{}{suffix}", s.strip_suffix(".0").unwrap_or(&s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_themes_parse() {
        for (name, text) in BUILTIN {
            parse(text).unwrap_or_else(|e| panic!("{name}: {e:#}"));
        }
        assert_eq!(parse(BUILTIN[0].1).unwrap().canvas, 0x0a0b0dff);
        assert_eq!(parse(BUILTIN[1].1).unwrap().canvas, 0x000000e6);
        assert_eq!(background(&parse(BUILTIN[0].1).unwrap()), WindowBackgroundAppearance::Opaque);
        assert_eq!(background(&parse(BUILTIN[1].1).unwrap()), WindowBackgroundAppearance::Blurred);
        assert_eq!(alpha(0x11223344, 0x80), 0x11223380);
        assert!(parse("canvas = \"#123\"").is_err());
    }

    #[test]
    fn compact_numbers() {
        assert_eq!(kilo(812), "812");
        assert_eq!(kilo(9_540), "9.5k");
        assert_eq!(kilo(138_400), "138k");
        assert_eq!(kilo(1_000_000), "1M");
        assert_eq!(kilo(1_240_000), "1.2M");
        assert_eq!(dollars(24.664), "$24.66");
        assert_eq!(dollars(0.004), "$0.004");
        assert_eq!(dollars(0.), "$0.00");
    }
}

/// Dollars with cents: `$24.66`, `$0.05`; tenths of a cent when there's less than one.
pub fn dollars(v: f64) -> String {
    if v > 0. && v < 0.01 { format!("${v:.3}") } else { format!("${v:.2}") }
}

/// `provider/model-id` → `model-id`.
pub fn short_model(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}
