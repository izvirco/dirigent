# Testing the desktop UI

Open your private screen (`computer`, `screen: "private"`), `scripts/ui.sh start` the app on it, and operate it with
the screen's tools. Never on the desktop. The app has its own port, data and config: nothing of Seb's is touched.

```
scripts/ui.sh start DISPLAY          # build + (re)launch on your private screen (e.g. :3), filling it
scripts/ui.sh setup [DIR] [MODEL]    # add DIR ("" = <session>/scratch) as a project, pick MODEL; restarts the app
scripts/ui.sh wait-idle [SECS]       # until no thread runs or has jobs (timeout 300 s)
scripts/ui.sh rpc JSON [EXPR]        # one protocol request to the session's service; EXPR is JS over the reply `r`
scripts/ui.sh run CMD...             # CMD with sqlite3, xdotool, magick, DISPLAY, UI_DIR (session dir), UI_PORT
scripts/ui.sh log [N]                # last N lines of the app's output (its service's included)
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
- Models: by their id at the proxy. `claude-opus-5-5` is `setup`'s default; use `claude-haiku-5-5` and short
  prompts wherever the model doesn't matter (the proxy is Seb's subscription).

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
scripts/ui.sh rpc '{"type":"prompt","thread_id":"…","text":"hi","mode":"prompt","model":"claude-haiku-5-5"}'
scripts/ui.sh run sh -c 'sqlite3 $UI_DIR/data/service.db "select id, name from threads"'
```

The app shows what the service pushes at once. Its own state (`data/desktop.db`: model, drafts, pane) is read at
launch only. What it copied: the screen's `clipboard_read({include_text: true})`.

## States that take setting up

- **A remote host, and one that is offline**: a `dirigent_service` of your own (`--data-dir`, `--config-dir`,
  `DIRIGENT_PORT`, without `DIRIGENT_THREAD_ID`, `DIRIGENT_URL` and `DIRIGENT_DATA_DIR`), named in
  `<session>/config/hosts.toml` under `[hosts]` (`remote = "ws://127.0.0.1:<port>/ws"`), then `start`. Fill it over
  `node scripts/ui-rpc.mjs <port> JSON`. Kill it and the host is offline; `data/desktop.db`'s `outbox` has what the
  app changed meanwhile.
- **`ssh://` hosts**: a fake `ssh` in a `UI_PATH` directory (put before `PATH`) that forwards
  `-L 127.0.0.1:L:127.0.0.1:R` to a test service. Always name the port (`ssh://…/<port>`): 7777 is Seb's.
- **Workspaces**: to see `Creating a workspace…` / `Activating nix dev shell…`, `sleep` in the test repo's
  `.git/hooks/post-checkout` and its flake's `shellHook`. Missing-workspace banner: delete the dir under
  `<session>/data/workspaces/`, or `jj workspace forget dirigent-xxxxxxxx`, and reopen the thread (within 10 s).
  Keep test repos outside the session dir.
- **A long thread** (the ruler scrolls past 32 marks): `rpc` prompts in a loop.
- **Sub-agents**: prompt "use the dirigent tool to spawn a sub-agent named … with model
  claude-haiku-5-5 whose task is …". A job with `notify: "never"` leaves a finished thread with a job
  running. `create_thread` without a prompt pushes nothing until an `archive_thread`. `open` jumps to a thread.
- **Refused messages**: a model the proxy doesn't serve: `"model":"nope"` in an `rpc` prompt, or
  `UPDATE threads SET model = 'nope'` in `service.db` with the app stopped. A host without a proxy: Settings › Proxy ›
  Remove (the setup's proxy step and the model picker's `Set up the proxy` show then).
- **Compaction** every turn: in `<session>/config/agent.toml`, `[compaction]` with `reserve = 5000000` and
  `keep_recent = 1` (the file is read for every turn, no restart).
- **A failed compaction's line, a locked thread**: in `service.db`, with the app stopped:

  ```sql
  INSERT INTO entries (thread_id, seq, id, parent_id, at, kind, json)
    SELECT thread_id, seq + 1, 'f1', id, at, 'compaction_failed', '{"type":"compaction_failed","error":"Compaction failed: 529"}'
    FROM entries WHERE thread_id = '…' ORDER BY seq DESC LIMIT 1;
  UPDATE threads SET locked = 1 WHERE id = '…';
  ```

