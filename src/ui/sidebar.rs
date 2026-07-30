mod project;
mod thread;

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, CursorStyle, DragMoveEvent, IntoElement, PathBuilder, Pixels, Point,
    Window, canvas, deferred, div, point, prelude::*, px,
};

use crate::{
    app::{Dirigent, KeyboardMode, SidebarMenu},
    model::{HarnessStatus, Id},
    theme::{
        blue, border, faint, muted, orange, red, rgb, surface, surface_hover, theme_text, yellow,
    },
};

#[derive(Clone, Copy)]
struct SidebarResizeDrag {
    width: f32,
    mouse_x: Pixels,
}

struct SidebarResizePreview;

impl Render for SidebarResizePreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThreadPlacement {
    Inbox,
    Workpool,
    Project,
}

fn add_icon_dot(path: &mut PathBuilder, center: Point<Pixels>) {
    let radius = px(1.25);
    path.move_to(point(center.x + radius, center.y));
    path.arc_to(
        point(radius, radius),
        px(0.0),
        false,
        true,
        point(center.x - radius, center.y),
    );
    path.arc_to(
        point(radius, radius),
        px(0.0),
        false,
        true,
        point(center.x + radius, center.y),
    );
    path.close();
}

fn ellipsis_vertical_icon(color: u32) -> impl IntoElement {
    canvas(
        |bounds, _, _| {
            let center = bounds.center();
            let mut dots = PathBuilder::fill();
            for offset in [-px(4.67), px(0.0), px(4.67)] {
                add_icon_dot(&mut dots, point(center.x, center.y + offset));
            }
            dots.build().ok()
        },
        move |_, dots, window, _| {
            if let Some(dots) = dots {
                window.paint_path(dots, rgb(color));
            }
        },
    )
    .size(px(16.0))
    .flex_none()
}

fn grip_vertical_icon(color: u32) -> impl IntoElement {
    canvas(
        |bounds, _, _| {
            let center = bounds.center();
            let mut dots = PathBuilder::fill();
            for x in [-px(2.0), px(2.0)] {
                for y in [-px(4.67), px(0.0), px(4.67)] {
                    add_icon_dot(&mut dots, point(center.x + x, center.y + y));
                }
            }
            dots.build().ok()
        },
        move |_, dots, window, _| {
            if let Some(dots) = dots {
                window.paint_path(dots, rgb(color));
            }
        },
    )
    .size(px(16.0))
    .flex_none()
}

fn plus_icon(color: u32) -> impl IntoElement {
    canvas(
        |bounds, _, _| {
            let center = bounds.center();
            let radius = px(4.67);
            let mut plus = PathBuilder::stroke(px(1.33));
            plus.move_to(point(center.x - radius, center.y));
            plus.line_to(point(center.x + radius, center.y));
            plus.move_to(point(center.x, center.y - radius));
            plus.line_to(point(center.x, center.y + radius));
            plus.build().ok()
        },
        move |_, plus, window, _| {
            if let Some(plus) = plus {
                window.paint_path(plus, rgb(color));
            }
        },
    )
    .size(px(16.0))
    .flex_none()
}

fn chevron_icon(expanded: bool) -> impl IntoElement {
    canvas(
        move |bounds, _, _| {
            let center = bounds.center();
            let center_x = center.x - px(4.0);
            let mut chevron = PathBuilder::stroke(px(1.33));
            if expanded {
                chevron.move_to(point(center_x - px(4.0), center.y - px(2.0)));
                chevron.line_to(point(center_x, center.y + px(2.0)));
                chevron.line_to(point(center_x + px(4.0), center.y - px(2.0)));
            } else {
                chevron.move_to(point(center_x - px(2.0), center.y - px(4.0)));
                chevron.line_to(point(center_x + px(2.0), center.y));
                chevron.line_to(point(center_x - px(2.0), center.y + px(4.0)));
            }
            chevron.build().ok()
        },
        |_, chevron, window, _| {
            if let Some(chevron) = chevron {
                window.paint_path(chevron, rgb(muted()));
            }
        },
    )
    .size(px(16.0))
    .flex_none()
}

