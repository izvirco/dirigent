//! Dirigent desktop: gpui client for dirigent_service hosts.

// A release build on Windows is a GUI app: no console window next to it (it logs to its
// file). Debug builds keep theirs for the stderr log.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod account;
mod add_project;
mod ambient;
mod app;
mod cache;
mod changelog;
mod config;
mod hosts_tab;
mod icons;
mod images;
mod input;
mod instance;
mod leaderboard;
mod lists;
mod login;
mod markdown;
mod mcp;
mod menu;
mod migration;
mod modal;
mod net;
mod onboarding;
mod orchestra;
mod picker;
mod placement;
mod projects;
mod proxy;
mod ruler;
mod screen;
mod select;
mod settings;
mod sidebar;
mod theme;
mod thread;
mod titlebar;
mod update;
mod usage;
mod view;
mod working;

use app::{Dirigent, Dismiss, NewThread, Quit, TextBigger, TextReset, TextSmaller, ToggleLog, ToggleSidebar};
use cache::Cache;
use dirigent_protocol::release;
use dirigent_service::hosts;
use gpui::{App, AppContext, Focusable, KeyBinding, Keystroke, QuitMode, TitlebarOptions, WindowOptions};
use gpui_platform::application;

fn main() {
    // `--data-dir` / `--config-dir`: shared with the embedded service, which keeps its db,
    // workspaces and jobs next to `desktop.db`. An operand is a folder to open (Explorer's
    // "Open in Dirigent", `dirigent_desktop <dir>`).
    let operands = match dirigent_service::init_dirs() {
        Ok(operands) => operands,
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(2);
        }
    };
    dirigent_service::init_logging("desktop");
    // Absolute: the desktop that opens it may be another process, in another directory.
    let open = operands.first().map(|p| std::path::absolute(p).map_or_else(|_| p.clone(), |a| a.display().to_string()));
    if instance::hand_off(open.as_deref()) {
        tracing::info!(?open, "handed over to the running desktop");
        return;
    }
    let launches = instance::listen();
    tracing::info!(
        version = release::VERSION,
        data = %dirigent_service::data_dir().display(),
        config = %dirigent_service::config_dir().display(),
        "desktop starting"
    );
    // Closing the window quits, on macOS too: gpui's default there keeps a windowless app
    // running, which every later launch would hand its folder to (`instance.rs`) and vanish.
    application().with_assets(icons::Assets).with_quit_mode(QuitMode::LastWindowClosed).run(move |cx: &mut App| {
        config::add_bundled_fonts(cx);
        // What we started goes with us: what the agent runs (commands, MCP servers and
        // screens, when its service runs in here), and the ssh tunnels to `ssh://` hosts.
        // Right in the callback, not in its future: that is polled once the windows are
        // gone, and with them the net thread the service runs on.
        cx.on_app_quit(|_| {
            dirigent_service::shutdown();
            dirigent_service::tunnel::close_all();
            async {}
        })
        .detach();
        cx.bind_keys(input::key_bindings());
        // `secondary-`: cmd on macOS, ctrl elsewhere.
        cx.bind_keys([
            KeyBinding::new("secondary-n", NewThread, Some(app::KEY_CONTEXT)),
            KeyBinding::new("escape", Dismiss, Some(app::KEY_CONTEXT)),
            // `[` reaches the handler even from inside an input; see `Dirigent::toggle_sidebar`.
            KeyBinding::new("[", ToggleSidebar, Some(app::KEY_CONTEXT)),
            KeyBinding::new("]", ToggleLog, Some(app::KEY_CONTEXT)),
            KeyBinding::new("secondary-q", Quit, Some(app::KEY_CONTEXT)),
            // Copies the transcript selection; an input with its own selection handles it first.
            KeyBinding::new("secondary-c", input::Copy, Some(app::KEY_CONTEXT)),
            // The text size (`[fonts] size`), like zoom in a browser; `+` is shift-= on most layouts.
            KeyBinding::new("secondary-=", TextBigger, Some(app::KEY_CONTEXT)),
            KeyBinding::new("secondary-+", TextBigger, Some(app::KEY_CONTEXT)),
            KeyBinding::new("secondary--", TextSmaller, Some(app::KEY_CONTEXT)),
            KeyBinding::new("secondary-0", TextReset, Some(app::KEY_CONTEXT)),
            KeyBinding::new("enter", onboarding::Continue, Some(onboarding::KEY_CONTEXT)),
        ]);
        // After the input's: in one, the open menu's arrows and Enter win.
        cx.bind_keys(menu::key_bindings());
        rekey_repeats(cx);
        // Every keystroke with its key-context stack, at debug (`DIRIGENT_LOG=dirigent_desktop=debug`):
        // the fastest way to debug a binding.
        cx.observe_keystrokes(|ev, _, _| tracing::debug!("key {} ctx={:?}", ev.keystroke.unparse(), ev.context_stack)).detach();

        // An install as pi left it is brought over first, in a window of its own
        // (`migration.rs`), before anything here opens a db or writes a setting.
        let start = move |cx: &mut App| start(open, launches, cx);
        if migration::needed() {
            migration::open(&config::peek(), start, cx);
        } else {
            start(cx);
        }
    });
    tracing::info!("desktop exited");
}

/// Start the app proper: its db, its settings, its window.
fn start(open: Option<String>, launches: Option<futures::channel::mpsc::UnboundedReceiver<instance::Launch>>, cx: &mut App) {
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
    // As it was when last used (`placement`).
    let window_bounds = placement::initial(&cache, cx);
    let window = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(window_bounds),
                window_background,
                // app_id/title are what testing.md keys on to find this window, and the
                // app_id what the app menu entry names (`StartupWMClass`, install.sh):
                // a nightly's windows go under its own entry and icon.
                app_id: Some(release::app().into()),
                titlebar: Some(TitlebarOptions {
                    title: Some(if release::nightly() { "Dirigent Nightly" } else { "Dirigent" }.into()),
                    appears_transparent: titlebar::CUSTOM,
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Dirigent::new(cache, config, window, cx)),
        )
        .unwrap();
    // Focus the root so the global key context (new thread, esc, ...) is active immediately.
    window
        .update(cx, |view, window, cx| {
            window.focus(&view.focus_handle(cx), cx);
            placement::remember(window, cx);
            view.serve_launches(open, launches, cx);
        })
        .unwrap();
    cx.activate(true);
}

/// gpui's Wayland client repeats a held key as the keystroke of its press, whatever modifiers
/// went down or up since: an arrow held before ctrl keeps moving by characters. X11, Windows
/// and macOS repeat it under the modifiers held now, and so does this: a keystroke whose
/// modifiers aren't the window's is dropped and sent again with them. Named keys only (arrows,
/// backspace, enter, ...): gpui leaves shift out of a symbol's keystroke (`!`) by design.
fn rekey_repeats(cx: &mut App) {
    if cx.compositor_name() != "Wayland" {
        return;
    }
    cx.intercept_keystrokes(|ev, window, cx| {
        let held = window.modifiers();
        if ev.keystroke.modifiers == held || ev.keystroke.key.chars().count() == 1 {
            return;
        }
        cx.stop_propagation();
        let keystroke = Keystroke { modifiers: held, ..ev.keystroke.clone() };
        window.defer(cx, move |window, cx| {
            window.dispatch_keystroke(keystroke, cx);
        });
    })
    .detach();
}
