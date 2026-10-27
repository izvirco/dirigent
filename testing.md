# Testing the desktop UI

`scripts/ui.sh` runs a debug `dirigent_desktop` on a private, invisible X display (Xvfb) and drives it with
synthetic input. It never touches Seb's screen, mouse, keyboard, data, or his service on :7777, so test whenever
you like. Call it directly (no `nix develop` needed); it pulls its tools from the dev shell itself.

## Controls

```
scripts/ui.sh start [W H]            # build + (re)launch, window = screen = WxH (default 1100x720)
scripts/ui.sh setup [DIR] [MODEL]    # add DIR (default <session>/scratch) as a project, pick MODEL (opus 5.5),
                                     #   land on its new-thread page with the composer focused. Restarts
                                     #   the app: runs in the session die, the pane resets
scripts/ui.sh shot [path] [X Y W H]  # screenshot → a fresh <session>/shot-XXXX.png (printed), optionally cropped
scripts/ui.sh click X Y [right]      # click
scripts/ui.sh move X Y               # hover (reveals hover-only controls)
scripts/ui.sh drag X1 Y1 X2 Y2       # press, move, release (e.g. the sidebar handle)
scripts/ui.sh scroll X Y N           # N wheel notches down at X Y; negative scrolls up
scripts/ui.sh type "text"            # type text into the focused input
scripts/ui.sh key KEY...             # chords, xdotool names: Return Escape Tab BackSpace ctrl+n shift+Return ctrl+Return
scripts/ui.sh wait-idle [SECS]       # until no thread runs or has async_bash jobs, for ~2 s (timeout 300 s)
scripts/ui.sh rpc JSON [EXPR]        # one protocol request to the session's service (`id` added), print the reply
scripts/ui.sh run CMD...             # CMD with the dev-shell tools (sqlite3, xclip, xdotool, magick) and
                                     #   DISPLAY, UI_DIR (session dir), UI_PORT (service port) set
scripts/ui.sh log [N]                # last N lines of app stdout/stderr (incl. embedded service + pi runtime)
scripts/ui.sh stop                   # kill the app and its display
scripts/ui.sh reset                  # stop + wipe the session's data: fresh service.db/desktop.db
```

- **Coordinates are screenshot pixels.** Scale is 1 and the window fills the screen at (0, 0), so a position you read
  off a shot is exactly what you pass to `click`.
- Read shots with the `read` tool, in the call _after_ the shot: in the same batch the read runs first and gets
  an empty file (that is not render lag; there is none under Xvfb). Crop (`shot path X Y W H`) when you
  only care about one region; it saves context, but a cropped shot's pixels are offset by its X Y: add them back
  before you click.
- Every input command waits 150 ms and `shot` 300 ms first, which is enough for clicks and typing. For agent work use
  `wait-idle`, not a guessed `sleep`.
- `start` rebuilds and relaunches the app; run it again after every code change. Its last line says `ready` or
  `build failed` (with the errors above it; full output in `<session>/build.log`). It runs `bun install` in a
  checkout without `node_modules`. Builds go to one target dir shared by every checkout and workspace
  (`~/.cache/dirigent-ui/target`), so a fresh workspace isn't a cold build.
- The data survives restarts (projects, threads, picked model, the pane you were on), so a restart can land you
  somewhere else than the last shot showed: take a shot before clicking on. `reset` for a clean slate.
- After `start`, a click chain without shots in between is only safe on a known layout (`setup` gives you one).

## Isolation and sessions

Each session lives in `/tmp/dirigent-ui/<session>/` and has its own Xvfb display, service port (7800 + display number),
data dir (`data/`: `service.db`, `desktop.db`, `workspaces/`, `jobs/`), config dir (`config/`, so `hosts.toml` starts
empty and `config.toml` is written with defaults), `app.log`, and pid files. The app gets them as
`--data-dir` / `--config-dir` (not `XDG_*`, so the agents it runs keep the real git / jj config). It runs its local
service embedded on that port (`DIRIGENT_PORT`), so prompting tests work out of the box and real pi auth is used.

The session defaults to the last 8 characters of your thread id (`PI_SESSION_ID`; `default` outside dirigent), so
every agent has its own without doing anything; `start` prints the name. Set `UI_SESSION=name` to pick one, e.g. to
look at another agent's session. `stop`/`reset` kill only the session's own process groups. `stop` when you're done. Sessions made before the
`--data-dir` change kept their dbs in `data/dirigent/` and now start empty: `reset` them.

## Typical loop

```sh
scripts/ui.sh start
scripts/ui.sh setup                           # project <session>/scratch, opus 5.5, new-thread page, composer focused
scripts/ui.sh type "Reply with just the word pong."
scripts/ui.sh key ctrl+Return                 # creates the thread and sends
scripts/ui.sh wait-idle; scripts/ui.sh shot   # then, in the next call, read the path it printed
scripts/ui.sh stop
```

`setup DIR` takes any directory (a git / jj repo for workspace tests: `git -c user.name=t -c user.email=t@t commit`
if the repo needs a first commit). Use `anthropic/claude-opus-5-5` for the parent and `anthropic/claude-haiku-5-5`
for children; the default model may be rate-limited.

