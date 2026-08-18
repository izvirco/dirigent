//! Renders project and thread navigation in the application sidebar.

mod project;
mod thread;

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Context, CursorStyle, DragMoveEvent, IntoElement, Pixels, Point, Transformation,
    Window, deferred, div, prelude::*, px, radians, relative, svg,
};

use crate::{
    app::{Dirigent, KeyboardMode, SidebarMenu},
    model::{CodexUsage, CodexUsageWindow, HarnessStatus, Id},
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

fn update_status_text(label: String, detail: Option<String>) -> impl IntoElement {
    div()
        .relative()
        .w_full()
        .h_full()
        .px_3()
        .flex()
        .items_center()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .child(label),
        )
        .when_some(detail, |element, detail| {
            element.child(div().ml_2().flex_none().child(detail))
        })
}

fn sidebar_icon(path: &'static str, color: u32) -> impl IntoElement {
    svg()
        .path(path)
        .size(px(16.0))
        .text_color(rgb(color))
        .flex_none()
}

fn ellipsis_vertical_icon(color: u32) -> impl IntoElement {
    sidebar_icon("icon/ellipsis-vertical.svg", color)
}

fn grip_vertical_icon(color: u32) -> impl IntoElement {
    sidebar_icon("icon/grip-vertical.svg", color)
}

fn git_branch_icon(color: u32) -> impl IntoElement {
    sidebar_icon("icon/git-branch.svg", color)
}

fn git_graph_icon(color: u32) -> impl IntoElement {
    sidebar_icon("icon/git-graph.svg", color)
}

fn plus_icon(color: u32) -> impl IntoElement {
    sidebar_icon("icon/plus.svg", color)
}

fn inbox_icon(color: u32) -> impl IntoElement {
    sidebar_icon("icon/inbox.svg", color)
}

fn workpool_icon(color: u32) -> impl IntoElement {
    sidebar_icon("icon/circle.svg", color)
}

fn chevron_icon(expanded: bool) -> impl IntoElement {
    let icon = svg()
        .path("icon/chevron-down.svg")
        .size(px(12.0))
        .text_color(rgb(muted()))
        .flex_none();

    if expanded {
        icon
    } else {
        icon.with_transformation(Transformation::rotate(radians(
            -std::f32::consts::FRAC_PI_2,
        )))
    }
}

