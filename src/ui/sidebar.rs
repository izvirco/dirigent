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

#[derive(Clone)]
struct ProjectDrag {
    id: Id,
    name: String,
    position: Point<Pixels>,
}

impl Render for ProjectDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .left(self.position.x - px(90.0))
            .top(self.position.y - px(18.0))
            .w(px(180.0))
            .h(px(36.0))
            .px_3()
            .flex()
            .items_center()
            .rounded_md()
            .border_1()
            .border_color(rgb(border()))
            .bg(rgb(surface()))
            .text_sm()
            .text_color(rgb(theme_text()))
            .child(self.name.clone())
    }
}

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
    fn render_thread_dropdown(
        &self,
        id: Id,
        archived: bool,
        top: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let archive_label = if archived { "Restore" } else { "Archive" };
        let archive_enabled = archived || self.can_archive_harness(id);
        let archive_item = div()
            .id(("archive-thread", id as usize))
            .h(px(30.0))
            .px_2()
            .flex()
            .items_center()
            .rounded_sm()
            .text_xs()
            .text_color(rgb(if archive_enabled { muted() } else { faint() }))
            .when(archive_enabled, |element| {
                element
                    .hover(|style| style.bg(rgb(surface_hover())).text_color(rgb(theme_text())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_harness_archived(id, !archived);
                        cx.stop_propagation();
                        cx.notify();
                    }))
            })
            .child(archive_label);

        deferred(
            div()
                .id(("thread-dropdown", id as usize))
                .absolute()
                .top(px(top))
                .right(px(2.0))
                .w(px(132.0))
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
                        .id(("rename-thread", id as usize))
                        .h(px(30.0))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .text_xs()
                        .text_color(rgb(muted()))
                        .hover(|style| style.bg(rgb(surface_hover())).text_color(rgb(theme_text())))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.begin_renaming_harness(id, cx);
                            cx.stop_propagation();
                        }))
                        .child("Rename"),
                )
                .child(archive_item)
                .child(
                    div()
                        .id(("delete-thread", id as usize))
                        .h(px(30.0))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .text_xs()
                        .text_color(rgb(red()))
                        .hover(|style| style.bg(rgb(surface_hover())))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.delete_harness(id);
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .child("Delete"),
                ),
        )
        .priority(2)
        .into_any_element()
    }

    fn render_sidebar_harness(
        &self,
        id: Id,
        placement: ThreadPlacement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let harness = self
            .harnesses
            .iter()
            .find(|harness| harness.id == id)
            .unwrap();
        let active = self.selected_harness == Some(id) && !self.adding_project;
        let title = harness
            .title
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let status = harness.status;
        let has_unread = harness.has_unread_completion;
        let attention_required = harness.attention_required;
        let archived = harness.archived;
        let quick_archive = !archived && self.can_archive_harness(id);
        let show_notification = has_unread && placement != ThreadPlacement::Workpool;
        let menu_open = self.sidebar_menu == Some(SidebarMenu::Thread(id));
        let renaming = self.renaming_harness == Some(id);
        let group = format!("sidebar-harness-{id}");
        let archive_group = format!("quick-archive-thread-{id}");
        let menu_group = format!("thread-menu-trigger-{id}");
        let row_background = if active {
            surface()
        } else {
            crate::theme::sidebar_bg()
        };
        let project_name = self
            .projects
            .iter()
            .find(|project| project.id == harness.project_id)
            .map(|project| project.name.as_str())
            .unwrap_or("Unknown project");
        let detailed = placement != ThreadPlacement::Project;
        let height = if detailed { 48.0 } else { 34.0 };
        let (state_label, state_color) = if placement == ThreadPlacement::Workpool {
            (format_elapsed(harness.run_started_at), muted())
        } else if attention_required {
            ("Needs input".to_string(), orange())
        } else {
            match status {
                HarnessStatus::Failed => ("Failed".to_string(), red()),
                HarnessStatus::Stopped => ("Stopped".to_string(), faint()),
                HarnessStatus::Starting => ("Starting".to_string(), muted()),
                HarnessStatus::Working => (format_elapsed(harness.run_started_at), muted()),
                HarnessStatus::Idle => (
                    harness.last_run_duration.map_or_else(
                        || "Finished".to_string(),
                        |duration| format!("Finished in {}", format_duration(duration)),
                    ),
                    muted(),
                ),
            }
        };

        div()
            .id(("sidebar-harness", id as usize))
            .group(group.clone())
            .relative()
            .h(px(height))
            .px_2()
            .flex()
            .items_center()
            .rounded_md()
            .when(active, |style| style.bg(rgb(surface())))
            .hover(|style| style.bg(rgb(surface_hover())))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_harness(id);
                this.sidebar_menu = None;
                cx.notify();
            }))
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .child(
                        div()
                            .h(px(if detailed { 21.0 } else { 34.0 }))
                            .line_height(px(if detailed { 21.0 } else { 34.0 }))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .line_clamp(1)
                            .text_sm()
                            .font_weight(if active || has_unread {
                                gpui::FontWeight::SEMIBOLD
                            } else {
                                gpui::FontWeight::NORMAL
                            })
                            .text_color(rgb(if archived {
                                muted()
                            } else if active {
                                theme_text()
                            } else {
                                muted()
                            }))
                            .when(renaming, |element| {
                                element.child(self.thread_rename_input.clone())
                            })
                            .when(!renaming, |element| element.child(title)),
                    )
                    .when(detailed && !renaming, |element| {
                        element.child(
                            div()
                                .h(px(18.0))
                                .w_full()
                                .flex()
                                .items_center()
                                .whitespace_nowrap()
                                .text_xs()
                                .text_color(rgb(faint()))
                                .child(
                                    div()
                                        .min_w(px(0.0))
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(project_name.to_string()),
                                )
                                .child(div().flex_1())
                                .child(
                                    div()
                                        .flex_none()
                                        .text_color(rgb(state_color))
                                        .child(state_label),
                                ),
                        )
                    }),
            )
            .when(show_notification || !has_unread, |element| {
                element.child(
                    div()
                        .id(("thread-hover-controls", id as usize))
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right(px(2.0))
                        .flex()
                        .items_center()
                        .when(!show_notification, |controls| {
                            controls.bg(rgb(surface_hover()))
                        })
                        .when(!show_notification && !menu_open, |controls| {
                            controls
                                .invisible()
                                .group_hover(group.clone(), |style| style.visible())
                        })
                        .when(quick_archive, |controls| {
                            controls.child(
                                div()
                                    .id(("quick-archive-thread", id as usize))
                                    .group(archive_group.clone())
                                    .w(px(26.0))
                                    .h_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .when(show_notification, |archive| {
                                        archive.invisible().group_hover(group.clone(), |style| {
                                            style.visible().bg(rgb(surface_hover()))
                                        })
                                    })
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_harness_archived(id, true);
                                        cx.stop_propagation();
                                        cx.notify();
                                    }))
                                    .child(hover_icon(
                                        archive_icon(muted()),
                                        archive_icon(blue()),
                                        archive_group,
                                    )),
                            )
                        })
                        .when(show_notification, |controls| {
                            controls.child(
                                div()
                                    .id(("thread-unread-indicator", id as usize))
                                    .w(px(26.0))
                                    .h_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(rgb(row_background))
                                    .group_hover(group.clone(), |style| {
                                        style.bg(rgb(surface_hover()))
                                    })
                                    .child(div().size(px(7.0)).rounded_full().bg(rgb(blue()))),
                            )
                        })
                        .when(!has_unread, |controls| {
                            controls.child(
                                div()
                                    .id(("thread-menu-trigger", id as usize))
                                    .group(menu_group.clone())
                                    .w(px(26.0))
                                    .h_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_base()
                                    .text_color(rgb(muted()))
                                    .hover(|style| style.text_color(rgb(theme_text())))
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_thread_menu(id);
                                        cx.stop_propagation();
                                        cx.notify();
                                    }))
                                    .child(hover_icon(
                                        ellipsis_vertical_icon(muted()),
                                        ellipsis_vertical_icon(blue()),
                                        menu_group,
                                    )),
                            )
                        }),
                )
            })
            .when(menu_open && !has_unread, |element| {
                element.child(self.render_thread_dropdown(id, archived, height - 2.0, cx))
            })
            .into_any_element()
    }

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

    fn render_project_dropdown(&self, id: Id, cx: &mut Context<Self>) -> AnyElement {
        deferred(
            div()
                .id(("project-dropdown", id as usize))
                .absolute()
                .top(px(30.0))
                .right(px(8.0))
                .w(px(170.0))
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
                        .id(("delete-project", id as usize))
                        .h(px(30.0))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .text_xs()
                        .text_color(rgb(red()))
                        .hover(|style| style.bg(rgb(surface_hover())))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.delete_project(id);
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .child("Delete"),
                ),
        )
        .priority(2)
        .into_any_element()
    }

    fn render_sidebar_project(&self, id: Id, cx: &mut Context<Self>) -> AnyElement {
        let project = self
            .projects
            .iter()
            .find(|project| project.id == id)
            .unwrap();
        let collapsed = self.collapsed_projects.contains(&id);
        let mut harness_ids = self
            .harnesses
            .iter()
            .filter(|harness| harness.project_id == id && harness.archived)
            .map(|harness| harness.id)
            .collect::<Vec<_>>();
        harness_ids.sort_by_key(|harness_id| {
            std::cmp::Reverse(
                self.harnesses
                    .iter()
                    .find(|harness| harness.id == *harness_id)
                    .unwrap()
                    .sidebar_order,
            )
        });
        let selected = self.selected_project == Some(id) && !self.adding_project;
        let menu_open = self.sidebar_menu == Some(SidebarMenu::Project(id));
        let name = project.name.clone();
        let group = format!("sidebar-project-{id}");
        let grip_group = format!("project-drag-handle-{id}");
        let menu_group = format!("project-menu-trigger-{id}");
        let plus_group = format!("new-harness-{id}");
        let drag = ProjectDrag {
            id,
            name: name.clone(),
            position: Point::default(),
        };

        div()
            .id(("sidebar-project", id as usize))
            .relative()
            .drag_over::<ProjectDrag>(move |style, dragged, _, _| {
                if dragged.id != id {
                    style.border_t_1().border_color(rgb(blue()))
                } else {
                    style
                }
            })
            .on_drop(cx.listener(move |this, dragged: &ProjectDrag, _, cx| {
                this.reorder_project(dragged.id, id);
                cx.notify();
            }))
            .child(
                div()
                    .id(("project-header", id as usize))
                    .group(group.clone())
                    .relative()
                    .min_h(px(32.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .text_sm()
                    .text_color(rgb(if selected {
                        theme_text()
                    } else {
                        crate::theme::secondary_text()
                    }))
                    .child(
                        div()
                            .id(("collapse-project", id as usize))
                            .min_w(px(0.0))
                            .h(px(26.0))
                            .flex_1()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|_, _, _, cx| cx.stop_propagation()),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_project_collapsed(id);
                                cx.stop_propagation();
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .w(px(16.0))
                                    .h_full()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_color(rgb(faint()))
                                    .hover(|style| style.text_color(rgb(theme_text())))
                                    .child(chevron_icon(!collapsed)),
                            )
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .flex_1()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(name),
                            ),
                    )
                    .child(
                        div()
                            .id(("project-hover-controls", id as usize))
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right(px(8.0))
                            .flex()
                            .items_center()
                            .when(!menu_open, |controls| {
                                controls
                                    .invisible()
                                    .group_hover(group, |style| style.visible())
                            })
                            .child(
                                div()
                                    .id(("project-drag-handle", id as usize))
                                    .group(grip_group.clone())
                                    .size(px(20.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_move()
                                    .on_drag(drag, |drag: &ProjectDrag, position, _, cx| {
                                        let mut preview = drag.clone();
                                        preview.position = position;
                                        cx.new(|_| preview)
                                    })
                                    .child(hover_icon(
                                        grip_vertical_icon(muted()),
                                        grip_vertical_icon(blue()),
                                        grip_group,
                                    )),
                            )
                            .child(
                                div()
                                    .id(("project-menu-trigger", id as usize))
                                    .group(menu_group.clone())
                                    .size(px(26.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .hover(|style| style.shadow_sm().text_color(rgb(theme_text())))
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_project_menu(id);
                                        cx.stop_propagation();
                                        cx.notify();
                                    }))
                                    .child(hover_icon(
                                        ellipsis_vertical_icon(muted()),
                                        ellipsis_vertical_icon(blue()),
                                        menu_group,
                                    )),
                            )
                            .child(
                                div()
                                    .id(("new-harness", id as usize))
                                    .group(plus_group.clone())
                                    .size(px(26.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .text_color(rgb(faint()))
                                    .hover(|style| style.shadow_sm().text_color(rgb(theme_text())))
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.start_new_harness(id);
                                        this.enter_input_mode(true);
                                        cx.stop_propagation();
                                        cx.notify();
                                    }))
                                    .child(hover_icon(
                                        plus_icon(muted()),
                                        plus_icon(blue()),
                                        plus_group,
                                    )),
                            ),
                    ),
            )
            .when(menu_open, |element| {
                element.child(self.render_project_dropdown(id, cx))
            })
            .when(!collapsed, |element| {
                element.child(
                    div()
                        .ml_1()
                        .mr_1()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(harness_ids.iter().copied().map(|harness_id| {
                            self.render_sidebar_harness(harness_id, ThreadPlacement::Project, cx)
                        }))
                        .when(harness_ids.is_empty(), |element| {
                            element.child(
                                div()
                                    .h(px(28.0))
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .text_xs()
                                    .text_color(rgb(faint()))
                                    .child("No archived threads"),
                            )
                        }),
                )
            })
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
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_bottom_sidebar_menu();
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
