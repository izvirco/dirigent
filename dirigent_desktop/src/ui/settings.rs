//! In-app editing of config.toml, using the same palette controls as onboarding.
use gpui::{AnyElement, Context, Entity, IntoElement, MouseButton, div, prelude::*, px};

use crate::{
    app::Dirigent,
    text_input::{InputEvent, TextInput},
    theme::{self, rgb},
};

pub(crate) struct Settings {
    font: Entity<TextInput>,
    themes: Vec<theme::ThemePreview>,
    error: Option<String>,
}

pub(super) fn theme_swatch(colors: [u32; 3], selected: bool) -> impl IntoElement {
    div()
        .size(px(88.0))
        .p_3()
        .rounded_xl()
        .when(selected, |tile| tile.bg(rgb(theme::border_emphasized())))
        .child(
            div()
                .size_full()
                .flex()
                .gap_1()
                .children(
                    colors
                        .into_iter()
                        .zip([5.0, 3.0, 2.0])
                        .map(|(color, weight)| {
                            div()
                                .flex_basis(px(0.0))
                                .flex_grow(weight)
                                .h_full()
                                .rounded_md()
                                .bg(rgb(color))
                        }),
                ),
        )
}

pub(super) fn telemetry_switch() -> impl IntoElement {
    div()
        .w(px(32.0))
        .h(px(18.0))
        .flex_shrink_0()
        .p(px(3.0))
        .rounded_full()
        .flex()
        .items_center()
        .bg(rgb(if theme::telemetry_enabled() {
            theme::accent()
        } else {
            theme::border_emphasized()
        }))
        .when(theme::telemetry_enabled(), |switch| switch.justify_end())
        .child(
            div()
                .size(px(12.0))
                .rounded_full()
                .bg(rgb(theme::theme_text())),
        )
}

pub(super) const TELEMETRY_LABEL: &str =
    "Enable telemetry to steal your data and use it for performance optimizations";

impl Dirigent {
    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.usage = None;
        let font = cx.new(|cx| {
            let mut input = TextInput::new("Font family", cx).compact().borderless();
            input.set_text(self.font.to_string(), cx);
            input
        });
        cx.subscribe(&font, |this, _, event, cx| {
            match event {
                InputEvent::Submit => this.save_settings_font(cx),
                InputEvent::Escape => {
                    this.settings = None;
                    this.enter_normal_mode();
                }
                _ => {}
            }
            cx.notify();
        })
        .detach();
        self.settings = Some(Settings {
            font,
            themes: theme::theme_previews(),
            error: None,
        });
        self.about_open = false;
        self.sidebar_menu = None;
        self.enter_normal_mode();
        cx.notify();
    }

    fn save_settings_font(&mut self, cx: &mut Context<Self>) {
        let Some(settings) = self.settings.as_mut() else {
            return;
        };
        let font = settings.font.read(cx).text().trim().to_string();
        match theme::set_font(&font) {
            Ok(()) => {
                self.font = font.into();
                settings.error = None;
            }
            Err(error) => settings.error = Some(error),
        }
        cx.notify();
    }

    pub(super) fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = self.settings.as_ref().unwrap();
        div()
            .id("settings-scroll")
            .bg(rgb(theme::bg()))
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .p_8()
            // Keep input clicks from activating the root's normal-mode focus behavior.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
            .child(
                div()
                    .w_full()
                    .max_w(px(640.0))
                    .mx_auto()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap_8()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Settings"),
                            )
                            .child(
                                div()
                                    .id("close-settings")
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .text_xs()
                                    .cursor_pointer()
                                    .text_color(rgb(theme::muted()))
                                    .hover(|s| s.bg(rgb(theme::surface_hover())))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.settings = None;
                                        this.enter_normal_mode();
                                        cx.notify();
                                    }))
                                    .child("Close"),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().text_sm().child("Font"))
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.0))
                                            .h(px(30.0))
                                            .px_2()
                                            .py_1()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(rgb(theme::border()))
                                            .bg(rgb(theme::surface()))
                                            .child(settings.font.clone()),
                                    )
                                    .child(
                                        div()
                                            .id("apply-settings-font")
                                            .h(px(30.0))
                                            .px_2()
                                            .flex()
                                            .items_center()
                                            .rounded_md()
                                            .text_xs()
                                            .cursor_pointer()
                                            .bg(rgb(theme::accent()))
                                            .text_color(rgb(theme::bg()))
                                            .hover(|s| s.bg(rgb(theme::accent_hover())))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.save_settings_font(cx)
                                            }))
                                            .child("Apply"),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_5()
                            .child(div().text_sm().child("Pick a UI flavour"))
                            .child(div().flex().flex_wrap().gap_5().children(
                                settings.themes.iter().enumerate().map(|(index, preview)| {
                                    let selected = preview.colors
                                        == [theme::bg(), theme::surface(), theme::accent()];
                                    div()
                                        .id(("settings-theme", index))
                                        .w(px(88.0))
                                        .flex()
                                        .flex_col()
                                        .gap_3()
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let settings = this.settings.as_mut().unwrap();
                                            settings.error =
                                                theme::select_preview(&settings.themes[index])
                                                    .err();
                                            cx.notify();
                                        }))
                                        .child(theme_swatch(preview.colors, selected))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(if selected {
                                                    theme::theme_text()
                                                } else {
                                                    theme::muted()
                                                }))
                                                .child(preview.name.replace('-', " ")),
                                        )
                                }),
                            )),
                    )
                    .child(
                        div()
                            .id("settings-telemetry")
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_3()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.settings.as_mut().unwrap().error =
                                    theme::set_telemetry(!theme::telemetry_enabled()).err();
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .text_color(rgb(theme::muted()))
                                    .child(TELEMETRY_LABEL),
                            )
                            .child(telemetry_switch()),
                    )
                    .when_some(settings.error.clone(), |el, error| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::error_text()))
                                .child(error),
                        )
                    }),
            )
            .into_any_element()
    }
}
