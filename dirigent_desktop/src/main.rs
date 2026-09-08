//! Starts the Dirigent desktop application and opens its primary window.

#![cfg_attr(target_os = "windows", feature(windows_process_extensions_show_window))]
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod app;
mod assets;
mod build_info;
mod cache;
mod delegation;
mod diff;
mod image_attachment;
mod logging;
mod markdown;
mod math;
mod model;
mod platform;
mod rpc;
mod selectable_text;
mod storage;
mod text_input;
mod theme;
mod title_generator;
mod ui;
#[cfg(feature = "self-update")]
mod update;
mod vcs;

use app::Dirigent;
use assets::Assets;
use gpui::{
    App, Bounds, CursorHideMode, SharedString, TitlebarOptions, WindowBounds, WindowOptions,
    prelude::*, px, size,
};
use gpui_platform::application;

#[cfg(feature = "bundled-lilex")]
fn load_bundled_fonts(cx: &App) {
    use std::borrow::Cow;

    let fonts = vec![
        Cow::Borrowed(&include_bytes!(concat!(env!("OUT_DIR"), "/Lilex-Regular.ttf"))[..]),
        Cow::Borrowed(&include_bytes!(concat!(env!("OUT_DIR"), "/Lilex-Italic.ttf"))[..]),
        Cow::Borrowed(&include_bytes!(concat!(env!("OUT_DIR"), "/Lilex-Medium.ttf"))[..]),
        Cow::Borrowed(&include_bytes!(concat!(env!("OUT_DIR"), "/Lilex-MediumItalic.ttf"))[..]),
        Cow::Borrowed(&include_bytes!(concat!(env!("OUT_DIR"), "/Lilex-SemiBold.ttf"))[..]),
        Cow::Borrowed(&include_bytes!(concat!(env!("OUT_DIR"), "/Lilex-SemiBoldItalic.ttf"))[..]),
        Cow::Borrowed(&include_bytes!(concat!(env!("OUT_DIR"), "/Lilex-Bold.ttf"))[..]),
        Cow::Borrowed(&include_bytes!(concat!(env!("OUT_DIR"), "/Lilex-BoldItalic.ttf"))[..]),
    ];

    cx.text_system()
        .add_fonts(fonts)
        .expect("failed to load bundled Lilex fonts");
}

fn main() -> std::process::ExitCode {
    #[cfg(windows)]
    let _installer_marker = dirigent_launcher::installer_marker(build_info::channel())
        .expect("could not register running Dirigent with the installer");
    let _logging_guard = match logging::initialize() {
        Ok(guard) => Some(guard),
        Err(error) => {
            logging::initialize_console();
            tracing::error!(error = %error, "file logging is unavailable");
            None
        }
    };

    tracing::info!(
        version = build_info::version(),
        channel = build_info::channel(),
        target = build_info::target(),
        commit = env!("DIRIGENT_COMMIT_ID"),
        "starting Dirigent"
    );
    #[cfg(feature = "self-update")]
    update::cleanup_old_versions();

    application().with_assets(Assets).run(|cx: &mut App| {
        let app_id = format!("dirigent-{}", build_info::channel());
        let app_name = if build_info::channel() == "stable" {
            "Dirigent".to_string()
        } else {
            format!("Dirigent ({})", build_info::channel())
        };
        cx.set_app_identity(&app_id, &app_name);
        cx.set_cursor_hide_mode(CursorHideMode::Never);

        #[cfg(feature = "bundled-lilex")]
        load_bundled_fonts(cx);

        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from(app_name)),
                    ..Default::default()
                }),
                app_id: Some(app_id.into()),
                ..Default::default()
            },
            |_, cx| cx.new(Dirigent::new),
        )
        .expect("failed to open window");

        cx.activate(true);
    });
    tracing::info!("application exited normally");
    #[cfg(feature = "self-update")]
    if let Err(error) = update::restart_after_shutdown() {
        tracing::error!(%error);
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
