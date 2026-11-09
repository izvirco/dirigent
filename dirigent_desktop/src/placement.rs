//! Where the window opens: as it was when last used (size, position, maximized /
//! fullscreen), kept in `kv`, so every launch (an update relaunches too) reopens it there.
//! Per data dir, like the rest of `desktop.db`. Wayland doesn't let a window place itself:
//! there only the size and state come back. On macOS gpui keeps a position relative to the
//! window's display and opens on the main one: there it comes back on the main display.

use crate::app::Dirigent;
use crate::cache::Cache;
use gpui::{App, Bounds, Context, Pixels, Task, Window, WindowBounds, point, px, size};
use std::time::Duration;

/// `kv`: `<state> <x> <y> <width> <height>`, the state `windowed`, `maximized` or
/// `fullscreen` and the bounds the windowed ones (what un-maximizing restores; X11 reports
/// the maximized ones), in logical pixels.
const KEY: &str = "window";
/// Nothing saved, or what was saved no longer fits: this size, centered.
const DEFAULT_SIZE: (f32, f32) = (1100., 720.);
/// How far below the window's top edge the middle of its title bar is, about.
const TITLE_BAR_MIDDLE: Pixels = px(16.);
/// A drag or a resize reports every frame: save once the window has held still this long.
/// A quit within it keeps the state from before.
const SETTLE: Duration = Duration::from_millis(500);

/// How the window opens: as `remember` saved it, if that still fits the displays.
pub fn initial(cache: &Cache, cx: &App) -> WindowBounds {
    let saved = cache.get(KEY);
    // No primary display: Wayland, where the compositor places the window, and gpui's
    // display sizes are off under fractional scaling (it divides by the output's integer
    // scale), so there is nothing to check against: the saved state applies as is.
    let displays: Option<Vec<_>> = cx.primary_display().map(|_| cx.displays().iter().map(|d| d.bounds()).collect());
    let default = Bounds::centered(None, size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)), cx);
    let bounds = restore(saved.as_deref(), displays.as_deref(), default);
    tracing::info!(?saved, ?displays, ?bounds, "window opens");
    bounds
}

/// Save the window's state whenever it changes, once it settles.
pub fn remember(window: &mut Window, cx: &mut Context<Dirigent>) {
    let mut pending = Task::ready(());
    cx.observe_window_bounds(window, move |_, window, cx| {
        // Read once settled: on X11 a maximize reports the new size before the new state.
        let save = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            this.update_in(cx, |this, window, _| {
                // Minimized (or covered): Windows then reports a maximized window as windowed,
                // X11 as not maximized. What was saved before stays.
                if !window.is_visible() {
                    return;
                }
                if let Err(e) = this.cache.lock().unwrap().set(KEY, &encode(window.window_bounds())) {
                    tracing::warn!("save the window bounds: {e:#}");
                }
            })
            .ok();
        });
        // Dropping the save still pending cancels it.
        drop(std::mem::replace(&mut pending, save));
    })
    .detach();
}

fn encode(state: WindowBounds) -> String {
    let (name, b) = match state {
        WindowBounds::Windowed(b) => ("windowed", b),
        WindowBounds::Maximized(b) => ("maximized", b),
        WindowBounds::Fullscreen(b) => ("fullscreen", b),
    };
    let [x, y, w, h] = [b.origin.x, b.origin.y, b.size.width, b.size.height].map(f32::from);
    format!("{name} {x} {y} {w} {h}")
}

fn parse(s: &str) -> Option<WindowBounds> {
    let mut parts = s.split_whitespace();
    let state = match parts.next()? {
        "windowed" => WindowBounds::Windowed,
        "maximized" => WindowBounds::Maximized,
        "fullscreen" => WindowBounds::Fullscreen,
        _ => return None,
    };
    let numbers: Vec<f32> = parts.map(|p| p.parse().ok().filter(|n: &f32| n.is_finite())).collect::<Option<_>>()?;
    let [x, y, w, h] = numbers[..] else { return None };
    (w > 0. && h > 0.).then(|| state(Bounds::new(point(px(x), px(y)), size(px(w), px(h)))))
}

