# Testing the desktop UI

Open your private screen (`computer`, `screen: "private"`), `scripts/ui.sh start` the app on it, and operate it with
the screen's tools. Never on the desktop. The app has its own port, data and config: nothing of Seb's is touched.

```
scripts/ui.sh start DISPLAY          # build + (re)launch on your private screen (e.g. :3), filling it
scripts/ui.sh setup [DIR] [MODEL]    # add DIR ("" = <session>/scratch) as a project, pick MODEL; restarts the app
scripts/ui.sh wait-idle [SECS]       # until no thread runs or has jobs (timeout 300 s)
scripts/ui.sh rpc JSON [EXPR]        # one protocol request to the session's service; EXPR is JS over the reply `r`
scripts/ui.sh run CMD...             # CMD with sqlite3, xdotool, magick, DISPLAY, UI_DIR (session dir), UI_PORT
scripts/ui.sh log [N]                # last N lines of the app's output (service and runtime included)
scripts/ui.sh stop                   # kill the app
scripts/ui.sh reset                  # stop + wipe the session's data
```

- The window is the screen (1280x800, scale 1): click where the picture shows it.
- Take the picture after an action in the same script, or in a later message; never in a second tool call of the
  same message: those run at the same time, and the picture shows the app as it was before.
- A look at one region: `get_desktop_state({screenshot_out_file: "<session>/s.png"})`, then
  `scripts/ui.sh run sh -c 'magick $UI_DIR/s.png -crop WxH+X+Y $UI_DIR/part.png'` and read that.
- `start` after every code change; it ends with `ready` or `build failed` (`<session>/build.log`).
- `reset` for a clean state, `wait-idle` instead of `sleep`, `stop` when done.
- The app dies with the screen: open it again and `start`.
- Session: `/tmp/dirigent-ui-<user>/<session>/` (`data/`, `config/`, `app.log`), named after your thread id;
  `UI_SESSION=name` picks another. Service port: 7800 + the display's number.
- Models: `anthropic/claude-opus-5-5` (the default) for parents, `anthropic/claude-haiku-5-5` for children.

```sh
# computer({ screen: "private" })   → names your display, say :3
scripts/ui.sh start :3
scripts/ui.sh setup
# on the screen: ctrl+n, type a prompt, ctrl+Return
scripts/ui.sh wait-idle
scripts/ui.sh stop
```

`rpc` requests are the `ClientMessage` variants of `dirigent_protocol/src/lib.rs`, in snake_case:

```sh
scripts/ui.sh rpc '{"type":"list_threads"}' 'r.threads.map(t => t.id + " " + t.name)'
scripts/ui.sh rpc '{"type":"create_thread","project_id":"…","name":"child","parent":"<thread id>"}'
scripts/ui.sh rpc '{"type":"prompt","thread_id":"…","text":"hi","mode":"prompt","model":"anthropic/claude-haiku-5-5"}'
scripts/ui.sh run sh -c 'sqlite3 $UI_DIR/data/service.db "select id, name from threads"'
```

The app shows what the service pushes at once. Its own state (`data/desktop.db`: model, drafts, pane) is read at
launch only. What it copied: the screen's `clipboard_read({include_text: true})`.

## States that take setting up

- **`ssh://` hosts**: a fake `ssh` in a `UI_PATH` directory (put before `PATH`) that forwards
  `-L 127.0.0.1:L:127.0.0.1:R` to a test service. Always name the port (`ssh://…/<port>`): 7777 is Seb's.
- **Workspaces**: to see `Creating a workspace…` / `Activating nix dev shell…`, `sleep` in the test repo's
  `.git/hooks/post-checkout` and its flake's `shellHook`. Missing-workspace banner: delete the dir under
  `<session>/data/workspaces/`, or `jj workspace forget dirigent-xxxxxxxx`, and reopen the thread (within 10 s).
  Keep test repos outside the session dir.
