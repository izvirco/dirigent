# Testing the desktop UI

`scripts/ui.sh` runs a debug `dirigent_desktop` on a private, invisible X display (Xvfb) and drives it with
synthetic input. It never touches Seb's screen, mouse, keyboard, data, or his service on :7777, so test whenever
you like. Call it directly (no `nix develop` needed); it pulls its tools from the dev shell itself.

## Controls

```
scripts/ui.sh start [W H]            # build + (re)launch, window = screen = WxH (default 1100x720)
scripts/ui.sh shot [path] [X Y W H]  # screenshot → /tmp/dirigent-ui/default/shot.png, optionally cropped
scripts/ui.sh click X Y [right]      # click
scripts/ui.sh move X Y               # hover (reveals hover-only controls)
scripts/ui.sh drag X1 Y1 X2 Y2       # press, move, release (e.g. the sidebar handle)
scripts/ui.sh scroll X Y N           # N wheel notches down at X Y; negative scrolls up
scripts/ui.sh type "text"            # type text into the focused input
scripts/ui.sh key KEY...             # chords, xdotool names: Return Escape Tab BackSpace ctrl+n shift+Return ctrl+Return
scripts/ui.sh log [N]                # last N lines of app stdout/stderr (incl. embedded service + pi runtime)
scripts/ui.sh stop                   # kill the app and its display
scripts/ui.sh reset                  # stop + wipe the session's data: fresh service.db/desktop.db
```

- **Coordinates are screenshot pixels.** Scale is 1 and the window fills the screen at (0, 0), so a position you read
  off a shot is exactly what you pass to `click`.
- Read shots with the `read` tool. Crop (`shot path X Y W H`) when you only care about one region; it saves context.
- Every input command waits 150 ms and `shot` 300 ms first, which is enough for clicks and typing. Network and agent
  work take longer: `sleep` before the shot.
- `start` rebuilds and relaunches the app; run it again after every code change. The data survives restarts
  (projects, threads, picked model); use `reset` for a clean slate.

## Isolation and sessions

Each session lives in `/tmp/dirigent-ui/<session>/` and has its own Xvfb display, service port (7800+),
`XDG_DATA_HOME` (`data/`), `XDG_CONFIG_HOME` (`config/`, so `hosts.toml` starts empty and `config.toml` is written with defaults), `app.log`, and pid files. The desktop runs its local service embedded on that port
(`DIRIGENT_PORT`), so prompting tests work out of the box and real pi auth is used. Set `UI_SESSION=name` on every
call to run several sessions side by side, e.g. two agents at once. `stop`/`reset` kill only the session's own
process groups.

## Typical loop

A fresh session has no projects and no model picked. Use `anthropic/claude-opus-5-5`; the default model may be
rate-limited.

```sh
mkdir -p /tmp/dirigent-ui/scratch
scripts/ui.sh start
scripts/ui.sh move 120 133                    # hover the Projects header…
scripts/ui.sh click 218 133                   # …and click its "+" → path input focused
scripts/ui.sh type /tmp/dirigent-ui/scratch
scripts/ui.sh key Return                      # adds the project, opens the new-thread page for it
scripts/ui.sh click 360 390                   # model chip → presets page; its "All models ›" link is at (676, 309)
scripts/ui.sh click 676 309                   # all-models page, search focused
scripts/ui.sh type "opus 5.5"; scripts/ui.sh key Return   # Enter picks the top match
scripts/ui.sh click 600 343                   # composer
scripts/ui.sh type "Reply with just the word pong."
scripts/ui.sh key ctrl+Return                 # creates the thread and sends
sleep 10; scripts/ui.sh shot                  # then read /tmp/dirigent-ui/default/shot.png
scripts/ui.sh stop
```

