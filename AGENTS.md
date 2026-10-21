# Dirigent — notes for agents

Read `plan.md` (architecture) and `testing.md` (how to drive the UI) first.

## Rules

- Seb dogfoods a `target/release/dirigent_desktop` with its service on :7777. Never kill or touch either. Test the UI only through `scripts/ui.sh`: it runs a debug build on its own invisible display, port and data dir, so you can't disturb Seb's desktop or data (see `testing.md`).
- Never `pkill -f <string>` from a shell whose own command line contains `<string>` — you kill your shell. Anchor patterns with `^/abs/path`.
- `dirigent_service` is a lib + bin. The desktop embeds the lib and runs the local service in-process; the binary is only for remote hosts.
- No db migrations: when a schema changes, delete `~/.local/share/dirigent/{service,desktop}.db*`. No version bumps.
- no component library, hand-rolled gpui.

## Gotchas that cost time

- **pi persistence**: messages are appended to `SessionManager` silently; `entry_appended` only fires for custom entries. The runtime diffs `getEntryCount()` after each event and emits `entry` messages. Don't rely on `entry_appended`.
- **gpui popups**: an `anchored()` child is `position: absolute` and lands at the flex _static position_ (e.g. centered in its parent). Wrap it in a zero-size `absolute().top_0().left_0()` div (see `menu.rs`).
- **gpui `svg()`** ignores inherited text color; pass it explicitly (`icons::icon(name, color)`).
- **gpui `flex_1()` inside an `overflow_y_scroll()` container** stretches to fill it and pushes later siblings to the bottom. Lists that share a scroll container must not grow (see `lists::scroll`).
- **Hover-revealed controls**: `group("row")` on the row + `opacity(0.).group_hover("row", |d| d.opacity(1.))` on the control; `cx.stop_propagation()` in its click handler so the row doesn't also fire.
- **Hover-revealed overlays** (sidebar peek): make the hot zone and the panel _one_ element whose width depends on the hover state, so a single `on_hover` covers both. Two elements flicker as the mouse crosses from one to the other.
- **gpui `Context::spawn` / listeners** often lack a `Window`; focus via `Dirigent::focus()` (deferred through the stored `AnyWindowHandle`).
- **oxfmt reformats TS to 2 spaces** — after `bun run fmt`, string-based patching of the runtime files silently misses; read the file first.
- **Default pi model may be rate-limited**; use `anthropic/claude-opus-5-5` for tests. Pick it in the model chip, it's remembered.
- **Clicking in tests**: `scripts/ui.sh click X Y` takes screenshot pixels (scale 1, window at 0,0). Hover-revealed controls need a `move` onto their row first.

## Checks before you're done

`cargo fmt --all`, `cargo clippy --workspace`, `cargo test --workspace`, `bun run typecheck`, `bun run lint`, `bun run fmt`.
