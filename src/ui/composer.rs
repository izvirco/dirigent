use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, BoxShadow, Context, Entity, Focusable, IntoElement,
    ObjectFit, StyledImage, Transformation, Window, deferred, div, img, percentage, prelude::*, px,
    radians, rgba, svg,
};

use crate::{
    app::{ComposerDropdown, Dirigent},
    model::{ContextUsage, HarnessStatus, PiProcessState, WorkspaceBackend, WorkspaceState},
    text_input::TextInput,
    theme::{bg, blue, border, muted, orange, red, rgb, surface, surface_hover, theme_text},
};

fn format_context_usage(usage: ContextUsage) -> String {
    let used = usage.used_tokens / 1_000;
    let total = usage.context_window / 1_000;
    format!("{used}k/{total}k")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NixBadgeState {
    Disabled,
    Enabled,
    Initializing,
    Errored,
}

impl NixBadgeState {
    fn for_process(enabled: bool, process_state: PiProcessState) -> Self {
        if !enabled {
            return Self::Disabled;
        }
        match process_state {
            PiProcessState::Initializing => Self::Initializing,
            PiProcessState::Errored => Self::Errored,
            PiProcessState::Stopped | PiProcessState::Ready => Self::Enabled,
        }
    }
}

fn format_queue_state(steering: usize, follow_up: usize) -> Option<String> {
    let mut parts = Vec::new();
    if steering > 0 {
        parts.push(format!("{steering} steering"));
    }
    if follow_up > 0 {
        parts.push(format!(
            "{follow_up} follow-up{}",
            if follow_up == 1 { "" } else { "s" }
        ));
    }
    (!parts.is_empty()).then(|| format!("{} queued", parts.join(" · ")))
}

pub(super) fn dropdown_arrow(open: bool) -> impl IntoElement {
    let icon = svg()
        .path("icon/chevron-down.svg")
        .size(px(12.0))
        .text_color(rgb(muted()))
        .flex_none();

    if open {
        icon.with_transformation(Transformation::rotate(radians(std::f32::consts::PI)))
    } else {
        icon
    }
}

impl Dirigent {
    fn render_new_harness_project_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let open = self.composer_dropdown == Some(ComposerDropdown::Project);
        let selected_project = self
            .selected_project
            .and_then(|id| self.projects.iter().find(|project| project.id == id));
        let selected_name = selected_project
            .map(|project| project.name.clone())
            .unwrap_or_else(|| "Select a project".into());

        div()
            .relative()
            .child(
                div()
                    .id("new-harness-project-picker")
                    .h(px(26.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(border()))
                    .bg(rgb(surface()))
                    .text_xs()
                    .cursor_pointer()
                    .when(open, |style| style.border_color(rgb(blue())))
                    .hover(|style| style.bg(rgb(surface_hover())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if open {
                            this.composer_dropdown = None;
                        } else {
                            this.toggle_composer_dropdown(ComposerDropdown::Project);
                        }
                        cx.notify();
                        cx.stop_propagation();
                    }))
                    .child(div().text_color(rgb(muted())).child("Create thread in"))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .max_w(px(260.0))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(theme_text()))
                            .child(selected_name),
                    )
                    .child(dropdown_arrow(open)),
            )
            .when(open, |element| {
                element.child(
                    deferred(
                        div()
                            .id("new-harness-project-dropdown")
                            .absolute()
                            .bottom(px(36.0))
                            .left_0()
                            .w_auto()
                            .max_h(px(280.0))
                            .p_1()
                            .overflow_y_scroll()
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(border()))
                            .bg(rgb(surface_hover()))
                            .occlude()
                            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                            .children(self.projects.iter().enumerate().map(|(index, project)| {
                                let project_id = project.id;
                                let selected = self.selected_project == Some(project_id);
                                let name = project.name.clone();
                                let path = project.path.display().to_string();
                                div()
                                    .id(("new-harness-project-option", index))
                                    .h(px(26.0))
                                    .px_1()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .rounded_md()
                                    .whitespace_nowrap()
                                    .text_xs()
                                    .cursor_pointer()
                                    .when(selected, |style| {
                                        style.bg(rgb(crate::theme::selection()))
                                    })
                                    .when(!selected, |element| {
                                        element.hover(|style| style.bg(rgb(border())))
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if this.selected_project == Some(project_id) {
                                            this.composer_dropdown = None;
                                        } else {
                                            this.start_new_harness(project_id);
                                        }
                                        cx.notify();
                                        cx.stop_propagation();
                                    }))
                                    .child(div().text_color(rgb(theme_text())).child(name))
                                    .child(div().text_color(rgb(muted())).child(path))
                            })),
                    )
                    .priority(2),
                )
            })
            .into_any_element()
    }

    fn render_model_picker(&self, current: &str, cx: &mut Context<Self>) -> AnyElement {
        let open = self.composer_dropdown == Some(ComposerDropdown::Model);
        let current = current.to_string();
        let model_label = current
            .split_once('/')
            .map_or(current.as_str(), |(_, model)| model)
            .to_string();
        div()
            .relative()
            .child(
                div()
                    .id("model-picker")
                    .h(px(26.0))
                    .max_w(px(260.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .text_xs()
                    .text_color(rgb(muted()))
                    .when(open, |style| {
                        style.bg(rgb(surface_hover())).text_color(rgb(theme_text()))
                    })
                    .hover(|style| style.bg(rgb(surface_hover())).text_color(rgb(theme_text())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if open {
                            this.composer_dropdown = None;
                        } else {
                            this.toggle_composer_dropdown(ComposerDropdown::Model);
                        }
                        cx.notify();
                        cx.stop_propagation();
                    }))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(model_label),
                    )
                    .child(dropdown_arrow(open)),
            )
            .when(open, |element| {
                element.child(
                    deferred(
                        div()
                            .id("model-dropdown")
                            .absolute()
                            .bottom(px(36.0))
                            .left_0()
                            .w_auto()
                            .max_h(px(300.0))
                            .p_1()
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(border()))
                            .bg(rgb(surface_hover()))
                            .group("model-dropdown-scrollbar")
                            .occlude()
                            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                            .child(
                                div()
                                    .id("model-dropdown-content")
                                    .max_h(px(290.0))
                                    .overflow_y_scroll()
                                    .track_scroll(&self.model_picker_scroll)
                                    .when(self.available_models.is_empty(), |element| {
                                        element.child(
                                            div()
                                                .h(px(26.0))
                                                .px_1()
                                                .flex()
                                                .items_center()
                                                .text_xs()
                                                .text_color(rgb(muted()))
                                                .child("Loading models…"),
                                        )
                                    })
                                    .children(
                                        self.available_models.iter().cloned().enumerate().map(
                                            |(index, model)| {
                                                let selected = current
                                                    == format!("{}/{}", model.provider, model.id);
                                                let provider = model.provider.clone();
                                                let model_id = model.id.clone();
                                                div()
                                                    .id(("model-option", index))
                                                    .h(px(26.0))
                                                    .px_1()
                                                    .flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .rounded_md()
                                                    .whitespace_nowrap()
                                                    .text_xs()
                                                    .when(selected, |style| {
                                                        style.bg(rgb(crate::theme::selection()))
                                                    })
                                                    .when(!selected, |element| {
                                                        element
                                                            .hover(|style| style.bg(rgb(border())))
                                                    })
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.select_model(
                                                            provider.clone(),
                                                            model_id.clone(),
                                                        );
                                                        cx.notify();
                                                    }))
                                                    .child(
                                                        div()
                                                            .text_color(rgb(theme_text()))
                                                            .child(model.name),
                                                    )
                                                    .child(div().text_color(rgb(muted())).child(
                                                        format!("{}/{}", model.provider, model.id),
                                                    ))
                                            },
                                        ),
                                    ),
                            )
                            .child(self.render_thin_scrollbar(
                                "model-dropdown-scrollbar",
                                &self.model_picker_scroll,
                            )),
                    )
                    .priority(2),
                )
            })
            .into_any_element()
    }

    fn render_reasoning_picker(&self, current: &str, cx: &mut Context<Self>) -> AnyElement {
        let open = self.composer_dropdown == Some(ComposerDropdown::Reasoning);
        let current = current.to_string();
        div()
            .relative()
            .child(
                div()
                    .id("reasoning-picker")
                    .h(px(26.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .text_xs()
                    .text_color(rgb(muted()))
                    .when(open, |style| {
                        style.bg(rgb(surface_hover())).text_color(rgb(theme_text()))
                    })
                    .hover(|style| style.bg(rgb(surface_hover())).text_color(rgb(theme_text())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if open {
                            this.composer_dropdown = None;
                        } else {
                            this.toggle_composer_dropdown(ComposerDropdown::Reasoning);
                        }
                        cx.notify();
                        cx.stop_propagation();
                    }))
                    .child(current.clone())
                    .child(dropdown_arrow(open)),
            )
            .when(open, |element| {
                element.child(
                    deferred(
                        div()
                            .id("reasoning-dropdown")
                            .absolute()
                            .bottom(px(36.0))
                            .left_0()
                            .w_auto()
                            .p_1()
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(border()))
                            .bg(rgb(surface_hover()))
                            .occlude()
                            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                            .children(self.reasoning_options().into_iter().enumerate().map(
                                |(index, level)| {
                                    let selected = current == level;
                                    let value = level.to_string();
                                    div()
                                        .id(("reasoning-option", index))
                                        .h(px(26.0))
                                        .px_1()
                                        .flex()
                                        .items_center()
                                        .rounded_md()
                                        .whitespace_nowrap()
                                        .text_xs()
                                        .text_color(rgb(theme_text()))
                                        .when(selected, |style| {
                                            style.bg(rgb(crate::theme::selection()))
                                        })
                                        .when(!selected, |element| {
                                            element.hover(|style| style.bg(rgb(border())))
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.select_thinking(value.clone());
                                            cx.notify();
                                        }))
                                        .child(level)
                                },
                            )),
                    )
                    .priority(2),
                )
            })
            .into_any_element()
    }

    fn render_workspace_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let workspace = self.selected_managed_workspace();
        let pending = self.pending_workspace_source();
        let repository = self.repository_snapshot_for_composer();
        let Some((label, backend)) = workspace
            .map(|workspace| (format!("{} workspace", workspace.id), workspace.backend))
            .or_else(|| {
                pending.map(|source| {
                    let label = match source.backend {
                        WorkspaceBackend::Jj => {
                            format!("New workspace alongside {}", source.source_label)
                        }
                        WorkspaceBackend::Git => {
                            format!("New workspace from {}", source.source_label)
                        }
                    };
                    (label, source.backend)
                })
            })
            .or_else(|| repository.map(|source| (source.source_label.clone(), source.backend)))
        else {
            return div().into_any_element();
        };
        let open = self.composer_dropdown == Some(ComposerDropdown::Workspace);
        let managed_path = workspace.map(|workspace| workspace.working_directory.clone());
        let provenance = workspace.map(|workspace| match workspace.backend {
            WorkspaceBackend::Jj => {
                format!("Workspace alongside {}", workspace.source_label)
            }
            WorkspaceBackend::Git => format!("Workspace from {}", workspace.source_label),
        });
        let workspace_status = workspace.and_then(|workspace| match &workspace.state {
            WorkspaceState::Provisioning => Some("Creating workspace…".to_string()),
            WorkspaceState::Failed(error) => Some(error.clone()),
            WorkspaceState::Ready => None,
        });
        let source = pending.or(repository);
        let dirty_warning = source
            .is_some_and(|source| source.backend == WorkspaceBackend::Git && source.dirty)
            .then(|| "Uncommitted changes will not be included".to_string());
        let has_metadata = workspace.is_some() || dirty_warning.is_some();
        let can_create = workspace.is_none()
            && self.selected_harness.is_none_or(|id| {
                self.harnesses
                    .iter()
                    .find(|harness| harness.id == id)
                    .is_none_or(|harness| harness.status != HarnessStatus::Working)
            });

        div()
            .relative()
            .child(
                div()
                    .id("workspace-picker")
                    .h(px(26.0))
                    .max_w(px(300.0))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .text_xs()
                    .text_color(rgb(muted()))
                    .when(open, |style| {
                        style.bg(rgb(surface_hover())).text_color(rgb(theme_text()))
                    })
                    .hover(|style| style.bg(rgb(surface_hover())).text_color(rgb(theme_text())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if open {
                            this.composer_dropdown = None;
                        } else {
                            this.toggle_workspace_dropdown();
                        }
                        cx.notify();
                        cx.stop_propagation();
                    }))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(label),
                    )
                    .child(dropdown_arrow(open)),
            )
            .when(open, |element| {
                element.child(
                    deferred(
                        div()
                            .id("workspace-dropdown")
                            .absolute()
                            .bottom(px(36.0))
                            .left_0()
                            .w_auto()
                            .p_1()
                            .flex()
                            .flex_col()
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(border()))
                            .bg(rgb(surface_hover()))
                            .occlude()
                            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                            .when(has_metadata, |menu| {
                                menu.child(
                                    div()
                                        .px_1()
                                        .py_1()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .whitespace_nowrap()
                                        .text_xs()
                                        .text_color(rgb(muted()))
                                        .when_some(managed_path.clone(), |metadata, path| {
                                            metadata.child(path.display().to_string())
                                        })
                                        .when_some(provenance, |metadata, provenance| {
                                            metadata.child(provenance)
                                        })
                                        .when_some(workspace_status, |metadata, status| {
                                            metadata.child(status)
                                        })
                                        .when_some(dirty_warning, |metadata, warning| {
                                            metadata.child(
                                                div().text_color(rgb(orange())).child(warning),
                                            )
                                        }),
                                )
                                .child(div().h(px(1.0)).mx_1().my_1().bg(rgb(border())))
                            })
                            .when_some(managed_path, |menu, path| {
                                let copy_path = path.display().to_string();
                                menu.child(
                                    div()
                                        .id("copy-workspace-path")
                                        .h(px(26.0))
                                        .px_1()
                                        .flex()
                                        .items_center()
                                        .rounded_md()
                                        .whitespace_nowrap()
                                        .text_xs()
                                        .text_color(rgb(theme_text()))
                                        .hover(|style| style.bg(rgb(border())))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.copy_text(copy_path.clone(), cx);
                                            this.composer_dropdown = None;
                                            cx.notify();
                                        }))
                                        .child("Copy path"),
                                )
                            })
                            .when(workspace.is_none() && pending.is_some(), |menu| {
                                menu.child(
                                    div()
                                        .id("use-project-directory")
                                        .h(px(26.0))
                                        .px_1()
                                        .flex()
                                        .items_center()
                                        .rounded_md()
                                        .whitespace_nowrap()
                                        .text_xs()
                                        .text_color(rgb(theme_text()))
                                        .hover(|style| style.bg(rgb(border())))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.use_project_directory();
                                            cx.notify();
                                        }))
                                        .child("Use project directory"),
                                )
                            })
                            .when(
                                workspace.is_none() && pending.is_none() && can_create,
                                |menu| {
                                    let source_label = source
                                        .map(|source| source.source_label.clone())
                                        .unwrap_or_default();
                                    let action = match backend {
                                        WorkspaceBackend::Jj => {
                                            format!("New workspace alongside {source_label}")
                                        }
                                        WorkspaceBackend::Git => {
                                            format!("New workspace from {source_label}")
                                        }
                                    };
                                    menu.child(
                                        div()
                                            .id("create-workspace-on-send")
                                            .h(px(26.0))
                                            .px_1()
                                            .flex()
                                            .items_center()
                                            .rounded_md()
                                            .whitespace_nowrap()
                                            .text_xs()
                                            .text_color(rgb(theme_text()))
                                            .hover(|style| style.bg(rgb(border())))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.choose_new_workspace();
                                                cx.notify();
                                            }))
                                            .child(action),
                                    )
                                },
                            ),
                    )
                    .priority(2),
                )
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_prompt(
        &self,
        input: Entity<TextInput>,
        model: &str,
        thinking: &str,
        context_usage: Option<ContextUsage>,
        queue_state: Option<String>,
        working: bool,
        creating: bool,
        nix_available: bool,
        nix_state: NixBadgeState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focused = input.focus_handle(cx).is_focused(window);
        let images = input.read(cx).images();
        div()
            .relative()
            .w_full()
            .max_w(px(820.0))
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_xl()
            .border_1()
            .border_color(rgba(0x00000000))
            .bg(rgb(surface()))
            .when(focused, |element| {
                element.border_color(rgb(blue()).opacity(0.5)).shadow(vec![
                    BoxShadow::new(px(0.0), px(0.0), rgb(blue()).opacity(0.19).into())
                        .blur_radius(px(5.0)),
                ])
            })
            .when(!images.is_empty(), |element| {
                element.child(
                    div()
                        .w_full()
                        .px_2()
                        .pt_1()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .children(images.into_iter().enumerate().map(|(index, attachment)| {
                            let preview = attachment.image.clone();
                            let label = attachment.label.clone();
                            let remove_label = attachment.label.clone();
                            let remove_input = input.clone();
                            div()
                                .id(("composer-image", index))
                                .relative()
                                .w(px(96.0))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_image_preview(preview.clone());
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .w(px(96.0))
                                        .h(px(68.0))
                                        .overflow_hidden()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(rgb(border()))
                                        .child(
                                            img(attachment.image)
                                                .size_full()
                                                .object_fit(ObjectFit::Cover),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(blue()))
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(format!("[{label}]")),
                                )
                                .child(
                                    div()
                                        .id(("remove-composer-image", index))
                                        .absolute()
                                        .top(px(-5.0))
                                        .right(px(-5.0))
                                        .size(px(18.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_full()
                                        .bg(rgb(border()))
                                        .text_xs()
                                        .text_color(rgb(theme_text()))
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            remove_input.update(cx, |input, cx| {
                                                input.remove_image(&remove_label, cx)
                                            });
                                            cx.stop_propagation();
                                        }))
                                        .child("×"),
                                )
                        })),
                )
            })
            .child(input)
            .child(
                div()
                    .w_full()
                    .pl_1()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(self.render_workspace_picker(cx))
                    .child(self.render_model_picker(model, cx))
                    .child(self.render_reasoning_picker(thinking, cx))
                    .when_some(context_usage, |element, usage| {
                        element.child(
                            div()
                                .h(px(26.0))
                                .px_2()
                                .flex()
                                .items_center()
                                .text_xs()
                                .text_color(rgb(muted()))
                                .child(format_context_usage(usage)),
                        )
                    })
                    .when_some(queue_state, |element, queue_state| {
                        element.child(
                            div()
                                .h(px(26.0))
                                .px_2()
                                .flex()
                                .items_center()
                                .rounded_md()
                                .bg(rgb(orange()).opacity(0.10))
                                .text_xs()
                                .text_color(rgb(orange()))
                                .child(queue_state),
                        )
                    })
                    .when(nix_available, |element| {
                        let initializing = nix_state == NixBadgeState::Initializing;
                        let color = match nix_state {
                            NixBadgeState::Disabled => muted(),
                            NixBadgeState::Enabled | NixBadgeState::Initializing => blue(),
                            NixBadgeState::Errored => red(),
                        };
                        element.child(
                            div()
                                .id("nix-toggle")
                                .h(px(26.0))
                                .px_2()
                                .flex()
                                .items_center()
                                .gap_1()
                                .rounded_md()
                                .text_xs()
                                .text_color(rgb(color))
                                .when(
                                    matches!(
                                        nix_state,
                                        NixBadgeState::Enabled | NixBadgeState::Initializing
                                    ),
                                    |style| style.bg(rgb(crate::theme::accent_surface())),
                                )
                                .hover(|style| style.bg(rgb(surface_hover())))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.toggle_nix();
                                    cx.notify();
                                }))
                                .child("nix")
                                .when(initializing, |badge| {
                                    badge.child(
                                        svg()
                                            .path("icon/loader-circle.svg")
                                            .size(px(11.0))
                                            .flex_none()
                                            .text_color(rgb(blue()))
                                            .with_animation(
                                                "nix-spinner",
                                                Animation::new(Duration::from_millis(900)).repeat(),
                                                |icon, delta| {
                                                    icon.with_transformation(
                                                        Transformation::rotate(percentage(delta)),
                                                    )
                                                },
                                            ),
                                    )
                                }),
                        )
                    })
                    .child(div().flex_1())
                    .when(working, |element| {
                        element.child(
                            div()
                                .id("abort-agent")
                                .h(px(30.0))
                                .px_3()
                                .flex()
                                .items_center()
                                .rounded_lg()
                                .text_xs()
                                .text_color(rgb(crate::theme::error_text()))
                                .hover(|style| style.bg(rgb(crate::theme::error_bg())))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.abort_selected();
                                    cx.notify();
                                }))
                                .child("Stop"),
                        )
                    })
                    .child(
                        div()
                            .id(if creating {
                                "start-harness"
                            } else {
                                "send-prompt"
                            })
                            .size(px(30.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgb(blue()))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(rgb(bg()))
                            .hover(|style| style.bg(rgb(crate::theme::accent_hover())))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if creating {
                                    this.create_harness(cx);
                                } else {
                                    this.send_composer(cx);
                                }
                            }))
                            .child("↑"),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn render_composer(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let harness = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id));
        let working = harness.is_some_and(|harness| harness.status == HarnessStatus::Working);
        let composer_input = self
            .selected_composer_input()
            .expect("selected harness must have a composer");
        let model = harness
            .and_then(|harness| harness.model.clone())
            .unwrap_or_else(|| "model loading".into());
        let thinking = harness
            .and_then(|harness| harness.thinking_level.clone())
            .unwrap_or_else(|| "loading".into());
        let context_usage = harness.and_then(|harness| harness.context_usage);
        let queue_state = harness.and_then(|harness| {
            format_queue_state(harness.steering_queue.len(), harness.follow_up_queue.len())
        });
        let nix_available = harness.is_some_and(|harness| self.harness_has_devshell(harness.id));
        let nix_state = harness.map_or(NixBadgeState::Disabled, |harness| {
            NixBadgeState::for_process(harness.nix_enabled, harness.process_state)
        });

        div()
            .w_full()
            .px_6()
            .pb_5()
            .pt_2()
            .flex_none()
            .flex()
            .justify_center()
            .child(self.render_prompt(
                composer_input,
                &model,
                &thinking,
                context_usage,
                queue_state,
                working,
                false,
                nix_available,
                nix_state,
                window,
                cx,
            ))
    }

    pub(super) fn render_new_harness(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let model = self
            .draft_model
            .clone()
            .unwrap_or_else(|| "model loading".into());
        let thinking = self
            .draft_thinking_level
            .clone()
            .unwrap_or_else(|| "loading".into());
        let nix_available = self
            .selected_project
            .is_some_and(|project_id| self.project_has_devshell(project_id));

        div()
            .w_full()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .px_6()
            .child(
                div()
                    .w_full()
                    .max_w(px(820.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .child(self.render_new_harness_project_picker(cx)),
                    )
                    .child(self.render_prompt(
                        self.harness_input.clone(),
                        &model,
                        &thinking,
                        None,
                        None,
                        false,
                        true,
                        nix_available,
                        NixBadgeState::for_process(self.draft_nix_enabled, PiProcessState::Stopped),
                        window,
                        cx,
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{format_context_usage, format_queue_state};
    use crate::model::ContextUsage;

    #[test]
    fn formats_queue_counts() {
        assert_eq!(
            format_queue_state(1, 0).as_deref(),
            Some("1 steering queued")
        );
        assert_eq!(
            format_queue_state(2, 1).as_deref(),
            Some("2 steering · 1 follow-up queued")
        );
        assert_eq!(format_queue_state(0, 0), None);
    }

    #[test]
    fn formats_used_and_total_context_in_thousands() {
        assert_eq!(
            format_context_usage(ContextUsage {
                used_tokens: 60_000,
                context_window: 200_000,
            }),
            "60k/200k"
        );
    }
}
