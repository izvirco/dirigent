# Dirigent — notes for agents

Read `testing.md` (how to drive the UI) first.

## Rules

- Seb dogfoods a `target/release/dirigent_desktop` with its service on :7777. Never kill or touch either. Test the UI only through `scripts/ui.sh`: it runs a debug build on its own invisible display, port and data dir, so you can't disturb Seb's desktop or data (see `testing.md`).
- Never `pkill -f <string>` from a shell whose own command line contains `<string>` — you kill your shell. Anchor patterns with `^/abs/path`.
- `dirigent_service` is a lib + bin. The desktop embeds the lib and runs the local service in-process; the binary is only for remote hosts.
- `dirigent_api` is the update api on Seb's server and the release workflow's `publish`; `packaging/` has the Windows installer and the Linux install script. How releases and updates work, and what must stay compatible for installs in the field: `packaging/README.md`. The version is `[workspace.package] version`; releases run by hand (`.github/workflows/release.yml`).
- One desktop per data dir: a second launch hands its folder to the running one and exits (`instance.rs`, keyed on `<data dir>/desktop.port`). Always pass `--data-dir` to a desktop you start by hand, or it hands off to Seb's.
- `flake.nix` also packages the three binaries for nix (`packaging/README.md`). Changing `Cargo.lock` or `bun.lock` changes a hash there: bump it (`lib.fakeHash`, then `nix build .#dirigent_service` prints the right one).
- Never set `DIRIGENT_BUILD_UPDATE_URL` outside the release workflow: it bakes the update url into the binary. Tests use `DIRIGENT_UPDATE_URL` at run time.
- Seb's data is live: an existing `service.db` / `desktop.db` must open with the new code. Schema changes are additive and self-applying (`CREATE TABLE IF NOT EXISTS`, `ensure_column` in `storage.rs`); new wire fields get `#[serde(default)]`. No version bumps. Test dbs can be wiped freely (`scripts/ui.sh reset`). `--data-dir` / `--config-dir` (both binaries) point an instance away from the real dirs.
- no component library, hand-rolled gpui.
- Log with `tracing` (`info!` for lifecycle events, `warn!` / `error!` for failures, `debug!` for chatter), never `eprintln!` (except where there is no logger yet: `init_dirs` failing, `init_logging` itself). Both binaries log to stderr and `<data dir>/logs/<desktop|service>.log` (`logging.rs`); `DIRIGENT_LOG` adds filter directives. Nothing per streamed event at info.

## Gotchas that cost time

- **Sizes zoom with `[fonts] size`**: write lengths as `theme::u(n)` (design px, `n` at the default 13) and text sizes as `theme::text::{xs, sm, base, lg}`, never gpui's `px(n)` / `.text_sm()`. `px` is for hairlines and pixel math (mouse positions, paint); divide by `theme::zoom()` to turn pixels into design px. gpui's rem spacing (`p_2`, `gap_1`) zooms through the window's rem size.

