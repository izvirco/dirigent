mod composer;
mod conversation;
mod extension_dialog;
mod keyboard_menu;
mod markdown;
mod project_setup;
mod sidebar;

use gpui::{
    AnyElement, Context, IntoElement, ScrollHandle, Window, deferred, div, prelude::*, px,
    relative, rgb, rgba,
};

use crate::{
    app::{Dirigent, PathCompletionTarget},
    theme::{bg, border, muted, orange, theme_text},
};

impl Dirigent {
    pub(super) fn has_path_completion(&self, target: PathCompletionTarget) -> bool {
        self.path_completion
            .as_ref()
            .is_some_and(|completion| completion.target == target)
    }

    pub(super) fn render_path_completion_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(completion) = self.path_completion.as_ref() else {
            return div().into_any_element();
        };
        let selected = completion.selected;
        let last = completion.items.len().saturating_sub(1);
        deferred(
            div()
                .id("path-completion-menu")
                .w_full()
                .overflow_hidden()
                .rounded_lg()
                .border_1()
                .border_color(rgb(border()))
                .bg(rgb(crate::theme::popup_bg()))
                .occlude()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                )
                .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                .children(
                    completion
                        .items
                        .iter()
                        .cloned()
                        .enumerate()
                        .map(|(index, path)| {
                            div()
                                .id(("path-completion", index))
                                .h(px(24.0))
                                .w_full()
                                .px_2()
                                .flex()
                                .items_center()
                                .when(index == 0, |element| element.rounded_t_lg())
                                .when(index == last, |element| element.rounded_b_lg())
                                .when(index == selected, |element| {
                                    element.bg(rgb(crate::theme::selection()))
                                })
                                .when(index != selected, |element| {
                                    element.hover(|style| style.bg(rgb(border())))
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.choose_path_completion(index, cx);
                                    cx.stop_propagation();
                                }))
                                .child(
                                    div()
                                        .min_w(px(0.0))
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .text_xs()
                                        .text_color(rgb(theme_text()))
                                        .child(path),
                                )
                        }),
                ),
        )
        .priority(2)
        .into_any_element()
    }

    pub(super) fn render_thin_scrollbar(
        &self,
        id: impl Into<String>,
        handle: &ScrollHandle,
    ) -> AnyElement {
        let id = id.into();
        let viewport = handle.bounds().size.height.as_f32();
        let max_offset = handle.max_offset().y.as_f32();
        let thumb_fraction = if viewport > 0.0 && max_offset > 0.0 {
            let min_fraction = (10.0 / viewport).clamp(0.08, 1.0);
            (viewport / (viewport + max_offset)).clamp(min_fraction, 1.0)
        } else {
            1.0
        };
        let scroll_fraction = if max_offset > 0.0 {
            (-handle.offset().y.as_f32() / max_offset).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let thumb_top = (1.0 - thumb_fraction) * scroll_fraction;

        div()
            .id(id.clone())
            .absolute()
            .top(px(3.0))
            .bottom(px(3.0))
            .right(px(2.0))
            .w(px(2.0))
            .rounded_full()
            .when(max_offset > 0.0, |element| {
                element.bg(rgba(0xffffff16)).child(
                    div()
                        .absolute()
                        .top(relative(thumb_top))
                        .h(relative(thumb_fraction))
                        .min_h(px(10.0))
                        .w_full()
                        .rounded_full()
                        .bg(rgb(muted()))
                        .group_hover(id, |style| style.bg(rgb(orange()))),
                )
            })
            .into_any_element()
    }

    pub(super) fn render_thin_horizontal_scrollbar(
        &self,
        id: impl Into<String>,
        handle: &ScrollHandle,
    ) -> AnyElement {
        let id = id.into();
        let viewport = handle.bounds().size.width.as_f32();
        let max_offset = handle.max_offset().x.as_f32();
        let thumb_fraction = if viewport > 0.0 && max_offset > 0.0 {
            let min_fraction = (10.0 / viewport).clamp(0.08, 1.0);
            (viewport / (viewport + max_offset)).clamp(min_fraction, 1.0)
        } else {
            1.0
        };
        let scroll_fraction = if max_offset > 0.0 {
            (-handle.offset().x.as_f32() / max_offset).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let thumb_left = (1.0 - thumb_fraction) * scroll_fraction;

        div()
            .id(id.clone())
            .absolute()
            .left(px(3.0))
            .right(px(3.0))
            .bottom(px(2.0))
            .h(px(2.0))
            .rounded_full()
            .when(max_offset > 0.0, |element| {
                element.bg(rgba(0xffffff16)).child(
                    div()
                        .absolute()
                        .left(relative(thumb_left))
                        .w(relative(thumb_fraction))
                        .min_w(px(10.0))
                        .h_full()
                        .rounded_full()
                        .bg(rgb(muted()))
                        .group_hover(id, |style| style.bg(rgb(orange()))),
                )
            })
            .into_any_element()
    }

    pub(crate) fn render_center(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .relative()
            .min_w(px(420.0))
            .h_full()
            .flex_1()
            .flex()
            .flex_col()
            .bg(rgb(bg()))
            .when_some(self.banner.clone(), |element, banner| {
                element.child(
                    div()
                        .px_5()
                        .py_2()
                        .flex()
                        .items_center()
                        .gap_3()
                        .border_b_1()
                        .border_color(rgb(border()))
                        .bg(rgb(crate::theme::warning_bg()))
                        .text_xs()
                        .text_color(rgb(crate::theme::warning_text()))
                        .child(div().flex_1().child(banner))
                        .child(
                            div()
                                .id("dismiss-banner")
                                .text_color(rgb(muted()))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.banner = None;
                                    cx.notify();
                                }))
                                .child("×"),
                        ),
                )
            })
            .when(self.adding_project, |element| {
                element.child(self.render_add_project(cx))
            })
            .when(!self.adding_project && self.creating_harness, |element| {
                element.child(self.render_new_harness(window, cx))
            })
            .when(
                !self.adding_project && !self.creating_harness && self.selected_harness.is_some(),
                |element| {
                    element
                        .child(self.render_conversation(cx))
                        .child(self.render_composer(window, cx))
                },
            )
            .when(
                !self.adding_project && !self.creating_harness && self.selected_harness.is_none(),
                |element| {
                    element.child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(muted()))
                            .child(
                                div()
                                    .w_full()
                                    .max_w(px(560.))
                                    .mx_auto()
                                    .text_left()
                                    .flex()
                                    .flex_col()
                                    .gap_3()
                                    .child(
                                        div()
                                            .text_lg()
                                            .italic()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(rgb(theme_text()))
                                            .child(
                                                "“A computer can never be held accountable. \
                                                 Therefore a computer must never make a management \
                                                 decision.”",
                                            ),
                                    )
                                    .child("IBM, 1979"),
                            ),
                    )
                },
            )
            .when_some(self.keyboard_menu, |element, menu| {
                element.child(self.render_keyboard_menu(menu, cx))
            })
            .when(self.pending_dialog.is_some(), |element| {
                element.child(self.render_extension_dialog(cx))
            })
            .into_any_element()
    }
}