- **A long thread** (the ruler scrolls past 32 marks): `rpc` prompts in a loop.
- **Sub-agents**: prompt "use the dirigent tool to spawn a sub-agent named … with model
  anthropic/claude-haiku-5-5 whose task is …". A job with `notify: "never"` leaves a finished thread with a job
  running. `create_thread` without a prompt pushes nothing until an `archive_thread`. `open` jumps to a thread.
- **Refused messages**: a model of a provider the session isn't signed in to (Settings › Providers signs out), or
  `UPDATE threads SET model = 'provider/id'` in `service.db` with the app stopped.
- **Compaction** every turn: a test project with this `.pi/settings.json`:
  `{"compaction":{"reserveTokens":5000000,"keepRecentTokens":1}}`. To slow or fail it: `.pi/extensions/x.ts` with a
  `pi.on("session_before_compact", …)` that sleeps, and returns `{ compaction: { get summary() { throw … } } }`.
- **Computer use in the app under test** (`"computer"` in a prompt's `tools`): its `desktop` is your private screen.
  - An app there: `scripts/ui.sh run sh -c 'setsid xcalc -geometry 300x400+500+200 &'` (`nixpkgs#xcalc`).
  - The app back on top: `scripts/ui.sh run sh -c 'xdotool search --name "^Dirigent( Nightly)?$" | xargs xdotool windowraise'`.
  - A covered window is only captured under a compositor: `picom --backend xrender` (`nixpkgs#picom`).
  - The fallback to the desktop: a `UI_PATH` directory with an `Xvfb` that exits 1.
  - Over `rpc`: `screen` events, `get_screen`, and `screen: true` in `list_threads`.
- **The setup** (`onboarding.rs`): `UI_ONBOARDING=1 scripts/ui.sh start :3`. It animates: ~2 s title card, ~0.6 s
  per step, ~1 s fade out.
- **A nightly build** (its title `Dirigent Nightly`, window class, logo, version): `start` with
  `DIRIGENT_BUILD_VERSION` set, and again without it when done (both rebuild):
  `DIRIGENT_BUILD_VERSION=0.0.10-nightly.202611151432 scripts/ui.sh start :3`.
  The session's dirs and port are the script's either way. A nightly's own dirs (`packaging/README.md`) only show
  without `--data-dir`: start the binary by hand with `HOME`, `XDG_DATA_HOME` and `XDG_CONFIG_HOME` in a temp
  dir, and with `DIRIGENT_PORT=<a free port> DIRIGENT_BIND=127.0.0.1`: its default port, 7776, is Seb's nightly.
- **The changelog modal**: lower `kv.last_version` in `data/desktop.db` and `start`. Esc closes it.
- **Window placement** (`kv.window`, `placement.rs`): `start` resizes the window after launch, so relaunch the
  binary as `launch` in `ui.sh` does without the `xdotool` line, and check `xdotool getwindowgeometry`.
- **Open in Dirigent**: run the binary with the session's dirs and a folder,
  `target/debug/dirigent_desktop --data-dir <session>/data --config-dir <session>/config /some/dir`.
  Never without `--data-dir`: that hands off to Seb's desktop.
- **The update button**: `dirigent_api serve` with `S3_PREFIX=test/` and a newer `linux-x86_64` release
  (`dirigent_api publish`, `packaging/README.md`), then
  `DIRIGENT_UPDATE_URL=http://127.0.0.1:<port>/api/v0/updates/<channel> scripts/ui.sh start :3`. The release is a
  tarball of `dirigent/` with the debug binary as `dirigent` and `packaging/linux/install.sh`, signed with your
  own key (`DIRIGENT_UPDATE_KEY=<public key>`). It installs into `<session>/prefix`
  (`<session>/data/logs/install.log`). "Restart when idle" needs an agent working.
- **Forge sign-in**, never against the real forge: a local `vir_central` with a public app (redirect
  `http://127.0.0.1/forge/callback`), `DIRIGENT_FORGE_URL` / `DIRIGENT_FORGE_CLIENT_ID`, and a fake `xdg-open`
  that approves (`POST /api/oauth/authorize?<the url's query>` with `{"allow":true}` and a test user's session as
  bearer, then `curl` the redirect). `dirigent_service --data-dir … login`: `POST /api/oauth/device/decide`.
  Only for the look of Settings › Forge providers: a dead `DIRIGENT_FORGE_URL` (`http://127.0.0.1:9`) and a
  hand-written `data/forge.json` (`null` for signed out by the forge), reread every 5 s:
  `{"forge": "<url>", "client_id": "<id>", "sub": "u1", "username": "seb", "picture": "", "refresh_token": "x"}`.
- **The leaderboard**: a local `dirigent_api serve` with `DIRIGENT_API_DB`, `DIRIGENT_API_BIND`,
  `DIRIGENT_API_URL`, `FORGE_URL` / `DIRIGENT_FORGE_CLIENT_ID` and
  `S3_ENDPOINT=http://s3.invalid S3_REGION=x S3_BUCKET=x`. Without a forge: a stand-in on `DIRIGENT_FORGE_URL` whose `POST /api/oauth/token` answers an RS256
  `at+jwt` (`iss`, `aud` = the form's `resource`, `client_id`, `sub`, `exp`, `preferred_username`) signed with
  `dirigent_auth/src/test_key.der`, served at `/api/oauth/jwks`.

## Gotchas

- **Links open Seb's real browser** (`xdg-open`): put a fake one in `UI_PATH` before clicking any.
- Logs: `DIRIGENT_LOG=dirigent_desktop=debug` (every keystroke with its key context), `dirigent_service=debug`,
  `dirigent=debug` for both. Not a bare `debug`.
- This is gpui's X11 backend on software Vulkan. Wayland-only details (IME, clipboard, decorations) and
  translucent themes are Seb's to check.
- Key repeat (Wayland-only, `rekey_repeats` in `main.rs`) under headless sway:
  `nix shell nixpkgs#sway-unwrapped nixpkgs#wtype nixpkgs#grim`,
  `WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 sway` on a private `XDG_RUNTIME_DIR`
  (config: `output HEADLESS-1 resolution 1100x720`, `default_border none`, `xwayland disable`), the binary launched
  as in `ui.sh` with that `WAYLAND_DISPLAY` and no `DISPLAY` (`ui.sh stop` first).
  `wtype -k End -s 300 -P Left -s 1000 -M ctrl -s 400 -p Left -m ctrl` (the first key of a run is lost), `grim`.
- Fractional scaling: `GPUI_X11_SCALE_FACTOR=1.25 scripts/ui.sh start :3`.

## Non-UI crates

- `dirigent_runtime`: `bun run dirigent_runtime/src/index.ts`, feed JSONL on stdin, e.g.
  `{"type":"load","thread_id":"t1","cwd":"/tmp/x","entries":[]}` then `{"type":"prompt","thread_id":"t1","text":"hi","mode":"prompt"}`.
- `dirigent_service`: the standalone binary is for remote hosts; the desktop embeds the lib. `cargo run -p dirigent_service`, poke it with `websocat ws://localhost:7778/ws` (`nix run nixpkgs#websocat`):
  `{"type":"list_threads","id":1}`, `{"type":"add_project","id":2,"path":"/tmp/x"}`, `{"type":"create_thread","id":3,"project_id":"..."}`, `{"type":"prompt","id":4,"thread_id":"...","text":"hi","mode":"prompt"}`.
  Use `DIRIGENT_PORT=7778 cargo run -p dirigent_service -- --data-dir /tmp/somewhere --config-dir /tmp/somewhere-config`
  to keep a test instance away from the real one.
- `dirigent_cli`: `cargo run -p dirigent_cli -- --config-dir /tmp/somewhere-config pi --help` runs the bundled pi on that agent dir.
- `dirigent_runtime` compiled, as installs run it: `cd dirigent_runtime && bun run build` writes `dist/runtime/`;
  feed `dist/runtime/dirigent_runtime` the same JSONL.
- `dirigent_api`: see `packaging/README.md` and the top of `dirigent_api/src/main.rs`.
- Unit tests: `cargo test --workspace` (protocol round-trips, storage, view parsing). `bun run typecheck`, `bun run lint`, `bun run fmt:check` for the runtime.
