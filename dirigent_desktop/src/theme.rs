//! Palette, type, and the handful of shared widgets every pane uses.
//!
//! Linear-style chrome: the window canvas is the darkest surface and carries the sidebar;
//! the main pane floats on it as a rounded, bordered panel. One navy accent is spent
//! only on live things (running agents, send, focus). Status has a fixed vocabulary —
//! one glyph per state — reused in the sidebar counts, list rows, and thread header.

use crate::app::Dirigent;
use crate::icons::icon;
use dirigent_protocol::ThreadSummary;
use gpui::{AnyElement, Context, Div, ElementId, FontWeight, SharedString, Stateful, div, prelude::*, px, rgb};

// Surfaces, darkest to lightest.
pub const CANVAS: u32 = 0x0a0b0d;
pub const PANEL: u32 = 0x121316;
pub const RAISED: u32 = 0x18191d;
pub const HOVER: u32 = 0x1e2025;
pub const LINE: u32 = 0x25272d;
pub const LINE_SOFT: u32 = 0x1c1e23;

// Text.
pub const FG: u32 = 0xe4e5e8;
pub const FG_1: u32 = 0x8c909a;
pub const FG_2: u32 = 0x5d616a;

// The one accent (rgb 34 48 78): a fill, a brighter fill for the active menu item, a
// lighter line for focus and glow, and a tint light enough to read as text.
pub const NAVY: u32 = 0x22304e;
pub const NAVY_ACTIVE: u32 = 0x2c3f66;
pub const NAVY_LINE: u32 = 0x3a5080;
pub const NAVY_INK: u32 = 0x7a97cf;
/// Selected text, in the composer and the transcript (rgba).
pub const SELECTION: u32 = 0x4f6fd155;

// Status.
pub const WORKING: u32 = 0x6f8fd4;
pub const DONE: u32 = 0x6aa884;
pub const ERROR: u32 = 0xd26a6a;
pub const WARN: u32 = 0xcfa45a;

/// Gap between the window edge / sidebar and the main panel.
pub const GUTTER: f32 = 8.;

/// Width of the reading column in the thread view: a comfortable line length at 14px.
pub const COLUMN: f32 = 720.;

/// The composer runs a little wider than the column it sits under.
pub const COMPOSER: f32 = 760.;

#[derive(Clone)]
/// One font for everything.
pub struct Fonts {
    pub mono: SharedString,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Working,
    Done,
    Archived,
}

impl Status {
    pub fn of(t: &ThreadSummary) -> Self {
        if t.running() {
            Status::Working
        } else if t.archived {
            Status::Archived
        } else {
            Status::Done
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Working => "Working",
            Status::Done => "Done",
            Status::Archived => "Archived",
        }
    }

    /// The status glyph: a still accent dot while working (no spinners), a check when done.
    pub fn glyph(self) -> AnyElement {
        match self {
            Status::Working => div().size(px(16.)).flex().items_center().justify_center().child(dot(WORKING)).into_any_element(),
            Status::Done => icon("circle-check", DONE).into_any_element(),
            Status::Archived => icon("archive", FG_2).into_any_element(),
        }
    }
}

/// A 7px status dot.
pub fn dot(color: u32) -> Div {
    div().size(px(7.)).rounded_full().bg(rgb(color)).flex_shrink_0()
}

/// A square icon-only button, 26px.
pub fn icon_button(
    id: impl Into<ElementId>,
    name: &'static str,
    cx: &mut Context<Dirigent>,
    on_click: impl Fn(&mut Dirigent, &mut Context<Dirigent>) + 'static,
) -> Stateful<Div> {
    tinted_icon_button(id, name, FG_1, cx, on_click)
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
        .hover(|d| d.bg(rgb(HOVER)))
        .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
        .child(icon(name, color))
}

/// A small raised button with an icon and a label (header actions).
pub fn button(id: impl Into<SharedString>, name: &'static str, label: impl Into<SharedString>) -> Stateful<Div> {
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
        .bg(rgb(RAISED))
        .border_1()
        .border_color(rgb(LINE))
        .text_color(rgb(FG))
        .font_weight(FontWeight::MEDIUM)
        .hover(|d| d.bg(rgb(HOVER)))
        .child(icon(name, FG_1))
        .child(label.into())
}

/// A text chip that opens a menu: `label ▾`. Quiet until hovered; keeps the hover look
/// while its menu is `open`.
pub fn chip(id: impl Into<SharedString>, label: impl Into<SharedString>, dim: bool, open: bool) -> Stateful<Div> {
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
        .text_color(rgb(if dim { FG_2 } else { FG_1 }))
        .when(open, |d| d.bg(rgb(HOVER)).text_color(rgb(FG)))
        .hover(|d| d.bg(rgb(HOVER)).text_color(rgb(FG)))
        .child(label.into())
        .child(icon("chevron-down", FG_2).size(px(12.)))
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
        .bg(rgb(RAISED))
        .border_1()
        .border_color(rgb(LINE))
        .text_size(px(12.))
        .when(open, |d| d.bg(rgb(HOVER)))
        .hover(|d| d.bg(rgb(HOVER)))
        .child(div().text_color(rgb(FG_1)).child(prefix))
        .child(div().text_color(rgb(if dim { FG_2 } else { FG })).font_weight(FontWeight::MEDIUM).child(value.into()))
        .child(icon("chevron-down", FG_2).size(px(12.)).ml_0p5())
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
        .child(div().text_color(rgb(FG_1)).child(title))
        .child(div().text_size(px(12.)).text_color(rgb(FG_2)).child(hint))
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

/// `provider/model-id` → `model-id`.
pub fn short_model(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}
