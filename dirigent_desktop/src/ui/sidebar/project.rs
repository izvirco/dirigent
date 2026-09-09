//! Renders project sections and project drag-and-drop behavior.

use super::*;

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

impl Dirigent {
    pub(super) fn render_project_dropdown(&self, id: Id, cx: &mut Context<Self>) -> AnyElement {
        deferred(
            div()
                .id(("project-dropdown", id as usize))
                .absolute()
                .top(px(30.0))
                .right(px(8.0))
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
                        .id(("project-settings", id as usize))
                        .h(px(26.0))
                        .px_1()
                        .flex()
                        .items_center()
                        .rounded_md()
                        .whitespace_nowrap()
                        .text_xs()
                        .text_color(rgb(theme_text()))
                        .hover(|style| style.bg(rgb(surface_hover())))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_project_settings(id, cx);
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .child("Settings"),
                )
                .child(
                    div()
                        .id(("delete-project", id as usize))
                        .h(px(26.0))
                        .px_1()
                        .flex()
                        .items_center()
                        .rounded_md()
                        .whitespace_nowrap()
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
    pub(super) fn render_sidebar_project(&self, id: Id, cx: &mut Context<Self>) -> AnyElement {
        let project = self
            .projects
            .iter()
            .find(|project| project.id == id)
            .unwrap();
        let collapsed = self.collapsed_projects.contains(&id);
        let keep_active_threads_in_project = project.keep_active_threads_in_project;
        let mut inbox_ids = self
            .harnesses
            .iter()
            .filter(|harness| {
                keep_active_threads_in_project && harness.project_id == id && harness.is_in_inbox()
            })
            .map(|harness| harness.id)
            .collect::<Vec<_>>();
        let mut workpool_ids = self
            .harnesses
            .iter()
            .filter(|harness| {
                keep_active_threads_in_project
                    && harness.project_id == id
                    && harness.is_in_workpool()
            })
            .map(|harness| harness.id)
            .collect::<Vec<_>>();
        let mut archived_ids = self
            .harnesses
            .iter()
            .filter(|harness| {
                harness.project_id == id && harness.archived && harness.delegation.parent.is_none()
            })
            .map(|harness| harness.id)
            .collect::<Vec<_>>();
        let order = |harness_id: &Id| {
            std::cmp::Reverse(
                self.harnesses
                    .iter()
                    .find(|harness| harness.id == *harness_id)
                    .unwrap()
                    .sidebar_order,
            )
        };
        inbox_ids.sort_by_key(order);
        workpool_ids.sort_by_key(order);
        archived_ids.sort_by_key(order);
        const ARCHIVED_THREAD_LIMIT: usize = 8;
        let hidden_thread_count = archived_ids.len().saturating_sub(ARCHIVED_THREAD_LIMIT);
        let archived_threads_expanded = self.expanded_archived_projects.contains(&id);
        let selected = self.settings.is_none() && !self.about_open
            && self.selected_project == Some(id)
            && (!self.adding_project || self.project_settings == Some(id));
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
                            .gap_1()
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
                            )
                            .when(
                                collapsed
                                    && !menu_open
                                    && keep_active_threads_in_project
                                    && !inbox_ids.is_empty(),
                                |element| {
                                    element.child(
                                        div()
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .text_xs()
                                            .text_color(rgb(yellow()))
                                            .group_hover(group.clone(), |style| style.invisible())
                                            .child(inbox_icon(yellow()))
                                            .child(inbox_ids.len().to_string()),
                                    )
                                },
                            )
                            .when(
                                collapsed
                                    && !menu_open
                                    && keep_active_threads_in_project
                                    && !workpool_ids.is_empty(),
                                |element| {
                                    element.child(
                                        div()
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .text_xs()
                                            .text_color(rgb(blue()))
                                            .group_hover(group.clone(), |style| style.invisible())
                                            .child(workpool_icon(blue()))
                                            .child(workpool_ids.len().to_string()),
                                    )
                                },
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
                                    .h(px(26.0))
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .text_xs()
                                    .hover(|style| style.shadow_sm().text_color(rgb(theme_text())))
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if menu_open {
                                            this.sidebar_menu = None;
                                        } else {
                                            this.toggle_project_menu(id);
                                        }
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
                        .when(!inbox_ids.is_empty(), |element| {
                            element
                                .child(self.render_section_header(
                                    "Inbox",
                                    inbox_ids.len(),
                                    yellow(),
                                ))
                                .children(inbox_ids.iter().copied().map(|harness_id| {
                                    self.render_sidebar_harness(
                                        harness_id,
                                        ThreadPlacement::Inbox,
                                        cx,
                                    )
                                }))
                        })
                        .when(!workpool_ids.is_empty(), |element| {
                            element
                                .child(self.render_section_header(
                                    "Workpool",
                                    workpool_ids.len(),
                                    blue(),
                                ))
                                .children(workpool_ids.iter().copied().map(|harness_id| {
                                    self.render_sidebar_harness(
                                        harness_id,
                                        ThreadPlacement::Workpool,
                                        cx,
                                    )
                                }))
                        })
                        .when(
                            keep_active_threads_in_project && !archived_ids.is_empty(),
                            |element| {
                                element.child(self.render_section_header(
                                    "Archived",
                                    archived_ids.len(),
                                    faint(),
                                ))
                            },
                        )
                        .children(
                            archived_ids
                                .iter()
                                .take(ARCHIVED_THREAD_LIMIT)
                                .copied()
                                .map(|harness_id| {
                                    self.render_sidebar_harness(
                                        harness_id,
                                        ThreadPlacement::Project,
                                        cx,
                                    )
                                }),
                        )
                        .when(hidden_thread_count > 0, |element| {
                            element.child(
                                div()
                                    .id(("toggle-archived-threads", id as usize))
                                    .h(px(34.0))
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .rounded_md()
                                    .text_sm()
                                    .text_color(rgb(blue()))
                                    .hover(|style| style.bg(rgb(surface_hover())))
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if !this.expanded_archived_projects.remove(&id) {
                                            this.expanded_archived_projects.insert(id);
                                        }
                                        cx.stop_propagation();
                                        cx.notify();
                                    }))
                                    .child(if archived_threads_expanded {
                                        format!("Hide {hidden_thread_count} threads")
                                    } else {
                                        format!("Show {hidden_thread_count} more")
                                    }),
                            )
                        })
                        .when(archived_threads_expanded, |element| {
                            element.children(
                                archived_ids
                                    .iter()
                                    .skip(ARCHIVED_THREAD_LIMIT)
                                    .copied()
                                    .map(|harness_id| {
                                        self.render_sidebar_harness(
                                            harness_id,
                                            ThreadPlacement::Project,
                                            cx,
                                        )
                                    }),
                            )
                        })
                        .when(
                            archived_ids.is_empty()
                                && inbox_ids.is_empty()
                                && workpool_ids.is_empty(),
                            |element| {
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
                            },
                        ),
                )
            })
            .into_any_element()
    }
}