- **A host on another protocol** (`update that host` in Settings › Hosts, on the status word's tooltip): a
  websocket server that answers `{"type":"hello","host_name":"old","version":"0.0.9"}` and nothing else, as a
  `ws://127.0.0.1:<port>/ws` entry of `<session>/config/hosts.toml`.
- **MCP servers**: `[mcp.<name>]` in that `agent.toml`, or Settings › MCP servers. A stdio server to try:
  `command = "npx"`, `args = ["-y", "@modelcontextprotocol/server-everything"]`.
- **Computer use in the app under test** (`"computer"` in a prompt's `tools`): its `desktop` is your private screen.
  - An app there: `scripts/ui.sh run sh -c 'setsid xcalc -geometry 300x400+500+200 &'` (`nixpkgs#xcalc`).
  - The app back on top: `scripts/ui.sh run sh -c 'xdotool search --name "^Dirigent( Nightly)?$" | xargs xdotool windowraise'`.
  - A covered window is only captured under a compositor: `picom --backend xrender` (`nixpkgs#picom`).
  - A private screen that doesn't start (an error, not the desktop): a `UI_PATH` directory with an `Xvfb` that exits 1.
    The fallback to the desktop is for a machine with no `Xvfb` at all: a `PATH` without one.
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
- **An outdated proxy** (the sidebar's warning, its line in Settings › Proxy): a stand-in CLIProxyAPI, as a test
  instance has no management key. An http server that answers `/v1/models` with
  `{"data": [{"id": "claude-haiku-5-5", "owned_by": "anthropic"}]}`, `/v8/management/credentials` with
  `{"files": []}` and the header `X-CPA-VERSION: 8.0.75`, and `/v8/management/server/latest-version` with
  `{"latest-version": "v8.0.85"}` (a proxy is outdated 10 patch releases behind, or a minor one); then
  Settings › Proxy with its address and any two keys. To see the warning go, answer with a newer `X-CPA-VERSION`
  and click it (Settings › Proxy asks again; left alone, the app asks every hour).
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
- **The picture stops changing, or typing seems to land nowhere**: seen after moving the pointer with `xdotool`
  (a hover), on a screen without a window manager. What was typed and clicked meanwhile is not lost: it took
  effect, or does once the window has the keyboard focus again, so look before doing it over. To set it right:
  `scripts/ui.sh run sh -c 'xdotool search --name "^Dirigent( Nightly)?$" | head -1 | xargs xdotool windowfocus'`.
- **A model under test reads what it can reach**, the session's `data/proxy.json` with the proxy's API key
  included: don't print the tool results of a thread you didn't write the prompt for.
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

- `dirigent_agent`: `cargo run -p dirigent_agent --example run -- --cwd /tmp/x "…"` gives the agent one prompt
  and prints what comes of it (a session without a service, in temp dirs, working in `--cwd` for real, default: here;
  `--model`, `--effort`, `--tools codemode,async_bash`). `--example ask -- "…"` is one request to the proxy,
  streamed (`--model`, `--effort`, `--system "…"`, `--codex`: over the `codex` wire whatever the model,
  `--models`: what the proxy serves). Both default to `claude-haiku-5-5`; their proxy: [below](#tests-against-the-real-thing).
- `dirigent_service`: the standalone binary is for remote hosts; the desktop embeds the lib. `cargo run -p dirigent_service`, poke it with `websocat ws://localhost:7778/ws` (`nix run nixpkgs#websocat`):
  `{"type":"list_threads","id":1}`, `{"type":"add_project","id":2,"path":"/tmp/x"}`, `{"type":"create_thread","id":3,"project_id":"..."}`, `{"type":"prompt","id":4,"thread_id":"...","text":"hi","mode":"prompt","model":"claude-haiku-5-5"}`.
  Use `DIRIGENT_PORT=7778 cargo run -p dirigent_service -- --data-dir /tmp/somewhere --config-dir /tmp/somewhere-config`
  to keep a test instance away from the real one.
- `dirigent_api`: see `packaging/README.md` ("The update api").
- Unit tests: `cargo test --workspace` (protocol round-trips, storage, view parsing, the agent with a scripted model).

## Tests against the real thing

`cargo test` leaves them out (`#[ignore]`); run them with `-- --ignored`:

- `cargo test -p dirigent_agent proxy::live -- --ignored`: the proxy. They spend tokens, asking `claude-haiku-5-5`
  for little. The proxy, theirs and the examples': the one `DIRIGENT_PROXY_URL` and `DIRIGENT_PROXY_KEY` name, else
  this machine's install's (`~/.local/share/dirigent/proxy.json`; an install that is still a 0.0.10 has no API key
  there, so name the proxy then).
- `cua_driver` and `public_http` (`mcp`: the real driver, real servers), `a_private_screen` (computer use, on an
  Xvfb of its own).
