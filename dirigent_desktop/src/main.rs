//! Dirigent desktop: gpui client for dirigent_service hosts. See plan.md §4.

mod app;
mod cache;
mod hosts;
mod icons;
mod input;
mod lists;
mod markdown;
mod menu;
mod net;
mod picker;
mod projects;
mod ruler;
mod select;
mod sidebar;
mod theme;
mod thread;
mod usage;
mod view;

use app::{Abort, Dirigent, NewThread, Quit, ToggleLog, ToggleSidebar};
use cache::Cache;
use gpui::{App, AppContext, Bounds, Focusable, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;
use std::path::PathBuf;

fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .expect("HOME not set")
        .join("dirigent")
}

/// Resolve a font family through fontconfig (gpui wants a concrete installed name).
/// `preferred` is used when installed, else the system default for `generic`.
fn font(env: &str, preferred: &str, generic: &str, fallback: &str) -> String {
    if let Ok(f) = std::env::var(env) {
        return f;
    }
    let fc_match = |pattern: &str| {
        let out = std::process::Command::new("fc-match").args(["-f", "%{family[0]}", pattern]).output().ok()?;
        let s = String::from_utf8(out.stdout).ok()?;
        (!s.trim().is_empty()).then(|| s.trim().to_owned())
    };
    fc_match(preferred).filter(|f| f == preferred).or_else(|| fc_match(generic)).unwrap_or_else(|| fallback.into())
}

fn fonts() -> theme::Fonts {
    theme::Fonts { mono: font("DIRIGENT_FONT", "Lilex", "monospace", "DejaVu Sans Mono").into() }
}

fn main() {
    let cache = Cache::open(&data_dir().join("desktop.db")).expect("open cache");
    let fonts = fonts();

    application().with_assets(icons::Assets).run(move |cx: &mut App| {
        cx.bind_keys(input::key_bindings());
        cx.bind_keys([
            KeyBinding::new("ctrl-n", NewThread, Some(app::KEY_CONTEXT)),
            KeyBinding::new("escape", Abort, Some(app::KEY_CONTEXT)),
            // `[` reaches the handler even from inside an input; see `Dirigent::toggle_sidebar`.
            KeyBinding::new("[", ToggleSidebar, Some(app::KEY_CONTEXT)),
            KeyBinding::new("]", ToggleLog, Some(app::KEY_CONTEXT)),
            KeyBinding::new("ctrl-q", Quit, Some(app::KEY_CONTEXT)),
            // Copies the transcript selection; an input with its own selection handles it first.
            KeyBinding::new("ctrl-c", input::Copy, Some(app::KEY_CONTEXT)),
        ]);

        let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    // app_id/title are what testing.md keys on to find this window.
                    app_id: Some("dirigent".into()),
                    titlebar: Some(TitlebarOptions { title: Some("Dirigent".into()), ..Default::default() }),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Dirigent::new(cache, fonts, window, cx)),
            )
            .unwrap();
        if std::env::var_os("DIRIGENT_DEBUG_KEYS").is_some() {
            cx.observe_keystrokes(|ev, _, _| eprintln!("[keys] {} ctx={:?}", ev.keystroke.unparse(), ev.context_stack)).detach();
        }
        // Focus the root so the global key context (ctrl+n, esc, ...) is active immediately.
        window.update(cx, |view, window, cx| window.focus(&view.focus_handle(cx), cx)).unwrap();
        cx.activate(true);
    });
}