fn archive_icon(color: u32) -> impl IntoElement {
    canvas(
        |bounds, _, _| {
            let center = bounds.center();
            let mut archive = PathBuilder::stroke(px(1.33));

            archive.move_to(point(center.x - px(6.67), center.y - px(6.0)));
            archive.line_to(point(center.x + px(6.67), center.y - px(6.0)));
            archive.line_to(point(center.x + px(6.67), center.y - px(2.67)));
            archive.line_to(point(center.x - px(6.67), center.y - px(2.67)));
            archive.close();

            archive.move_to(point(center.x - px(5.33), center.y - px(2.67)));
            archive.line_to(point(center.x - px(5.33), center.y + px(4.67)));
            archive.arc_to(
                point(px(1.33), px(1.33)),
                px(0.0),
                false,
                false,
                point(center.x - px(4.0), center.y + px(6.0)),
            );
            archive.line_to(point(center.x + px(4.0), center.y + px(6.0)));
            archive.arc_to(
                point(px(1.33), px(1.33)),
                px(0.0),
                false,
                false,
                point(center.x + px(5.33), center.y + px(4.67)),
            );
            archive.line_to(point(center.x + px(5.33), center.y - px(2.67)));

            archive.move_to(point(center.x - px(1.33), center.y));
            archive.line_to(point(center.x + px(1.33), center.y));
            archive.build().ok()
        },
        move |_, archive, window, _| {
            if let Some(archive) = archive {
                window.paint_path(archive, rgb(color));
            }
        },
    )
    .size(px(16.0))
    .flex_none()
}

fn hover_icon(
    normal: impl IntoElement,
    hovered: impl IntoElement,
    group: String,
) -> impl IntoElement {
    div().relative().size(px(16.0)).child(normal).child(
        div()
            .absolute()
            .top_0()
            .left_0()
            .invisible()
            .group_hover(group, |style| style.visible())
            .child(hovered),
    )
}

fn format_duration(duration: Duration) -> String {
    let elapsed = duration.as_secs();
    if elapsed < 60 {
        format!("{elapsed}s")
    } else if elapsed < 3_600 {
        format!("{}m {:02}s", elapsed / 60, elapsed % 60)
    } else {
        format!("{}h {:02}m", elapsed / 3_600, (elapsed % 3_600) / 60)
    }
}

fn format_elapsed(started_at: Option<Instant>) -> String {
    format_duration(
        started_at
            .map(|started_at| started_at.elapsed())
            .unwrap_or_default(),
    )
}

impl Dirigent {
    fn render_section_header(&self, title: &'static str, count: usize, color: u32) -> AnyElement {
        div()
            .h(px(28.0))
            .px_2()
            .flex()
            .items_center()
            .text_xs()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(rgb(color))
            .child(title)
            .child(div().flex_1())
            .when(count > 0, |element| element.child(count.to_string()))
            .into_any_element()
    }