/// The saved state if its bounds still fit `displays` (`None`: unknown, it does), else
/// `default`. A window that was maximized or fullscreen stays so (on the default display)
/// when its display is gone.
fn restore(saved: Option<&str>, displays: Option<&[Bounds<Pixels>]>, default: Bounds<Pixels>) -> WindowBounds {
    match saved.and_then(parse) {
        Some(state) if displays.is_none_or(|d| fits(state.get_bounds(), d)) => state,
        Some(WindowBounds::Maximized(_)) => WindowBounds::Maximized(default),
        Some(WindowBounds::Fullscreen(_)) => WindowBounds::Fullscreen(default),
        _ => WindowBounds::Windowed(default),
    }
}

/// Whether bounds saved earlier still fit the displays: the middle of the title bar on one
/// of them, so the window can be grabbed and moved, and no bigger than one of them. Not
/// necessarily the same one, so a window spanning two displays comes back too.
fn fits(b: Bounds<Pixels>, displays: &[Bounds<Pixels>]) -> bool {
    let grip = point(b.center().x, b.origin.y + TITLE_BAR_MIDDLE);
    displays.iter().any(|d| d.contains(&grip)) && displays.iter().any(|d| b.size.width <= d.size.width && b.size.height <= d.size.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    #[test]
    fn states_round_trip() {
        for state in [
            WindowBounds::Windowed(bounds(10., 20., 1400., 860.)),
            WindowBounds::Maximized(bounds(-1920., 0., 800.5, 600.)),
            WindowBounds::Fullscreen(bounds(0., 0., 1100., 720.)),
        ] {
            assert_eq!(parse(&encode(state)), Some(state));
        }
        for bad in
            ["", "windowed", "windowed 1 2 3", "windowed 1 2 3 4 5", "minimized 0 0 10 10", "windowed 0 0 0 720", "windowed 0 0 NaN 720"]
        {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn falls_back_unless_the_saved_bounds_fit() {
        // A laptop with a bigger monitor to its right.
        let displays = [bounds(0., 0., 1536., 864.), bounds(1536., 0., 2560., 1440.)];
        let default = bounds(218., 72., 1100., 720.);
        let restore = |saved| restore(Some(saved), Some(&displays), default);

        assert_eq!(restore("windowed 1800 100 2000 1200"), WindowBounds::Windowed(bounds(1800., 100., 2000., 1200.)));
        // Half off the screen's left edge, the title bar's middle still on it.
        assert_eq!(restore("windowed -500 100 1100 720"), WindowBounds::Windowed(bounds(-500., 100., 1100., 720.)));
        // Across both, its title bar's middle on the laptop.
        assert_eq!(restore("windowed 0 0 2000 1200"), WindowBounds::Windowed(bounds(0., 0., 2000., 1200.)));
        assert_eq!(restore("maximized 1536 0 1200 800"), WindowBounds::Maximized(bounds(1536., 0., 1200., 800.)));

        // On a monitor that's gone, title bar off the bottom, bigger than any display, garbage.
        for saved in ["windowed 4200 100 800 600", "windowed 100 1430 800 600", "windowed 0 0 2600 1200", "whatever"] {
            assert_eq!(restore(saved), WindowBounds::Windowed(default), "{saved}");
        }
        // Maximized on a monitor that's gone: still maximized.
        assert_eq!(restore("maximized 4200 0 1200 800"), WindowBounds::Maximized(default));
        assert_eq!(super::restore(None, Some(&displays), default), WindowBounds::Windowed(default));

        // Displays unknown (Wayland): what was saved, if it parses.
        assert_eq!(super::restore(Some("windowed 0 0 5000 3000"), None, default), WindowBounds::Windowed(bounds(0., 0., 5000., 3000.)));
        assert_eq!(super::restore(Some("whatever"), None, default), WindowBounds::Windowed(default));
    }
}
