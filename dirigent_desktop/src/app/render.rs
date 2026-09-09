//! Composes the application's top-level GPUI layout.

use super::*;

impl Render for Dirigent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let window_transparent = bg() & 0xff != 0xff;
        if self.window_transparent != Some(window_transparent) {
            window.set_background_appearance(if window_transparent {
                WindowBackgroundAppearance::Transparent
            } else {
                WindowBackgroundAppearance::Opaque
            });
            self.window_transparent = Some(window_transparent);
        }

        self.finish_onboarding_fade();
        if self.onboarding.is_some() {
            return self.render_onboarding(window, cx);
        }

        self.frame_timing.collect_frames(Instant::now());
        let frame_timing_labels = self.frame_timing.labels();
        self.sync_path_completion_input(cx);
        let path_completion_anchor = self.path_completion.as_ref().and_then(|completion| {
            let at = completion.replacement.start.checked_sub(1)?;
            self.path_completion_input(completion.target)?
                .read(cx)
                .position_for_offset(at)
        });
        let diff_replaces_thread = self.settings.is_none() && self.diff_sidebar_replaces_thread(window);

        if self.focus_normal_mode {
            self.focus_normal_mode = false;
            window.focus(&self.thread_focus, cx);
        } else if self.focus_input {
            self.focus_input = false;
            let input = if self.renaming_harness.is_some() {
                Some(self.thread_rename_input.clone())
            } else if self.pending_dialog.is_some() {
                Some(self.extension_input.clone())
            } else if let Some(edit) = self.editing_message.as_ref() {
                Some(edit.input.clone())
            } else if self.project_settings.is_some() && self.workspace_settings_editing {
                Some(self.workspace_settings_input.clone())
            } else if self.adding_project {
                Some(self.project_input.clone())
            } else if self.creating_harness {
                Some(self.harness_input.clone())
            } else {
                self.selected_composer_input()
            };
            if let Some(input) = input {
                window.focus(&input.focus_handle(cx), cx);
            }
        }

        div()
            .id("app-root")
            .relative()
            .size_full()
            .flex()
            .overflow_hidden()
            .bg(rgb(bg()))
            .font_family(self.font.clone())
            .font_features(FontFeatures::disable_ligatures())
            .text_color(rgb(theme_text()))
            .track_focus(&self.thread_focus)
            .on_key_down(cx.listener(Self::on_root_key_down))
            .on_key_up(cx.listener(Self::on_root_key_up))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    let mut changed = false;
                    if this.keyboard_mode == KeyboardMode::Input {
                        this.enter_normal_mode();
                        changed = true;
                    }
                    if this.keyboard_menu.is_some() {
                        this.close_keyboard_menu();
                        changed = true;
                    }
                    if this.sidebar_menu.take().is_some() {
                        changed = true;
                    }
                    if this.path_completion.take().is_some() {
                        changed = true;
                    }
                    if changed {
                        cx.notify();
                    }
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                let changed = this.composer_dropdown.take().is_some()
                    | std::mem::take(&mut this.diff_turn_dropdown_open);
                if changed {
                    cx.notify();
                }
            }))
            .when(
                self.composer_dropdown.is_some()
                    || self.sidebar_menu.is_some()
                    || self.path_completion.is_some(),
                |element| {
                    element.child(
                        deferred(
                            div()
                                .id("dropdown-dismiss-backdrop")
                                .absolute()
                                .inset_0()
                                .size_full()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let changed = this.composer_dropdown.take().is_some()
                                        | std::mem::take(&mut this.diff_turn_dropdown_open)
                                        | this.sidebar_menu.take().is_some()
                                        | this.path_completion.take().is_some();
                                    if changed {
                                        cx.notify();
                                    }
                                })),
                        )
                        .priority(1),
                    )
                },
            )
            .child(self.render_sidebar(window, cx))
            .when(!diff_replaces_thread, |element| {
                element.child(self.render_center(window, cx))
            })
            .when(self.settings.is_none(), |element| element.child(self.render_diff_sidebar(window, cx)))
            .when_some(self.keyboard_menu, |element, menu| {
                element.child(self.render_keyboard_menu(menu, cx))
            })
            .when_some(path_completion_anchor, |element, anchor| {
                let width = 520.0;
                let left = anchor.x.as_f32().clamp(
                    8.0,
                    (window.viewport_size().width.as_f32() - width - 8.0).max(8.0),
                );
                element.child(
                    div()
                        .absolute()
                        .left(px(left))
                        .bottom(window.viewport_size().height - anchor.y + px(2.0))
                        .w(px(width))
                        .child(self.render_path_completion_menu(cx)),
                )
            })
            .when_some(self.pending_workspace_deletion, |element, harness_id| {
                element.child(self.render_workspace_delete_dialog(harness_id, cx))
            })
            .when_some(self.preview_image.clone(), |element, image| {
                element.child(
                    div()
                        .id("image-preview-backdrop")
                        .absolute()
                        .inset_0()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p_8()
                        .bg(gpui::rgba(0x000000cc))
                        .occlude()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.preview_image = None;
                            cx.notify();
                        }))
                        .child(
                            div()
                                .id("image-preview")
                                .w(window.viewport_size().width * 0.60)
                                .h(window.viewport_size().height * 0.60)
                                .flex()
                                .items_center()
                                .justify_center()
                                .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                                .child(
                                    img(image)
                                        .size_full()
                                        .object_fit(ObjectFit::Contain)
                                        .rounded_lg(),
                                ),
                        ),
                )
            })
            .when(
                self.debug_panels_visible && self.keyboard_menu.is_none(),
                |element| {
                    element.child(
                        div()
                            .absolute()
                            .right(px(8.0))
                            .bottom(px(8.0))
                            .px_2()
                            .py_1()
                            .flex()
                            .flex_col()
                            .items_end()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(border()))
                            .bg(rgb(bg()).opacity(0.87))
                            .text_xs()
                            .text_right()
                            .text_color(rgb(muted()))
                            .children(
                                frame_timing_labels
                                    .into_iter()
                                    .map(|label| div().w_full().child(label)),
                            ),
                    )
                },
            )
            .into_any_element()
    }
}
