//! The model picker popup, two pages behind the composer's model chip:
//!
//! - `Menu::Model`: the current combo (if unsaved) and the saved presets, each a
//!   `<name> <provider/id> <effort>` row with a hover-revealed ✕, grouped by model
//!   (`ordered`). The header saves the current combo or links to the second page.
//! - `Menu::AllModels`: back button, search input, and every model the host offers.
//!
//! A host that offers no models has no provider set up: both pages then lead to
//! Settings › Providers, to sign in to one there.

use crate::app::{Dirigent, Menu, Preset};
use crate::icons::icon;
use crate::menu::{self, ROW_H, Side};
use crate::net::HostStatus;
use crate::theme::{self, text, u};
use dirigent_protocol::ModelInfo;
use gpui::{AnyElement, Context, Div, ElementId, Stateful, div, prelude::*, rgba};

/// Both pages share one width so flipping between them doesn't resize the popup.
const WIDTH: f32 = 430.;
/// Height of the scrolling model list (~9 rows).
const LIST_H: f32 = 240.;

/// The popup, opening to `side` of the chip (above it in the composer, under it in an
/// edit bubble).
pub fn menu(this: &Dirigent, no_target: bool, side: Side, cx: &mut Context<Dirigent>) -> AnyElement {
    let page = match this.menu {
        Some(Menu::AllModels) => all_models(this, no_target, cx),
        _ => presets(this, cx),
    };
    menu::popup(side, vec![div().w(u(WIDTH)).flex().flex_col().children(page).into_any()], cx)
}

/// `{Saved presets | Save preset} … All models ›`, a hairline, then the preset rows.
fn presets(this: &Dirigent, cx: &mut Context<Dirigent>) -> Vec<AnyElement> {
    let current = this.current_preset();
    let saved = current.as_ref().is_some_and(|c| this.presets.contains(c));
    let left: AnyElement = match &current {
        Some(_) if !saved => {
            let save = header_button("save-preset", "Save preset", None, theme::t().accent_ink);
            menu::header_stop(save, |this, _, cx| this.save_preset(cx), cx).into_any()
        }
        _ => div().px_1p5().text_size(text::xs()).text_color(rgba(theme::t().fg_2)).child("Saved presets").into_any(),
    };
    let all = header_button("all-models", "All models", Some("chevron-right"), theme::t().fg_1);
    let all = menu::header_stop(all, |this, window, cx| this.open_all_models(window, cx), cx);
    let header = div().flex().flex_row().items_center().justify_between().h(u(ROW_H)).flex_shrink_0().px_0p5().child(left).child(all);

    let mut rows = vec![header.into_any(), menu::separator()];
    // First: nothing here runs without it.
    rows.extend(sign_in(this, cx));
    if let Some(c) = &current
        && !saved
    {
        rows.push(preset_row("p/current", c, true, None, this, cx));
    }
    for (i, p) in ordered(&this.presets, this.models()) {
        let selected = current.as_ref() == Some(p);
        rows.push(preset_row(format!("p/{i}"), p, selected, Some(i), this, cx));
    }
    if rows.len() == 2 {
        rows.push(menu::header("No presets yet. Pick a model, then save the combo."));
    }
    rows
}

/// The saved presets in the order they are listed, each with its index in `presets` (what
/// its ✕ removes): by model name, then `provider/id`, then effort in the model's own level
/// order (by name for a model the host doesn't offer).
fn ordered<'a>(presets: &'a [Preset], models: &[ModelInfo]) -> Vec<(usize, &'a Preset)> {
    let mut rows: Vec<_> = presets.iter().enumerate().collect();
    rows.sort_by_cached_key(|(_, p)| {
        let levels = models.iter().find(|m| m.key() == p.model).map(|m| m.thinking_levels.as_slice()).unwrap_or(&[]);
        let level = levels.iter().position(|l| Some(l) == p.effort.as_ref());
        (model_name(&p.model, models).to_lowercase(), p.model.clone(), level, p.effort.clone())
    });
    rows
}

/// What a preset's model is called: the host's name for it, else the id from its key.
fn model_name(key: &str, models: &[ModelInfo]) -> String {
    models.iter().find(|m| m.key() == key).map(|m| m.name.clone()).unwrap_or_else(|| theme::short_model(key).to_owned())
}

