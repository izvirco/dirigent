#![cfg_attr(target_os = "windows", feature(windows_process_extensions_show_window))]
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod app;
mod assets;
mod cache;
mod diff;
mod image_attachment;
mod logging;
mod markdown;
mod model;
mod platform;
mod rpc;
mod selectable_text;
mod storage;
mod text_input;
mod theme;
mod title_generator;
mod ui;
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

fn main() {
    let _logging_guard = match logging::initialize() {
        Ok(guard) => Some(guard),
        Err(error) => {
            logging::initialize_console();
            tracing::error!(error = %error, "file logging is unavailable");
            None
        }
    };
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        commit = env!("DIRIGENT_COMMIT_ID"),
        "starting Dirigent"
    );

    application().with_assets(Assets).run(|cx: &mut App| {
        cx.set_app_identity("dirigent", "Dirigent");
        cx.set_cursor_hide_mode(CursorHideMode::Never);

        #[cfg(feature = "bundled-lilex")]
        load_bundled_fonts(cx);

        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("Dirigent — pi workspace")),
                    ..Default::default()
                }),
                app_id: Some("dirigent".into()),
                ..Default::default()
            },
            |_, cx| cx.new(Dirigent::new),
        )
        .expect("failed to open window");

        cx.activate(true);
    });
}