**State without clicking.** `rpc` speaks the service protocol (`dirigent_protocol/src/lib.rs`; requests are the
`ClientMessage` variants in snake_case). `EXPR` is JavaScript over the reply `r`; arrays print one item per line:

```sh
scripts/ui.sh rpc '{"type":"list_threads"}' 'r.threads.map(t => t.id + " " + t.name)'
scripts/ui.sh rpc '{"type":"create_thread","project_id":"…","name":"child","parent":"<thread id>"}'
scripts/ui.sh rpc '{"type":"prompt","thread_id":"…","text":"hi","mode":"prompt","model":"anthropic/claude-haiku-5-5"}'
scripts/ui.sh run sh -c 'sqlite3 $UI_DIR/data/service.db "select id, name from threads"'
scripts/ui.sh run xclip -o -selection clipboard   # what the app copied
```

The app sees pushes from the service at once (a thread created over `rpc` shows up in the sidebar). The desktop's own
state (picked model, drafts, pane) lives in `data/desktop.db` and is read at launch only.

Landmarks at 1100×720 with the default 240 px sidebar (they move when the layout changes; trust the shot):
compose button (217, 19) with the Projects folder (191, 19) left of it (lit while the Projects page is open). Pinned
projects sit right under the title, the first at (43, 53) and the next ones 32 px apart, each with a hover-revealed
new-thread `+` (217); drag one onto another row's y to reorder. Nothing is pinned on a fresh db; a project added from
the Projects page is pinned at once. The Inbox and Workpool rows only show once they have a thread: Inbox right under
the pins (at (35, 53) with none) with its cards below it; the Workpool row sits above the bottom bar (its cards stack
up on it; folded by default, the chevron at x = 217 on a hovered nav row folds / unfolds, remembered across restarts).
Projects page (folder): a row per project, pin toggle at x ≈ 333 (first row y = 81, rows 40 px apart), hover-revealed
`⋯` at x ≈ 1006 (link / unlink / remove), the add form's path field under the rows (a host chip before it with more
than one host); Esc goes back. The gear is lit on the settings pages.
First list row (500, 58), draft page project chip (395, 289) and model chip (365, 379) → its "All models ›" link (680, 309), all-models search focused, Enter picks the top match.
The draft's model chip moves right (to ~425–495) when the project is a git / jj repo: the `in project` chip comes first. Thread composer
model chip (365, 676), thread header snooze / pin / archive / delete (942, 19) / (972, 19) / (1002, 19) / (1033, 19) and the right panel's icon button (1070, 19) at its far right; a sidebar card's snooze replaces its run time on hover (x = 216). The sidebar's bottom bar: hide/show (22, 699), settings gear (217, 699) → Settings › Config: theme chip (875, 122), font fields (740, 193 / 244 / 295; apply as you type), title model chip (846, 481), reasoning chip (879, 526), `Test` below the prompt (`scroll 665 400 10` first) opens the sample message with `Name it` under it, the telemetry switch is at the bottom; the Hosts tab is at (429, 19): name field (469, 228), address field (749, 228), Enter adds; the Usage tab is at (488, 19): range chips `1d 7d 30d custom` and `‹ ›` top right, legend rows under the chart toggle series.
An `ask_user` form sits above the composer: a stack: the open question as a card (option rows, a text-field row for free-text / custom answers, `Next` / `Answer` at its bottom right; Enter in the field = Next) with the other questions as one-line rows above (answered) and below (pending) it, `Dismiss` in the header; the thread shows an amber question mark before its title in the header and in lists. Settings › Config has an "Agent tools" section with a switch per tool. **Workspaces**: when the draft's project is a git / jj repo the composer starts with an `in project` chip; clicking it opens a popup: `in project` on top, then `New workspace from`, a searchable list of branches / tags / bookmarks with `HEAD` (`@-` for jj) first (Enter = top match, or the typed text as is; refreshes every 2 s while open); picking one turns the chip into `workspace from <base>` (remembered; the `in project` row undoes it); a dirty checkout or another thread working in it puts a centered warning line under the composer, ending in `isolate it`. A thread in its project directory has the same chip and popup, and picking moves it right away; while the checkout is dirty the chevron is faint, the click does nothing and hovering shows why. One in a workspace shows `dirigent-xxxxxxxx` in blue, a menu above it: copy path, back to project (remove / keep on disk). Archiving a thread with a workspace opens a confirm under the archive button (remove at ~y 80, keep at ~y 106). Delete a workspace dir under `<session>/data/workspaces/` (named like its branch, `dirigent-xxxxxxxx`), or forget it from the project (`jj workspace forget dirigent-xxxxxxxx`, which leaves the files), and (re)open the thread: an amber banner with `Recreate` / `Back to project` sits above the composer within 10 s, and no error toasts. Pinned project rows are 32 px apart; `drag` one onto another row's y to reorder (the order survives `start`). The sidebar's drag handle is at
x = sidebar width; `[` hides the sidebar and hovering x < 6 peeks it. Clicking a thread's work line (`› model effort ·
duration · n tool calls`, under the user bubble) opens the 400 px right panel with its log under the lineage tree (when there is a family); its drag
handle is the hairline on its left; `]` / the header's panel button toggle the panel (opening it on the run in progress, else the last work) and Esc drops the log. The log is sticky: with it showing, opening another thread (a sub-agent from the tree, the parent again) shows that thread's latest work. The ruler is the 28 px strip at the pane's left edge: the marks are vertically centered at a 7 px pitch, so mark `i` of `n` is at y = list middle + 7·(i − (n − 1) / 2); `move` onto it for the preview, `click` to center that message.

**Sub-agents** (the `dirigent` tool, on by default; Settings › Config has its switch): prompt a thread to "use the dirigent tool to spawn a sub-agent named … with model anthropic/claude-haiku-5-5 whose task is …" (opus for the parent, haiku for children keeps the limits happy). The child is listed under its parent in the panel (below) with the name the parent gave it, never in the sidebar or lists; the parent's transcript gets a `↳ sub-agent "…" started` line under the work line as soon as it is spawned and a `↳ sub-agent "…" finished` line once it settles (both click → the child); a child's header shows `project › parent › <status glyph> title`. A thread idle with children working, or with an `async_bash` job still running, shows an hourglass (Waiting) in the Workpool and before its title in the header. While a thread with relations is open, the right panel (400 px, no title row) shows the **lineage** tree at its top, over the work log when one is open; it is how you reach sub-agents. It shows the thread's family as a tree (30 px rows, 16 px per level, model and run time on the right, the family's `in (n% cached) / out · $` line under it; click a row to open that thread; a parent's sub-agents in the order they were last given a task, so a child the parent `send`s a new turn moves last). `open` makes the desktop jump to a thread.

## Gotchas

- Popups snap inside the window. A tall menu can end up under the next click target, so take a shot after opening one.
- `DIRIGENT_DEBUG_KEYS=1 scripts/ui.sh start` logs every keystroke the app receives with its key-context stack to
  `log` (fastest way to debug a binding). `start` passes your environment through to the app.
- This runs gpui's **X11** backend with software Vulkan (lavapipe). Layout, input and behaviour match Wayland.
  Wayland-only details (IME, clipboard, window decorations) need a check on the real desktop by Seb. Xvfb has no
  compositor, so a translucent theme renders on black: leave that check to Seb too.
- Fractional scaling: `GPUI_X11_SCALE_FACTOR=1.25 scripts/ui.sh start 1375 900` (shot pixels are then device pixels).

## How it works (if you need to go off-script)

- `Xvfb -displayfd` picks a free display; the number is in `/tmp/dirigent-ui/<session>/display`. `ui.sh run` sets
  `DISPLAY` for you: `scripts/ui.sh run xdotool mousedown 1` (e.g. to shoot a drag halfway), `scripts/ui.sh run import …`.
- The app is launched with `VK_ICD_FILENAMES=$DIRIGENT_UI_VK_ICD` (lavapipe; Xvfb can't present GPU frames),
  `--data-dir <session>/data --config-dir <session>/config`, `DIRIGENT_PORT=<port>`, `DIRIGENT_BIND=127.0.0.1` (the
  embedded service stays off the tailnet), and `DIRIGENT_RUNTIME` (this checkout's runtime), in its own process group (`setsid`). The
  dev shell's `FONTCONFIG_FILE` adds Lilex, the default font, to the system fonts.
- The tool paths from `nix develop` are cached in `/tmp/dirigent-ui/devenv-<hash>.sh`, keyed by `flake.nix` +
  `flake.lock`, so calls take well under a second. `start` still builds through `nix develop` (unless you're in a dev
  shell already).
- `ui.sh rpc` is `scripts/ui-rpc.ts` (bun); `wait-idle` and `setup` are built on it.

## Non-UI crates

- `dirigent_runtime`: `bun run dirigent_runtime/src/index.ts`, feed JSONL on stdin, e.g.
  `{"type":"load","thread_id":"t1","cwd":"/tmp/x","entries":[]}` then `{"type":"prompt","thread_id":"t1","text":"hi","mode":"prompt"}`.
- `dirigent_service`: the standalone binary is for remote hosts; the desktop embeds the lib. `cargo run -p dirigent_service`, poke it with `websocat ws://localhost:7778/ws` (`nix run nixpkgs#websocat`):
  `{"type":"list_threads","id":1}`, `{"type":"add_project","id":2,"path":"/tmp/x"}`, `{"type":"create_thread","id":3,"project_id":"..."}`, `{"type":"prompt","id":4,"thread_id":"...","text":"hi","mode":"prompt"}`.
  Use `DIRIGENT_PORT=7778 cargo run -p dirigent_service -- --data-dir /tmp/somewhere --config-dir /tmp/somewhere-config`
  to keep a test instance away from the real one.
- Unit tests: `cargo test --workspace` (protocol round-trips, storage, view parsing). `bun run typecheck`, `bun run lint`, `bun run fmt:check` for the runtime.
