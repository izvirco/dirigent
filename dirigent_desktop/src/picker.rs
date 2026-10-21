//! The model picker popup, two pages behind the composer's model chip:
//!
//! - `Menu::Model`: the current combo (if unsaved) and the saved presets, each a
//!   `<name> <provider/id> <effort>` row with a hover-revealed ✕. The header saves the
//!   current combo or links to the second page.
//! - `Menu::AllModels`: back button, search input, and every model the host offers.

use crate::app::{Dirigent, Menu, Preset};
use crate::icons::icon;
use crate::menu::{self, ROW_H, Side};
use crate::theme;
use dirigent_protocol::ModelInfo;
use gpui::{AnyElement, Context, Div, ElementId, Stateful, div, prelude::*, px, rgba};

/// Both pages share one width so flipping between them doesn't resize the popup.
const WIDTH: f32 = 430.;
/// Height of the scrolling model list (~9 rows).
const LIST_H: f32 = 240.;

pub fn menu(this: &Dirigent, no_target: bool, cx: &mut Context<Dirigent>) -> AnyElement {
    let page = match this.menu {
        Some(Menu::AllModels) => all_models(this, no_target, cx),
        _ => presets(this, cx),
    };
    menu::popup(Side::Above, vec![div().w(px(WIDTH)).flex().flex_col().children(page).into_any()], cx)
}

/// `{Saved presets | Save preset} … All models ›`, a hairline, then the preset rows.
fn presets(this: &Dirigent, cx: &mut Context<Dirigent>) -> Vec<AnyElement> {
    let current = this.current_preset();
    let saved = current.as_ref().is_some_and(|c| this.presets.contains(c));
    let left: AnyElement = match &current {
        Some(_) if !saved => header_button("save-preset", "Save preset", None, theme::t().accent_ink)
            .on_click(cx.listener(|this, _, _, cx| this.save_preset(cx)))
            .into_any(),
        _ => div().px_1p5().text_size(px(11.5)).text_color(rgba(theme::t().fg_2)).child("Saved presets").into_any(),
    };
    let header = div().flex().flex_row().items_center().justify_between().h(px(ROW_H)).flex_shrink_0().px_0p5().child(left).child(
        header_button("all-models", "All models", Some("chevron-right"), theme::t().fg_1)
            .on_click(cx.listener(|this, _, window, cx| this.open_all_models(window, cx))),
    );

    let mut rows = vec![header.into_any(), menu::separator()];
    if let Some(c) = &current
        && !saved
    {
        rows.push(preset_row("p/current", c, true, None, this, cx));
    }
    for (i, p) in this.presets.iter().enumerate() {
        let selected = current.as_ref() == Some(p);
        rows.push(preset_row(format!("p/{i}"), p, selected, Some(i), this, cx));
    }
    if rows.len() == 2 {
        rows.push(menu::header("No presets yet. Pick a model, then save the combo."));
    }
    rows
}

/// `‹ [search]`, a hairline, then the matching models in a fixed-height scroll.
fn all_models(this: &Dirigent, no_target: bool, cx: &mut Context<Dirigent>) -> Vec<AnyElement> {
    let back = div()
        .id("models-back")
        .size(px(ROW_H))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .hover(|d| d.bg(rgba(theme::t().line)))
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::Model, cx)))
        .child(icon("chevron-left", theme::t().fg_1));
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(px(ROW_H))
        .flex_shrink_0()
        .child(back)
        .child(icon("search", theme::t().fg_2).size(px(13.)))
        .child(div().flex_1().min_w_0().text_size(px(13.)).child(this.model_search.clone()));

    let models = this.filtered_models(cx);
    let items: Vec<AnyElement> = if models.is_empty() {
        let hint = match (no_target, this.models().is_empty()) {
            (true, _) => "Pick a project first",
            (false, true) => "No models yet. Waiting for the host.",
            (false, false) => "No models match",
        };
        vec![menu::header(hint)]
    } else {
        models.iter().map(|m| model_row(m, this, cx)).collect()
    };
    let list = div().id("models-list").flex().flex_col().max_h(px(LIST_H)).overflow_y_scroll().children(items).into_any();
    vec![header.into_any(), menu::separator(), list]
}

/// `<name> <provider/id>`; picks the model, keeping the effort when the model supports it.
fn model_row(m: &ModelInfo, this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let key = m.key();
    let selected = this.model.as_deref() == Some(&key);
    menu::item(format!("m/{key}"), m.name.clone(), key.clone(), selected, move |this, cx| this.set_model(key.clone(), cx), cx)
}

/// `<name> <provider/id> <effort>` with a ✕ on hover when `index` names a saved preset.
fn preset_row(
    id: impl Into<gpui::SharedString>,
    p: &Preset,
    selected: bool,
    index: Option<usize>,
    this: &Dirigent,
    cx: &mut Context<Dirigent>,
) -> AnyElement {
    let name = this
        .models()
        .iter()
        .find(|m| m.key() == p.model)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| theme::short_model(&p.model).to_owned());
    let preset = p.clone();
    menu::row(id, selected)
        .group("preset")
        .on_click(cx.listener(move |this, _, _, cx| this.use_preset(&preset, cx)))
        .child(div().flex_shrink_0().child(name))
        .child(div().min_w_0().truncate().text_color(rgba(theme::t().fg_1)).text_size(px(12.)).child(p.model.clone()))
        .children(p.effort.clone().map(|e| div().flex_shrink_0().text_color(rgba(theme::t().accent_ink)).text_size(px(12.)).child(e)))
        .child(div().flex_1())
        .children(index.map(|i| {
            div()
                .id(ElementId::Name(format!("p/{i}/x").into()))
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .opacity(0.)
                .group_hover("preset", |d| d.opacity(1.))
                .hover(|d| d.bg(rgba(theme::t().hover)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.remove_preset(i, cx);
                }))
                .child(icon("x", theme::t().fg_1).size(px(12.)))
        }))
        .into_any()
}

/// A quiet text button for the presets header, optionally with a trailing icon.
fn header_button(id: &'static str, label: &'static str, trailing: Option<&'static str>, color: u32) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_0p5()
        .h(px(22.))
        .px_1p5()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(rgba(color))
        .hover(|d| d.bg(rgba(theme::t().line)).text_color(rgba(theme::t().fg)))
        .child(label)
        .children(trailing.map(|name| icon(name, color).size(px(12.))))
}
