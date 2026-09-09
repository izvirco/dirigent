//! First-startup introduction, shown when config.toml did not exist at launch.
use std::time::Instant;

use crate::{
    app::Dirigent,
    theme::{self, rgb},
};
use gpui::{AnyElement, Context, FontWeight, Window, div, prelude::*, px};

pub(crate) struct Onboarding {
    started: Option<Instant>,
    leaving: Option<Instant>,
    themes: Vec<theme::ThemePreview>,
    selected: Option<usize>,
    error: Option<String>,
    dependencies: [(&'static str, bool); 2],
}

impl Onboarding {
    pub(crate) fn new() -> Self {
        let themes = theme::theme_previews();
        let selected = themes
            .iter()
            .position(|t| t.colors == [theme::bg(), theme::surface(), theme::accent()]);
        Self {
            started: None,
            leaving: None,
            themes,
            selected,
            error: None,
            dependencies: [
                ("pi", executable_on_path("pi")),
                ("Node.js", executable_on_path("node")),
            ],
        }
    }
}

// Check the same PATH inherited by child processes, without running tools on the UI thread.
fn executable_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        #[cfg(target_os = "windows")]
        {
            ["exe", "cmd", "bat"]
                .iter()
                .any(|extension| directory.join(format!("{name}.{extension}")).is_file())
        }
        #[cfg(not(target_os = "windows"))]
        {
            use std::os::unix::fs::PermissionsExt;
            directory.join(name).metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        }
    })
}

fn ease(value: f32) -> f32 {
    let t = value.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl Dirigent {
    pub(crate) fn finish_onboarding_fade(&mut self) {
        if self.onboarding.as_ref().is_some_and(|state| {
            state
                .leaving
                .is_some_and(|start| start.elapsed().as_secs_f32() >= 0.6)
        }) {
            self.onboarding = None;
        }
    }

    pub(crate) fn render_onboarding(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.onboarding.as_mut().unwrap();
        let elapsed = state
            .started
            .get_or_insert_with(Instant::now)
            .elapsed()
            .as_secs_f32();
        let leaving = state.leaving.map(|start| start.elapsed().as_secs_f32());
        if elapsed < 5.4 || leaving.is_some() {
            window.request_animation_frame();
        }
        let quote = elapsed < 4.6;
        let opacity = if quote {
            ease(elapsed / 0.9) * (1.0 - ease((elapsed - 3.0) / 0.8))
        } else {
            ease((elapsed - 4.6) / 0.8) * (1.0 - ease(leaving.unwrap_or(0.0) / 0.6))
        };
        let content = if quote {
            div().max_w(px(680.0)).flex().flex_col().gap_5()
                .child(div().text_size(px(22.0)).line_height(px(34.0))
                    .child("“A computer can never be held accountable. Therefore a computer must never make a management decision.”"))
                .child(div().text_sm().text_color(rgb(theme::muted())).child("IBM, 1979"))
                .into_any_element()
        } else {
            div().w_full().max_w(px(640.0)).flex().flex_col().items_start().text_left()
                .child(div().text_size(px(52.0)).font_weight(FontWeight::SEMIBOLD).child("Dirigent"))
                .child(div().mt_4().max_w(px(640.0)).text_left().text_size(px(15.0)).line_height(px(25.0))
                    .text_color(rgb(theme::muted()))
                    .child("Orchestrate your clankers from a decent-ish UI, and watch them fuck up your codebase in ways you can't even imagine yet."))
                .child(div().mt(px(56.0)).text_sm().child("Pick a UI flavour"))
                .child(div().mt_6().flex().flex_wrap().gap_5()
                    .children(state.themes.iter().enumerate().map(|(index, preview)| {
                        let selected = state.selected == Some(index);
                        div().id(("onboarding-theme", index)).w(px(88.0)).flex().flex_col().items_start().gap_3()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let state = this.onboarding.as_mut().unwrap();
                                if state.leaving.is_some() { return; }
                                match theme::select_preview(&state.themes[index]) {
                                    Ok(()) => { state.selected = Some(index); state.error = None; }
                                    Err(error) => state.error = Some(error),
                                }
                                cx.notify();
                            }))
                            .child(super::settings::theme_swatch(preview.colors, selected))
                            .child(div().text_xs().text_color(rgb(if selected { theme::theme_text() } else { theme::muted() })).child(preview.name.replace('-', " ")))
                    })))
                .child(div().mt_8().flex().flex_col().gap_2()
                    .children(state.dependencies.iter().map(|(name, present)| {
                        div().text_xs().text_color(rgb(if *present { theme::muted() } else { theme::warning_text() }))
                            .child(format!("{name} {}", if *present { "found on PATH" } else { "not found on PATH" }))
                    })))
                .child(div().id("onboarding-telemetry").mt_8().flex().items_center().gap_3().cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let state = this.onboarding.as_mut().unwrap();
                        if state.leaving.is_some() { return; }
                        state.error = theme::set_telemetry(!theme::telemetry_enabled()).err();
                        cx.notify();
                    }))
                    .child(div().flex_1().text_xs().text_color(rgb(theme::muted()))
                        .child(super::settings::TELEMETRY_LABEL))
                    .child(super::settings::telemetry_switch()))
                .when_some(state.error.clone(), |el, error| el.child(div().mt_4().text_xs().text_color(rgb(theme::error_text())).child(error)))
                .child(div().id("onboarding-enter").mt_6().px_6().py_2().rounded_lg()
                    .bg(rgb(theme::accent())).text_color(rgb(theme::bg())).text_sm().font_weight(FontWeight::MEDIUM)
                    .cursor_pointer().hover(|s| s.bg(rgb(theme::accent_hover())))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.onboarding.as_mut().unwrap().leaving.get_or_insert_with(Instant::now);
                        cx.notify();
                    })).child("Enter"))
                .into_any_element()
        };
        div()
            .id("onboarding")
            .size_full()
            .overflow_y_scroll()
            .bg(rgb(theme::bg()))
            .font_family(self.font.clone())
            .text_color(rgb(theme::theme_text()))
            .child(
                // Match the app root + center panel's translucent background layers.
                div()
                    .bg(rgb(theme::bg()))
                    .min_h_full()
                    .w_full()
                    .p_8()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .opacity(opacity)
                            .child(content),
                    ),
            )
            .into_any_element()
    }
}
