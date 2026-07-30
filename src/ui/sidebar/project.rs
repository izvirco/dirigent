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
                        .id(("project-settings", id as usize))
                        .h(px(30.0))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded_sm()
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
    pub(super) fn render_sidebar_project(&self, id: Id, cx: &mut Context<Self>) -> AnyElement {
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
        let selected = self.selected_project == Some(id)
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
}
