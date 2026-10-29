//! Dirigent desktop: gpui client for dirigent_service hosts.

// A release build on Windows is a GUI app: no console window next to it (it logs to its
// file). Debug builds keep theirs for the stderr log.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod add_project;
mod ambient;
mod app;
mod cache;
mod config;
mod icons;
mod images;
mod input;
mod lists;
mod markdown;
mod menu;
mod modal;
mod net;
mod orchestra;
mod picker;
mod projects;
mod ruler;
mod select;
mod settings;
mod sidebar;
mod theme;
mod thread;
mod titlebar;
mod usage;
mod view;
mod working;

use app::{Dirigent, Dismiss, NewThread, Quit, TextBigger, TextReset, TextSmaller, ToggleLog, ToggleSidebar};
use cache::Cache;
use dirigent_service::hosts;
use gpui::{App, AppContext, Bounds, Focusable, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;

fn main() {
    // `--data-dir` / `--config-dir`: shared with the embedded service, which keeps its db,
    // workspaces and jobs next to `desktop.db`.
    if let Err(e) = dirigent_service::init_dirs() {
        eprintln!("{e:#}");
        std::process::exit(2);
    }
    dirigent_service::init_logging("desktop");
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        data = %dirigent_service::data_dir().display(),
        config = %dirigent_service::config_dir().display(),
        "desktop starting"
    );
    let cache = Cache::open(&dirigent_service::data_dir().join("desktop.db")).expect("open cache");
    // A broken config.toml still starts the app (on defaults); the watcher picks up the fix.
    let config = config::load().unwrap_or_else(|e| {
        tracing::warn!("{e:#}");
        config::Config::default()
    });
    // Before the config watcher takes its first stamp, so the write isn't seen as an edit.
    if let Err(e) = theme::write_example() {
        tracing::warn!("{e:#}");
    }
    // A translucent theme needs the window created see-through; `Dirigent` toasts a bad theme.
    let window_background = theme::load(&config.theme).map(|t| theme::background(&t)).unwrap_or_default();

    application().with_assets(icons::Assets).run(move |cx: &mut App| {
        cx.bind_keys(input::key_bindings());
        cx.bind_keys([
            KeyBinding::new("ctrl-n", NewThread, Some(app::KEY_CONTEXT)),
            KeyBinding::new("escape", Dismiss, Some(app::KEY_CONTEXT)),
            // `[` reaches the handler even from inside an input; see `Dirigent::toggle_sidebar`.
            KeyBinding::new("[", ToggleSidebar, Some(app::KEY_CONTEXT)),
            KeyBinding::new("]", ToggleLog, Some(app::KEY_CONTEXT)),
            KeyBinding::new("ctrl-q", Quit, Some(app::KEY_CONTEXT)),
            // Copies the transcript selection; an input with its own selection handles it first.
            KeyBinding::new("ctrl-c", input::Copy, Some(app::KEY_CONTEXT)),
            // The text size (`[fonts] size`), like zoom in a browser; `+` is shift-= on most layouts.
            KeyBinding::new("ctrl-=", TextBigger, Some(app::KEY_CONTEXT)),
            KeyBinding::new("ctrl-+", TextBigger, Some(app::KEY_CONTEXT)),
            KeyBinding::new("ctrl--", TextSmaller, Some(app::KEY_CONTEXT)),
            KeyBinding::new("ctrl-0", TextReset, Some(app::KEY_CONTEXT)),
        ]);

        let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_background,
                    // app_id/title are what testing.md keys on to find this window.
                    app_id: Some("dirigent".into()),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Dirigent".into()),
                        appears_transparent: titlebar::CUSTOM,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Dirigent::new(cache, config, window, cx)),
            )
            .unwrap();
        // Every keystroke with its key-context stack, at debug (`DIRIGENT_LOG=dirigent_desktop=debug`):
        // the fastest way to debug a binding.
        cx.observe_keystrokes(|ev, _, _| tracing::debug!("key {} ctx={:?}", ev.keystroke.unparse(), ev.context_stack)).detach();
        // Focus the root so the global key context (ctrl+n, esc, ...) is active immediately.
        window.update(cx, |view, window, cx| window.focus(&view.focus_handle(cx), cx)).unwrap();
        cx.activate(true);
    });
}
