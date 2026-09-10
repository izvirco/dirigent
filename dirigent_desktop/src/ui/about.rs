//! Renders build information and an inventory of local storage locations.

use std::path::PathBuf;

use gpui::{Context, IntoElement, SharedString, div, prelude::*, px};

use crate::{
    app::Dirigent,
    build_info, platform,
    theme::{border, muted, rgb, surface_hover, theme_text},
};

fn info_row(label: &'static str, value: impl Into<SharedString>) -> impl IntoElement {
    div()
        .h(px(30.0))
        .flex()
        .items_center()
        .gap_4()
        .border_b_1()
        .border_color(rgb(border()))
        .text_sm()
        .child(div().w(px(100.0)).text_color(rgb(muted())).child(label))
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .text_color(rgb(theme_text()))
                .child(value.into()),
        )
}

struct StorageLocation {
    label: String,
    description: &'static str,
    path: Result<PathBuf, String>,
}

impl Dirigent {
    fn storage_locations(&self) -> Vec<StorageLocation> {
        let mut locations = Vec::new();
        let mut add = |label: String, description, path| {
            locations.push(StorageLocation {
                label,
                description,
                path,
            })
        };
        add(
            "Configuration".into(),
            "config.toml (font, theme, telemetry) and theme/*.toml palettes.",
            platform::config_dir(),
        );
        add(
            "Application state".into(),
            "Projects, threads, drafts, workspace and delegation metadata. SQLite may also create -wal and -shm files alongside this database.",
            platform::state_database_path(),
        );
        add(
            "Conversation cache".into(),
            "Disposable cached conversations, images and model metadata; includes SQLite sidecar files. Update dry runs stage downloads in the adjacent updates directory.",
            platform::cache_path(),
        );
        add(
            "Logs".into(),
            "Application diagnostics, performance logs and crash reports.",
            platform::logs_directory(),
        );
        add(
            "Pi bridge bundles".into(),
            "Versioned private TypeScript/JavaScript extensions used by Pi and delegated agents.",
            platform::bridges_directory(),
        );
        add(
            "Default workspaces".into(),
            "Managed Git worktrees and JJ workspaces. Projects can override this location in their settings.",
            platform::workspace_root(),
        );
        let executable = std::env::current_exe().map_err(|e| e.to_string());
        if let Ok(root) = executable.as_ref().map_err(Clone::clone).and_then(|exe| {
            dirigent_launcher::installation_root(exe, build_info::channel(), build_info::version())
        }) {
            add(
                "Installation".into(),
                "Launcher, current.json, install.lock and versions/ containing application releases and temporary .staging-* update downloads.",
                Ok(root),
            );
        } else {
            add(
                "Application executable".into(),
                "This build is not running from a managed launcher installation.",
                executable,
            );
        }
        let agent_dir = std::env::var("PI_CODING_AGENT_DIR")
            .ok()
            .filter(|v| !v.is_empty())
            .map(|value| {
                if let Some(suffix) = value
                    .strip_prefix("~/")
                    .or_else(|| value.strip_prefix("~\\"))
                {
                    platform::home_dir()
                        .map(|home| home.join(suffix))
                        .ok_or_else(|| "Home directory unavailable".to_string())
                } else if value == "~" {
                    platform::home_dir().ok_or_else(|| "Home directory unavailable".to_string())
                } else {
                    Ok(PathBuf::from(value))
                }
            })
            .unwrap_or_else(|| {
                platform::home_dir()
                    .map(|home| home.join(".pi/agent"))
                    .ok_or_else(|| "Home directory unavailable".to_string())
            });
        add(
            "Pi user data (shared with Pi)".into(),
            "Pi manages settings, credentials, extensions and sessions here by default. PI_CODING_AGENT_DIR overrides this path; project dev shells may use different Pi settings.",
            agent_dir,
        );
        locations
    }

    fn render_storage_locations(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_col().gap_4()
            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Files on your system"))
            .child(div().text_xs().text_color(rgb(muted())).child("Resolved locations for this platform and build channel, including environment overrides. Some locations are created only when used. External tools and agents can store additional files outside these locations."))
            .children(self.storage_locations().into_iter().enumerate().map(|(index, location)| {
                let path = location.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|error| format!("Unavailable: {error}"));
                let copy = path.clone();
                div().flex().flex_col().gap_2().pb_4().border_b_1().border_color(rgb(border()))
                    .child(div().flex().items_center().gap_3()
                        .child(div().flex_1().text_sm().child(location.label))
                        .when(location.path.is_ok(), |row| row.child(div().id(("copy-storage-path", index)).px_2().py_1().rounded_md().text_xs().cursor_pointer()
                            .text_color(rgb(muted())).hover(|s| s.bg(rgb(surface_hover())))
                            .on_click(cx.listener(move |this, _, _, cx| this.copy_text(copy.clone(), cx)))
                            .child("Copy"))))
                    .child(div().id(("storage-path", index)).w_full().overflow_x_scroll().text_xs().child(div().whitespace_nowrap().child(path)))
                    .child(div().text_xs().text_color(rgb(muted())).child(location.description))
            }))
    }

    pub(super) fn render_about(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("about-scroll")
            .flex_1()
            .overflow_y_scroll()
            .p_8()
            .child(
                div()
                    .w_full()
                    .max_w(px(680.0))
                    .mx_auto()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .text_xl()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme_text()))
                                    .child("About Dirigent"),
                            )
                            .child(
                                div()
                                    .id("close-about")
                                    .h(px(32.0))
                                    .px_2()
                                    .py_1()
                                    .flex()
                                    .items_center()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(rgb(muted()))
                                    .hover(|style| {
                                        style.bg(rgb(surface_hover())).text_color(rgb(theme_text()))
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.about_open = false;
                                        cx.notify();
                                    }))
                                    .child("Close"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .pb_2()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme_text()))
                                    .child("Build information"),
                            )
                            .child(info_row("Version", build_info::version()))
                            .child(info_row("Pi version", self.pi_version.clone()))
                            .child(info_row("Commit", env!("DIRIGENT_COMMIT_ID")))
                            .child(info_row("Target", build_info::target()))
                            .child(info_row("Channel", build_info::channel())),
                    )
                    .child(self.render_storage_locations(cx)),
            )
    }
}