/// When the composer's host is up but offers no models: a row to Settings › Providers, to
/// sign in to a provider there.
fn sign_in(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<AnyElement> {
    let host = this.composer_host()?.to_owned();
    let up = this.host(&host).is_some_and(|h| h.status == HostStatus::Connected);
    (up && this.models().is_empty()).then(|| {
        let open = move |this: &mut Dirigent, cx: &mut Context<Dirigent>| this.open_providers(Some(host.clone()), cx);
        menu::item("sign-in", "Sign in to a provider", "this host has no models yet", false, open, cx)
    })
}

/// `‹ [search]`, a hairline, then the matching models in a fixed-height scroll.
fn all_models(this: &Dirigent, no_target: bool, cx: &mut Context<Dirigent>) -> Vec<AnyElement> {
    // Mouse only, not a `menu::header_stop`: Left / Right are the search's.
    let back = div()
        .id("models-back")
        .size(u(ROW_H))
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
        .h(u(ROW_H))
        .flex_shrink_0()
        .child(back)
        .child(div().flex_1().min_w_0().text_size(text::base()).child(this.model_search.clone()));

    let list = menu::results("models-list", LIST_H, cx, |cx| {
        let models = this.filtered_models(cx);
        if models.is_empty() {
            let hint = match (no_target, this.models().is_empty()) {
                (true, _) => "Pick a project first",
                (false, true) => "No models yet. Waiting for the host.",
                (false, false) => "No models match",
            };
            vec![sign_in(this, cx).unwrap_or_else(|| menu::header(hint))]
        } else {
            models.iter().map(|m| model_row(m, this, cx)).collect()
        }
    });
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
    let name = model_name(&p.model, this.models());
    let preset = p.clone();
    menu::stop(menu::row(id), selected, move |this, _, cx| this.use_preset(&preset, cx), cx)
        .group("preset")
        .child(div().flex_shrink_0().child(name))
        .child(div().min_w_0().truncate().text_color(rgba(theme::t().fg_1)).text_size(text::sm()).child(p.model.clone()))
        .children(p.effort.clone().map(|e| div().flex_shrink_0().text_color(rgba(theme::t().accent_ink)).text_size(text::sm()).child(e)))
        .child(div().flex_1())
        .children(index.map(|i| {
            div()
                .id(ElementId::Name(format!("p/{i}/x").into()))
                .size(u(20.))
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
                .child(icon("x", theme::t().fg_1).size(u(12.)))
        }))
        .into_any()
}

/// A quiet text button for the presets header, optionally with a trailing icon. A
/// `menu::header_stop` lights it.
fn header_button(id: &'static str, label: &'static str, trailing: Option<&'static str>, color: u32) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_0p5()
        .h(u(22.))
        .px_1p5()
        .rounded_md()
        .cursor_pointer()
        .text_size(text::sm())
        .text_color(rgba(color))
        .hover(|d| d.text_color(rgba(theme::t().fg)))
        .child(label)
        .children(trailing.map(|name| icon(name, color).size(u(12.))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_group_by_model_then_effort_level() {
        let model = |id: &str, name: &str| ModelInfo {
            provider: "p".into(),
            id: id.into(),
            name: name.into(),
            context_window: 0,
            thinking_levels: ["low", "high", "xhigh"].map(String::from).to_vec(),
        };
        let preset = |model: &str, effort: &str| Preset { model: model.into(), effort: Some(effort.into()) };
        // The host's names order the models, not their keys; `gone` isn't offered any more.
        let models = [model("a", "Opus"), model("b", "Fable")];
        let presets = [
            preset("p/a", "xhigh"),
            preset("p/gone", "low"),
            preset("p/b", "xhigh"),
            preset("p/b", "low"),
            preset("p/gone", "high"),
            preset("p/a", "high"),
        ];
        let order: Vec<usize> = ordered(&presets, &models).into_iter().map(|(i, _)| i).collect();
        // Fable low, xhigh; gone high, low (by name); Opus high, xhigh.
        assert_eq!(order, [3, 2, 4, 1, 5, 0]);
    }
}