fn archive_icon(color: u32) -> impl IntoElement {
    sidebar_icon("icon/archive.svg", color)
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

fn format_codex_usage(usage: CodexUsage) -> Option<String> {
    let mut windows = Vec::new();
    let mut push_window = |label: &str, window: CodexUsageWindow| {
        let available = (100.0 - window.used_percent).clamp(0.0, 100.0);
        windows.push(format!("{label} {available:.0}% left"));
    };
    if let Some(window) = usage.five_hour {
        push_window("5h", window);
    }
    if let Some(window) = usage.weekly {
        push_window("week", window);
    }
    (!windows.is_empty()).then(|| windows.join(" · "))
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
                .w_auto()
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
                        .h(px(26.0))
                        .px_1()
                        .flex()
                        .items_center()
                        .rounded_md()
                        .whitespace_nowrap()
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
                )
                .child(
                    div()
                        .id("about-dirigent")
                        .h(px(26.0))
                        .px_1()
                        .flex()
                        .items_center()
                        .rounded_md()
                        .whitespace_nowrap()
                        .text_xs()
                        .text_color(rgb(muted()))
                        .hover(|style| style.bg(rgb(surface_hover())).text_color(rgb(theme_text())))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.about_open = true;
                            this.check_for_updates();
                            this.sidebar_menu = None;
                            this.enter_normal_mode();
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .child("About"),
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
            .filter(|harness| {
                harness.is_in_inbox()
                    && !self.projects.iter().any(|project| {
                        project.id == harness.project_id && project.keep_active_threads_in_project
                    })
            })
            .map(|harness| harness.id)
            .collect::<Vec<_>>();
        let mut workpool_ids = self
            .harnesses
            .iter()
            .filter(|harness| {
                harness.is_in_workpool()
                    && !self.projects.iter().any(|project| {
                        project.id == harness.project_id && project.keep_active_threads_in_project
                    })
            })
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
        let codex_usage = self.codex_usage.and_then(format_codex_usage);
        let update_action = match &self.update_state {
            crate::update::UpdateState::Available(release) => Some((
                format!("Update to {}", crate::update::display_version(release)),
                None,
                true,
                None,
                false,
            )),
            crate::update::UpdateState::Downloading {
                release,
                downloaded,
                total,
            } => {
                let downloaded_mib = *downloaded as f64 / (1024.0 * 1024.0);
                let total_mib = *total as f64 / (1024.0 * 1024.0);
                let progress = (*total > 0).then_some(*downloaded as f32 / *total as f32);
                Some((
                    format!("Downloading {}", crate::update::display_version(release)),
                    Some(format!("{downloaded_mib:.1}/{total_mib:.1} MiB")),
                    false,
                    progress,
                    false,
                ))
            }
            crate::update::UpdateState::Ready { release, .. } => Some((
                format!(
                    "Restart to finish update to {}",
                    crate::update::display_version(release)
                ),
                None,
                true,
                None,
                true,
            )),
            crate::update::UpdateState::Failed { .. } => {
                Some(("Update failed · Retry".into(), None, true, None, false))
            }
            crate::update::UpdateState::Checking | crate::update::UpdateState::Current => None,
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
            .when_some(
                update_action,
                |element, (label, detail, clickable, progress, ready)| {
                    let progress_label = label.clone();
                    let progress_detail = detail.clone();
                    let downloading = progress.is_some();
                    element.child(
                        div()
                            .id("dirigent-update")
                            .relative()
                            .mx_3()
                            .mt_2()
                            .h(px(30.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .overflow_hidden()
                            .rounded_md()
                            .bg(if ready {
                                rgb(orange()).opacity(0.20)
                            } else if downloading {
                                rgb(blue()).opacity(0.20)
                            } else {
                                rgb(blue()).opacity(0.10)
                            })
                            .text_xs()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(if ready { orange() } else { blue() }))
                            .child(update_status_text(label, detail))
                            .when_some(progress, |element, progress| {
                                let progress = progress.clamp(0.0, 1.0);
                                element.child(
                                    div()
                                        .absolute()
                                        .left_0()
                                        .top_0()
                                        .bottom_0()
                                        .w(relative(progress))
                                        .overflow_hidden()
                                        .rounded_md()
                                        .bg(rgb(orange()).opacity(0.40))
                                        .text_color(rgb(orange()))
                                        .child(
                                            div()
                                                .w(relative(1.0 / progress.max(0.001)))
                                                .h_full()
                                                .child(update_status_text(
                                                    progress_label,
                                                    progress_detail,
                                                )),
                                        ),
                                )
                            })
                            .when(clickable, |element| {
                                element
                                    .cursor(CursorStyle::PointingHand)
                                    .hover(move |style| {
                                        style.bg(if ready {
                                            rgb(orange()).opacity(0.40)
                                        } else {
                                            rgb(blue()).opacity(0.20)
                                        })
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.activate_update(cx);
                                        cx.stop_propagation();
                                    }))
                            }),
                    )
                },
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
                    .child(
                        div()
                            .relative()
                            .top(px(3.0))
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_xs()
                            .text_color(rgb(muted()))
                            .when_some(codex_usage, |element, usage| element.child(usage)),
                    )
                    .child(
                        div()
                            .id("sidebar-bottom-menu-trigger")
                            .relative()
                            .top(px(3.0))
                            .flex_none()
                            .group("sidebar-bottom-menu-trigger")
                            .h(px(26.0))
                            .px_2()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .text_xs()
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
                            this.resize_sidebar(drag.width + delta.as_f32(), cx);
                            cx.notify();
                        },
                    )),
            )
    }
}
