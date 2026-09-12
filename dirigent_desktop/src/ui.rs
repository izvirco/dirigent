//! Collects GPUI views and shared UI rendering helpers.

mod about;
pub(crate) mod usage;
pub(crate) mod settings;
pub(crate) mod onboarding;
mod composer;
mod conversation;
mod delegation;
mod diff_sidebar;

pub(crate) use conversation::{ConversationRenderCache, ConversationScrollAnchor};
pub(crate) use diff_sidebar::DiffRenderCache;
mod extension_dialog;
mod keyboard_menu;
mod markdown;
mod project_settings;
mod project_setup;
mod sidebar;
mod workspace_delete_dialog;

use std::{cell::Cell, rc::Rc};

use gpui::{
    AnyElement, Context, CursorStyle, DragMoveEvent, IntoElement, MouseButton, Pixels,
    ScrollHandle, Window, deferred, div, point, prelude::*, px, relative, rgba,
};

use crate::{
    app::{Dirigent, PathCompletionTarget},
    theme::{bg, border, muted, orange, rgb, theme_text},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThinScrollbarAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone)]
struct ThinScrollbarDrag {
    id: String,
    axis: ThinScrollbarAxis,
    handle: ScrollHandle,
    start_pointer: Rc<Cell<Pixels>>,
    start_offset: f32,
    max_offset: f32,
    thumb_travel: f32,
}

impl ThinScrollbarDrag {
    fn scroll_to_pointer(&self, pointer: Pixels) {
        let offset = scrollbar_drag_offset(
            self.start_offset,
            (pointer - self.start_pointer.get()).as_f32(),
            self.max_offset,
            self.thumb_travel,
        );
        let current = self.handle.offset();
        self.handle.set_offset(match self.axis {
            ThinScrollbarAxis::Horizontal => point(px(offset), current.y),
            ThinScrollbarAxis::Vertical => point(current.x, px(offset)),
        });
    }
}

struct ThinScrollbarDragPreview;

