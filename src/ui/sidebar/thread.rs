use super::*;

impl Dirigent {
    pub(super) fn render_thread_dropdown(
        &self,
        id: Id,
        archived: bool,
        top: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let archive_label = if archived { "Restore" } else { "Archive" };
        let archive_enabled = archived || self.can_archive_harness(id);
        let has_workspace = self.workspace_for_harness(id).is_some();
        let can_delete_workspace = self.can_delete_workspace_for_harness(id);
        let deleting_workspace = self.workspace_deletion_pending(id);
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
                .w(px(if has_workspace { 220.0 } else { 132.0 }))
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
                        .text_color(rgb(if deleting_workspace { faint() } else { red() }))
                        .when(!deleting_workspace, |element| {
                            element
                                .hover(|style| style.bg(rgb(surface_hover())))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_harness(id);
                                    cx.stop_propagation();
                                    cx.notify();
                                }))
                        })
                        .child("Delete"),
                )
                .when(has_workspace, |menu| {
                    menu.child(
                        div()
                            .id(("delete-thread-and-workspace", id as usize))
                            .h(px(30.0))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded_sm()
                            .text_xs()
                            .text_color(rgb(if can_delete_workspace { red() } else { faint() }))
                            .when(can_delete_workspace, |element| {
                                element
                                    .hover(|style| style.bg(rgb(surface_hover())))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.begin_delete_thread_and_workspace(id);
                                        cx.stop_propagation();
                                        cx.notify();
                                    }))
                            })
                            .child(if deleting_workspace {
                                "Deleting workspace…"
                            } else {
                                "Delete thread and workspace"
                            }),
                    )
                }),
        )
        .priority(2)
        .into_any_element()
    }
    pub(super) fn render_sidebar_harness(
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
        let active = self.selected_harness == Some(id)
            && !self.adding_project
            && self.project_settings.is_none();
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
        let menu_open = self.sidebar_menu == Some(SidebarMenu::Thread(id));
        let renaming = self.renaming_harness == Some(id);
        let group = format!("sidebar-harness-{id}");
        let archive_group = format!("quick-archive-thread-{id}");
        let menu_group = format!("thread-menu-trigger-{id}");
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
            .when(menu_open, |style| style.bg(rgb(surface_hover())))
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
                            } else if has_unread {
                                blue()
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
            .child(
                div()
                    .id(("thread-hover-controls", id as usize))
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right(px(2.0))
                    .flex()
                    .items_center()
                    .rounded_r_md()
                    .bg(rgb(surface_hover()))
                    .when(!menu_open, |controls| {
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
                    .child(
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
                                if menu_open {
                                    this.sidebar_menu = None;
                                } else {
                                    this.toggle_thread_menu(id);
                                }
                                cx.stop_propagation();
                                cx.notify();
                            }))
                            .child(hover_icon(
                                ellipsis_vertical_icon(if menu_open { blue() } else { muted() }),
                                ellipsis_vertical_icon(blue()),
                                menu_group,
                            )),
                    ),
            )
            .when(menu_open, |element| {
                element.child(self.render_thread_dropdown(id, archived, height - 2.0, cx))
            })
            .into_any_element()
    }
}