- **pi persistence**: messages are appended to `SessionManager` silently; `entry_appended` only fires for custom entries. The runtime diffs `getEntryCount()` after each event and emits `entry` messages. Don't rely on `entry_appended`.
- **gpui popups**: an `anchored()` child is `position: absolute` and lands at the flex _static position_ (e.g. centered in its parent). Wrap it in a zero-size `absolute().top_0().left_0()` div (see `menu.rs`).
- **gpui `svg()`** ignores inherited text color; pass it explicitly (`icons::icon(name, color)`).
- **gpui `img()` with `max_w` / `max_h`** can overflow its parent: gpui makes both its sizes definite and taffy lays it out taller than the parent counts. Size a box from the bitmap's pixel size and let the img fill it (`thread::inline_image`); an aspect-ratio box goes in a plain block div, not straight into a flex column.
- **gpui `flex_1()` inside an `overflow_y_scroll()` container** stretches to fill it and pushes later siblings to the bottom. Lists that share a scroll container must not grow (see `lists::scroll`).
- **gpui `uniform_list`** sizes every item from item 0 and lays items out at their content width: make them all the same height and `w_full()` (see `lists::thread_list`).
- **Hover-revealed controls**: `group("row")` on the row + `opacity(0.).group_hover("row", |d| d.opacity(1.))` on the control; `cx.stop_propagation()` in its click handler so the row doesn't also fire.
- **Hover-revealed overlays** (sidebar peek): make the hot zone and the panel _one_ element whose width depends on the hover state, so a single `on_hover` covers both. Two elements flicker as the mouse crosses from one to the other.
- **gpui `Animation::with_max_fps`** never redraws (its throttled notify doesn't reach the window): leave it off and let `with_animation` request frames.
- **gpui `Context::spawn` / listeners** often lack a `Window`; focus via `Dirigent::focus()` (deferred through the stored `AnyWindowHandle`).
- **oxfmt reformats TS to 2 spaces** — after `bun run fmt`, string-based patching of the runtime files silently misses; read the file first.
- **Default pi model may be rate-limited**; use `anthropic/claude-opus-5-5` for tests. Pick it in the model chip, it's remembered.
- **Clicking in tests**: `scripts/ui.sh click X Y` takes screenshot pixels (scale 1, window at 0,0). Hover-revealed controls need a `move` onto their row first.
- **Screenshots in tests**: never `read` a shot in the same tool batch as the `ui.sh … shot` that takes it: the read runs first (an empty file, or the previous frame for a reused path). That is not render lag; read in the next call. Set state up with `ui.sh setup` / `rpc`, wait for agents with `ui.sh wait-idle`.
- **The `dirigent` tool is a ws client of the service** (`dirigent_runtime/src/client.ts`, url from `DIRIGENT_URL`; other hosts, from `ListHosts`, the same way, directly): add a capability as a protocol message + service handler, then a `Dirigent` helper; don't add a runtime→service side channel. Notices to a parent thread (a sub-agent ended) come from the service (`notify_thread`), never from the runtime, which may be restarted meanwhile.
- **`cargo test -p dirigent_service` needs FTS5**: the bundled sqlite has it; a system sqlite may not.

## Windows

The desktop and service also build natively on Windows (MSVC: rustup + Visual Studio Build Tools with the C++ workload, whose Windows SDK has the `fxc.exe` gpui compiles its shaders with; bun on `PATH`; Git for Windows, whose bash runs agents' `bash` / `async_bash`). `bun install`, then `cargo build --release -p dirigent_desktop`; like on Linux the binary finds the runtime in its checkout. An install has it compiled next to the binary instead (`runtime/dirigent_runtime.exe`, `runtime_cmd`). Config lives in `%APPDATA%\dirigent`, data and logs in `%LOCALAPPDATA%\dirigent` (dated log files only: no `desktop.log` symlink). A release build has no console, so it logs to the file alone.

- What differs is in `dirigent_service/src/platform.rs` (default dirs, host name, `command()`) plus a few `cfg`s: fonts (`config.rs`, no fontconfig), the primary selection (`input::write_to_primary`), `async_bash`'s shell and kill (`tools.ts`), no nix shell (`sessions.ts`).
- Start programs that also run on Windows through `platform::command`, not `Command::new`: it keeps them from flashing a console window there.
- Paths from a host may be Windows paths (`C:\x\y`): the desktop splits them at `\` too (`projects::basename`, `add_project::split`).
- `build.rs` downloads Lilex (embedded in the binary) unless `DIRIGENT_LILEX_DIR` points at its TTFs; the dev shells set it, so builds there work offline. On a Windows target it also compiles the icon and version info into the exe (resource 1 is also gpui's window icon).
- `scripts/ui.sh` is Linux-only; Seb tests on Windows himself. You can still check that it compiles: `nix develop .#windows -c cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu`.

## Checks before you're done

`cargo fmt --all`, `cargo clippy --workspace`, the Windows `cargo clippy` above when you touched platform code, `cargo test --workspace`, `bun run typecheck`, `bun run lint`, `bun run fmt`.