impl gpui::Render for ThinScrollbarDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn scrollbar_drag_offset(
    start_offset: f32,
    pointer_delta: f32,
    max_offset: f32,
    travel: f32,
) -> f32 {
    if travel > 0.0 {
        (start_offset - pointer_delta * max_offset / travel).clamp(-max_offset, 0.0)
    } else {
        0.0
    }
}

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
        deferred(
            div()
                .id("path-completion-menu")
                .w_full()
                .p_1()
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
                                .h(px(26.0))
                                .w_full()
                                .px_1()
                                .flex()
                                .items_center()
                                .rounded_md()
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
        cx: &mut Context<Self>,
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
        let drag = ThinScrollbarDrag {
            id: id.clone(),
            axis: ThinScrollbarAxis::Vertical,
            handle: handle.clone(),
            start_pointer: Rc::new(Cell::new(px(0.0))),
            start_offset: handle.offset().y.as_f32(),
            max_offset,
            thumb_travel: (viewport - 6.0).max(0.0) * (1.0 - thumb_fraction),
        };
        let drag_id = id.clone();

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
                        .id(format!("{id}-thumb"))
                        .group(id.clone())
                        .absolute()
                        .top(relative(thumb_top))
                        .right(px(-3.0))
                        .h(relative(thumb_fraction))
                        .min_h(px(10.0))
                        .w(px(8.0))
                        .cursor(CursorStyle::Arrow)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        )
                        .on_drag(drag, |drag, _, window, cx| {
                            drag.start_pointer.set(window.mouse_position().y);
                            cx.new(|_| ThinScrollbarDragPreview)
                        })
                        .on_drag_move::<ThinScrollbarDrag>(cx.listener(
                            move |_, event: &DragMoveEvent<ThinScrollbarDrag>, _, cx| {
                                let drag = event.drag(cx);
                                if drag.id != drag_id || drag.axis != ThinScrollbarAxis::Vertical {
                                    return;
                                }
                                drag.scroll_to_pointer(event.event.position.y);
                                cx.notify();
                                cx.stop_propagation();
                            },
                        ))
                        .child(
                            div()
                                .ml(px(3.0))
                                .h_full()
                                .w(px(2.0))
                                .rounded_full()
                                .bg(rgb(muted()))
                                .group_hover(id, |style| style.bg(rgb(orange()))),
                        ),
                )
            })
            .into_any_element()
    }

    pub(super) fn render_thin_horizontal_scrollbar(
        &self,
        id: impl Into<String>,
        handle: &ScrollHandle,
        cx: &mut Context<Self>,
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
        let drag = ThinScrollbarDrag {
            id: id.clone(),
            axis: ThinScrollbarAxis::Horizontal,
            handle: handle.clone(),
            start_pointer: Rc::new(Cell::new(px(0.0))),
            start_offset: handle.offset().x.as_f32(),
            max_offset,
            thumb_travel: (viewport - 6.0).max(0.0) * (1.0 - thumb_fraction),
        };
        let drag_id = id.clone();

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
                        .id(format!("{id}-thumb"))
                        .group(id.clone())
                        .absolute()
                        .left(relative(thumb_left))
                        .bottom(px(-3.0))
                        .w(relative(thumb_fraction))
                        .min_w(px(10.0))
                        .h(px(8.0))
                        .cursor(CursorStyle::Arrow)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        )
                        .on_drag(drag, |drag, _, window, cx| {
                            drag.start_pointer.set(window.mouse_position().x);
                            cx.new(|_| ThinScrollbarDragPreview)
                        })
                        .on_drag_move::<ThinScrollbarDrag>(cx.listener(
                            move |_, event: &DragMoveEvent<ThinScrollbarDrag>, _, cx| {
                                let drag = event.drag(cx);
                                if drag.id != drag_id || drag.axis != ThinScrollbarAxis::Horizontal
                                {
                                    return;
                                }
                                drag.scroll_to_pointer(event.event.position.x);
                                cx.notify();
                                cx.stop_propagation();
                            },
                        ))
                        .child(
                            div()
                                .mt(px(3.0))
                                .h(px(2.0))
                                .w_full()
                                .rounded_full()
                                .bg(rgb(muted()))
                                .group_hover(id, |style| style.bg(rgb(orange()))),
                        ),
                )
            })
            .into_any_element()
    }

    pub(crate) fn render_center(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.usage.is_some() {
            return self.render_usage(cx);
        }
        if self.settings.is_some() {
            return self.render_settings(cx);
        }
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
            .when(self.about_open, |element| {
                element.child(self.render_about(cx))
            })
            .when(!self.about_open, |element| {
                element.when_some(self.project_settings, |element, project_id| {
                    element.child(self.render_project_settings(project_id, cx))
                })
            })
            .when(
                !self.about_open && self.project_settings.is_none() && self.adding_project,
                |element| element.child(self.render_add_project(cx)),
            )
            .when(
                !self.about_open
                    && self.project_settings.is_none()
                    && !self.adding_project
                    && self.creating_harness,
                |element| element.child(self.render_new_harness(window, cx)),
            )
            .when(
                !self.about_open
                    && self.project_settings.is_none()
                    && !self.adding_project
                    && !self.creating_harness
                    && self.selected_harness.is_some(),
                |element| {
                    element
                        .child(self.render_delegation(cx))
                        .child(self.render_conversation(cx))
                        .child(self.render_composer(window, cx))
                        .child(self.render_changes_toggle(cx))
                },
            )
            .when(
                !self.about_open
                    && self.project_settings.is_none()
                    && !self.adding_project
                    && !self.creating_harness
                    && self.selected_harness.is_none(),
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
                                    .max_w(px(680.))
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
                                            .child(div().child(
                                                "“A computer can never be held accountable.",
                                            ))
                                            .child(div().child(
                                                "Therefore a computer must never make a management decision.”",
                                            )),
                                    )
                                    .child("IBM, 1979"),
                            ),
                    )
                },
            )
            .when(self.pending_dialog.is_some(), |element| {
                element.child(self.render_extension_dialog(cx))
            })
            .into_any_element()
    }
}
