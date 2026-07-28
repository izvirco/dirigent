#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod app;
mod cache;
mod markdown;
mod model;
mod platform;
mod rpc;
mod selectable_text;
mod storage;
mod text_input;
mod theme;
mod ui;

use app::Dirigent;
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
    application().run(|cx: &mut App| {
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