    fn render_bottom_sidebar_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        deferred(
            div()
                .id("sidebar-bottom-dropdown")
                .absolute()
                .right(px(12.0))
                .bottom(px(46.0))
                .w(px(150.0))
                .p_1()
                .flex()
                .flex_col()
                .rounded_md()
                .border_1()
                .border_color(rgb(border()))
                .bg(rgb(crate::theme::menu_bg()))
                .shadow_lg()
                .occlude()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                )
                .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                .child(
                    div()
                        .id("add-project")
                        .h(px(32.0))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .text_xs()
                        .text_color(rgb(muted()))
                        .hover(|style| style.bg(rgb(surface_hover())).text_color(rgb(theme_text())))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.begin_adding_project();
                            this.enter_input_mode(true);
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .child("Add project"),
                ),
        )
        .priority(2)
        .into_any_element()
    }

    pub(crate) fn render_sidebar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut inbox_ids = self
            .harnesses
            .iter()
            .filter(|harness| harness.is_in_inbox())
            .map(|harness| harness.id)
            .collect::<Vec<_>>();
        let mut workpool_ids = self
            .harnesses
            .iter()
            .filter(|harness| harness.is_in_workpool())
            .map(|harness| harness.id)
            .collect::<Vec<_>>();
        let order = |id: &Id| {
            std::cmp::Reverse(
                self.harnesses
                    .iter()
                    .find(|harness| harness.id == *id)
                    .unwrap()
                    .sidebar_order,
            )
        };
        inbox_ids.sort_by_key(order);
        workpool_ids.sort_by_key(order);

        let project_ids = self
            .projects
            .iter()
            .map(|project| project.id)
            .collect::<Vec<_>>();
        let bottom_menu_open = self.sidebar_menu == Some(SidebarMenu::Bottom);
        let resize_drag = SidebarResizeDrag {
            width: self.sidebar_width,
            mouse_x: window.mouse_position().x,
        };

        div()
            .relative()
            .w(px(self.sidebar_width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(rgb(border()))
            .bg(rgb(crate::theme::sidebar_bg()))
            .child(
                div()
                    .id("project-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_3()
                    .pt_3()
                    .when(!inbox_ids.is_empty(), |element| {
                        element
                            .child(self.render_section_header("Inbox", inbox_ids.len(), yellow()))
                            .child(div().flex().flex_col().gap_1().children(
                                inbox_ids.into_iter().map(|id| {
                                    self.render_sidebar_harness(id, ThreadPlacement::Inbox, cx)
                                }),
                            ))
                            .child(div().h(px(10.0)))
                    })
                    .when(!workpool_ids.is_empty(), |element| {
                        element
                            .child(self.render_section_header(
                                "Workpool",
                                workpool_ids.len(),
                                blue(),
                            ))
                            .child(div().flex().flex_col().gap_1().children(
                                workpool_ids.into_iter().map(|id| {
                                    self.render_sidebar_harness(id, ThreadPlacement::Workpool, cx)
                                }),
                            ))
                            .child(div().h(px(14.0)))
                    })
                    .child(self.render_section_header("Projects", 0, faint()))
                    .child(
                        div().mt(px(-8.0)).children(
                            project_ids
                                .into_iter()
                                .map(|id| self.render_sidebar_project(id, cx)),
                        ),
                    ),
            )
            .child(
                div()
                    .relative()
                    .h(px(56.0))
                    .px_4()
                    .flex_none()
                    .flex()
                    .items_center()
                    .when(bottom_menu_open, |element| {
                        element.child(self.render_bottom_sidebar_menu(cx))
                    })
                    .child(
                        div()
                            .relative()
                            .top(px(3.0))
                            .mr_3()
                            .text_xs()
                            .text_color(rgb(if self.keyboard_mode == KeyboardMode::Input {
                                blue()
                            } else {
                                muted()
                            }))
                            .child(self.keyboard_mode.label()),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("sidebar-bottom-menu-trigger")
                            .group("sidebar-bottom-menu-trigger")
                            .size(px(30.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|_, _, _, cx| cx.stop_propagation()),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if bottom_menu_open {
                                    this.sidebar_menu = None;
                                } else {
                                    this.toggle_bottom_sidebar_menu();
                                }
                                cx.stop_propagation();
                                cx.notify();
                            }))
                            .child(hover_icon(
                                ellipsis_vertical_icon(muted()),
                                ellipsis_vertical_icon(blue()),
                                "sidebar-bottom-menu-trigger".to_string(),
                            )),
                    ),
            )
            .child(
                div()
                    .id("sidebar-resize-handle")
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right(px(-3.0))
                    .w(px(7.0))
                    .cursor(CursorStyle::ResizeColumn)
                    .hover(|style| style.bg(rgb(blue())))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .on_drag(resize_drag, |_, _, _, cx| cx.new(|_| SidebarResizePreview))
                    .on_drag_move::<SidebarResizeDrag>(cx.listener(
                        |this, event: &DragMoveEvent<SidebarResizeDrag>, _, cx| {
                            let drag = event.drag(cx);
                            let delta = event.event.position.x - drag.mouse_x;
                            this.resize_sidebar(drag.width + delta.as_f32());
                            cx.notify();
                        },
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{format_duration, format_elapsed};
    use std::time::{Duration, Instant};

    #[test]
    fn formats_thread_run_times() {
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
        assert!(
            format_elapsed(Some(Instant::now() - Duration::from_secs(65))).starts_with("1m 05s")
        );
    }
}