Landmarks at 1100×720 with the default 240 px sidebar (they move when the layout changes; trust the shot):
compose button (219, 21), Inbox (55, 57), Workpool (66, 85), Projects "+" (218, 133, hover-revealed), first project
(60, 159) with its hover-revealed `⋯` (176) and new-thread `+` (210) — the latter three shift down ~73 px per thread card listed under Inbox / Workpool (collapse them with the
chevron at x = 214 on the nav row), first list row (500, 68), draft page project chip (388, 279) and model chip (360, 390), thread composer
model chip (370, 667), thread header snooze / pin / archive / delete (972, 28) / (1002, 28) / (1032, 28) / (1062, 28); a sidebar card's snooze replaces its run time on hover (x = 207). The sidebar's bottom bar: hide/show (30, 699), settings gear (210, 699) → Settings › Config: theme chip (873, 131), font fields (736, 202 / 253 / 304; apply as you type), title model chip (850, 471), reasoning chip (876, 516), `Test` (422, 698) opens the sample message with `Name it` under it, the telemetry switch is at the bottom; the Hosts tab is at (414, 28): name field (465, 313), address field (745, 313), Enter adds; the Usage tab is at (474, 28): range chips `1d 7d 30d custom` and `‹ ›` top right, legend rows under the chart toggle series.
An `ask_user` form sits above the composer: a stack: the open question as a card (option rows, a text-field row for free-text / custom answers, `Next` / `Answer` at its bottom right; Enter in the field = Next) with the other questions as one-line rows above (answered) and below (pending) it, `Dismiss` in the header; the thread shows `Asking` in the header and an amber question mark in lists. Settings › Config has an "Agent tools" section with a switch per tool. **Workspaces**: when the draft's project is a git / jj repo the composer starts with an `in project` chip; clicking it opens a popup: `in project` on top, then `New workspace from`, a searchable list of branches / tags / bookmarks with `HEAD` (`@-` for jj) first (Enter = top match, or the typed text as is; refreshes every 2 s while open); picking one turns the chip into `workspace from <base>` (remembered; the `in project` row undoes it); a dirty checkout or another thread working in it puts a centered warning line under the composer, ending in `isolate it`. A thread in its project directory has the same chip and popup, and picking moves it right away; while the checkout is dirty the chevron is faint, the click does nothing and hovering shows why. One in a workspace shows `dirigent-xxxxxxxx` in blue, a menu above it: copy path, back to project (remove / keep on disk). Archiving a thread with a workspace opens a confirm under the archive button (remove at ~y 89, keep at ~y 115). Delete a workspace dir under `/tmp/dirigent-ui/default/data/dirigent/workspaces/` (named like its branch, `dirigent-xxxxxxxx`) and (re)open the thread: an amber banner with `Recreate` / `Back to project` sits above the composer within 10 s. The sidebar's drag handle is at
x = sidebar width; `[` hides the sidebar and hovering x < 10 peeks it. Clicking a thread's work line (`› model effort ·
duration · n tool calls`, under the user bubble) or `]` opens the 400 px work log panel on the pane's right; its drag
handle is the hairline on its left, and `]` / Esc (when nothing is running) close it. The ruler is the 28 px strip at the pane's left edge: the marks are vertically centered at a 7 px pitch, so mark `i` of `n` is at y = list middle + 7·(i − (n − 1) / 2); `move` onto it for the preview, `click` to center that message.

## Gotchas

- Popups snap inside the window. A tall menu can end up under the next click target, so take a shot after opening one.
- `DIRIGENT_DEBUG_KEYS=1 scripts/ui.sh start` logs every keystroke the app receives with its key-context stack to
  `log` (fastest way to debug a binding). `start` passes your environment through to the app.
- This runs gpui's **X11** backend with software Vulkan (lavapipe). Layout, input and behaviour match Wayland.
  Wayland-only details (IME, clipboard, window decorations) need a check on the real desktop by Seb.

## How it works (if you need to go off-script)

- `Xvfb -displayfd` picks a free display; the number is in `/tmp/dirigent-ui/<session>/display`. Then any X tool works:
  `DISPLAY=:N xdotool …`, `DISPLAY=:N import -window root out.png`.
- The app is launched with `VK_ICD_FILENAMES=$DIRIGENT_UI_VK_ICD` (lavapipe; Xvfb can't present GPU frames),
  `XDG_DATA_HOME=<session>/data`, `XDG_CONFIG_HOME=<session>/config`, `DIRIGENT_PORT=<port>` and
  `DIRIGENT_BIND=127.0.0.1` (the embedded service stays off the tailnet), in its own process group (`setsid`).
- The tool paths from `nix develop` are cached in `/tmp/dirigent-ui/devenv-<hash>.sh`, keyed by `flake.nix` +
  `flake.lock`, so calls take well under a second. `start` still builds through `nix develop`.

## Non-UI crates

- `dirigent_runtime`: `bun run dirigent_runtime/src/index.ts`, feed JSONL on stdin, e.g.
  `{"type":"load","thread_id":"t1","cwd":"/tmp/x","entries":[]}` then `{"type":"prompt","thread_id":"t1","text":"hi","mode":"prompt"}`.
- `dirigent_service`: the standalone binary is for remote hosts; the desktop embeds the lib. `cargo run -p dirigent_service`, poke it with `websocat ws://localhost:7777/ws` (`nix run nixpkgs#websocat`):
  `{"type":"list_threads","id":1}`, `{"type":"create_thread","id":2,"cwd":"/tmp/x"}`, `{"type":"prompt","id":3,"thread_id":"...","text":"hi","mode":"prompt"}`.
  Use `XDG_DATA_HOME=/tmp/somewhere DIRIGENT_PORT=7778` to keep a test instance away from the real one.
- Unit tests: `cargo test --workspace` (protocol round-trips, storage, view parsing). `bun run typecheck`, `bun run lint`, `bun run fmt:check` for the runtime.
