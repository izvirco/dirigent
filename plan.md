# Dirigent MVP plan

Dirigent is a desktop client for pi threads running on one or more hosts (local or over tailscale).
Each host runs a `dirigent_service` that owns its threads; the desktop talks to every host over WebSocket.

```
┌─────────────────────┐   ws (dirigent_protocol)   ┌─────────────────────┐   stdio jsonl   ┌──────────────────┐
│ dirigent_desktop    │ ◄────────────────────────► │ dirigent_service    │ ◄─────────────► │ dirigent_runtime │
│ gpui, cache sqlite  │        one per host        │ sqlite = truth      │   (bun child)   │ pi sdk, no state │
└─────────────────────┘                            └─────────────────────┘                 └──────────────────┘
```

Principles:

- Host is the source of truth for its threads. Desktop only caches.
- Runtime is stateless: it can be killed at any time; the service reloads sessions from sqlite on demand.
- Show cached data instantly, replace with fresh data when it arrives. Never block the UI on the network.
- No component library. Everything hand-rolled in gpui (markdown included: pulldown-cmark → divs and text runs).
- No auth. Service binds to localhost + tailscale interface only.

## Crates / packages

| name                | kind     | role                                                 |
| ------------------- | -------- | ---------------------------------------------------- |
| `dirigent_protocol` | rust lib | client ↔ service message types (serde JSON)          |
| `dirigent_service`  | rust bin | ws server, sqlite storage, runtime supervisor        |
| `dirigent_runtime`  | bun/ts   | wraps `@earendil-works/pi-coding-agent`, stdio JSONL |
| `dirigent_desktop`  | rust bin | gpui app, cache sqlite, ws client per host           |

Add `dirigent_protocol` to the workspace.

## 1. `dirigent_runtime` (TypeScript, bun)

One long-lived process per service. Holds any number of live `AgentSession`s keyed by `thread_id`, all in memory. Nothing is written to disk (`SessionManager.inMemory(cwd, opts, entries)`).

Messages in (one JSON object per line on stdin):

```ts
{ type: "load",   thread_id, cwd, entries: FileEntry[], model?, thinking_level? }   // header + entries from sqlite; model/level seed a fresh thread
{ type: "unload", thread_id }
{ type: "prompt", thread_id, text, mode: "prompt" | "steer" | "follow_up", images?: [{ mime_type, data /* base64 */ }] }
{ type: "abort",  thread_id }
```

Messages out (stdout):

```ts
{ type: "loaded",  thread_id, model, thinking_level, header }  // header = pi session header to persist
{ type: "entry",   thread_id, entry: SessionEntry }             // the persistence path (see below)
{ type: "event",   thread_id, event: AgentSessionEvent }        // raw pi event (message_update stripped of `partial`)
{ type: "warning", thread_id, message }
{ type: "error",   thread_id?, message }
```

Notes:

- `entry` messages are the persistence path: the service writes every one of them to sqlite. Pi persists messages into its `SessionManager` silently (`entry_appended` only fires for custom entries), so after every session event the runtime diffs `getEntryCount()` against what it has forwarded and emits the new tail. The runtime never needs to be asked "what's the state".
- `prompt` carries optional `model` (`provider/id`) and `thinking_level`; the runtime switches the session first if they differ. Pi persists the switch as entries, so it survives reloads. `load` carries the same for a thread with no entries yet, so a fresh session is created with that model and never persists pi's default one.
- `list_models` → `{ type: "models", models: [{ provider, id, name, context_window, thinking_levels }] }`: models this host has credentials for.
- Load = `createAgentSession({ cwd, sessionManager: SessionManager.inMemory(cwd, {}, entries) })` then `session.subscribe(...)`.
- A `load` for an already-loaded thread is a no-op. Service decides when to `unload` (idle timeout).
- Extension UI requests (`select`/`confirm`/`input`) are auto-cancelled in the MVP. TODO: forward to client.
- **Nix dev shell**: on `load`, if `<cwd>/flake.nix` exists and `nix` is on `PATH`, capture the dev shell environment once with `nix develop <cwd> --command env -0`, and inject it via the bash tool's `spawnHook` (`BashToolOptions.spawnHook: ctx => ({ ...ctx, env })`). Captured lazily on the first prompt, cached per live session. Failure to enter the shell is reported as a `warning` event, not an error — the thread still works with the plain environment.
- Logs go to stderr so stdout stays a clean protocol stream.
- Why stdio and not WebSocket here: both are a byte pipe with newline-framed JSON on top; neither is measurably faster for our message sizes. stdio wins on simplicity: no port, no handshake, no reconnect logic, and the runtime's lifetime is tied to the service (child dies with parent, EOF on stdin = shutdown). The overhead that matters — JSON serialization and the LLM round trip — is identical either way.
- Dev: `bun run dirigent_runtime/src/index.ts`. Later: `bun build --compile` and ship next to the service binary.

Deps: `@earendil-works/pi-coding-agent`. Keep `dirigent_runtime/src` tiny: `index.ts` (stdio loop), `sessions.ts` (map of live sessions).

## 2. `dirigent_protocol` (Rust)

Plain serde types, `#[serde(tag = "type", rename_all = "snake_case")]`. Requests carry an `id`; replies carry `reply_to`; pushes have neither.

```rust
// client → service
enum ClientMessage {
    ListProjects { id },
    AddProject   { id, path },                       // host validates the directory exists, reads git origin
    RemoveProject{ id, project_id },                 // soft delete; threads become orphans
    ListThreads  { id },
    GetThread    { id, thread_id },                  // full entries
    CreateThread { id, project_id },                 // cwd = the project's path
    Prompt       { id, thread_id, text, mode: PromptMode, model?, thinking_level?, images: Vec<ImageAttachment> },
    Abort        { id, thread_id },
    ArchiveThread{ id, thread_id, archived },        // hide from the inbox; still replayable
    DeleteThread { id, thread_id },                  // gone, entries included
    ListModels   { id },
    GetBlob      { id, blob_id },                   // image bytes behind an entry's `blob` ref
    ListUsage    { id, since, until, bucket_ms },   // spend per time slot and model, for the usage page
}

// service → client
enum ServerMessage {
    Hello        { host_name, version },
    Projects     { reply_to: Option<u64>, projects: Vec<Project> },      // reply or push on change
    ProjectAdded { reply_to, project },
    Threads      { reply_to: Option<u64>, threads: Vec<ThreadSummary> }, // reply or push on change
    Thread       { reply_to, thread_id, header: Value, entries: Vec<Entry> },
    ThreadCreated{ reply_to, thread: ThreadSummary },
    Ok           { reply_to },
    Error        { reply_to: Option<u64>, message },
    Entry        { thread_id, entry: Entry },   // pushed whenever an entry is persisted
    Event        { thread_id, event: Value },   // pi AgentSessionEvent, forwarded as JSON
    Models       { reply_to, models: Vec<ModelInfo> },
    Blob         { reply_to, blob_id, mime_type, data /* base64 */ },
    Usage        { reply_to, buckets: Vec<UsageBucket> },
}

struct Project       { id, path, origin: Option<String> /* normalized git origin */, created_at }
struct ThreadSummary { id, project_id: Option<String> /* None = orphan */, cwd, name, model, thinking_level, created_at, updated_at, entry_count, running_since: Option<i64> /* ms, while an agent works */, last_run_ms: Option<i64>, archived }
struct ModelInfo     { provider, id, name, context_window, thinking_levels }
struct Entry { seq: u64, id, parent_id: Option<String>, kind: String, json: Value }
struct UsageBucket { start, provider, model, input, output, cache_read, cache_write, cost: f64, messages }
```

Pi entries and events stay `serde_json::Value` on the wire. The desktop has a small `view` module that extracts what it renders (user text, assistant text/thinking/tool_call blocks, tool results, `text_delta`, `tool_execution_*`). Anything it doesn't understand is rendered as a one-line "unknown entry: <type>" placeholder. This keeps the protocol crate pi-version-agnostic.

No subscribe/unsubscribe for the MVP: the service pushes all events to all clients, clients drop what they aren't viewing.

Tests: one round-trip test per message enum (serialize → deserialize equals).

## 3. `dirigent_service` (Rust)

Deps: `tokio`, `axum` (ws), `rusqlite` (bundled), `serde`/`serde_json`, `dirigent_protocol`.

### Storage (`storage.rs`)

One sqlite file per host at `$XDG_DATA_HOME/dirigent/service.db` (WAL mode).

```sql
projects (id TEXT PK, path TEXT, origin TEXT, created_at INTEGER, deleted_at INTEGER)
threads  (id TEXT PK, project_id TEXT, cwd TEXT, name TEXT, header TEXT, model TEXT, thinking_level TEXT, created_at INTEGER, updated_at INTEGER, last_run_ms INTEGER, archived_at INTEGER)
entries  (thread_id TEXT, seq INTEGER, id TEXT, parent_id TEXT, kind TEXT, json TEXT, PRIMARY KEY (thread_id, seq))
blobs    (id TEXT PK /* sha256 */, mime TEXT, bytes BLOB)
usage    (at, provider, model, input, output, cache_read, cache_write, cost)   -- one row per assistant message
```

`seq` is the append order; replay to the runtime is `SELECT json ORDER BY seq`. **Images** (pasted into prompts, or in tool results) never sit in `entries.json`: `append_entry` moves every `{type:"image", data, mimeType}` block's bytes into `blobs` (content-addressed, so duplicates share a row) and stores `blob: <id>` in its place; that stripped form is what clients get. Replaying into the runtime inflates them back (`blobs.rs`). Clients fetch bytes lazily with `GetBlob` and cache them. Prompts arrive with images inline; pi may resize them, so only the entry pi writes back feeds `blobs`. `threads.model` / `thinking_level` are kept current from `model_change` / `thinking_level_change` entries. **Usage** is derived: every assistant message's `usage` (tokens, pi's cost) becomes a `usage` row at append time, by the same `json_extract` select that backfills the table once when it is empty on open, so spend history survives the table being new. Rows outlive their thread. `ListUsage` is one `GROUP BY` over it. That's the whole storage story. No migrations — one `CREATE TABLE IF NOT EXISTS` block; when the schema changes, delete the db files.

### Projects

A project is a directory on a host; the user clones it there and adds the absolute path. The host validates the path and records the normalized git `origin` (if any). Removing a project is a soft delete: its threads stay and are shown as orphans. No git management, ever.

Test: insert thread + entries, read back in order.

### Runtime supervisor (`runtime.rs`)

- Spawns `bun run <runtime path>` (path from `DIRIGENT_RUNTIME` env / config). Reads stdout lines, writes stdin lines.
- Tracks `loaded: HashSet<thread_id>`. On `Prompt` for an unloaded thread: load entries from sqlite → send `load` → wait for `loaded` → send `prompt`.
- A thread is `running` from the moment a `Prompt` (mode `prompt`) arrives — the service also writes the prompt's model / thinking level to `threads` right then and pushes `Threads` — until pi's `agent_settled`. Loading a session (and capturing a nix shell) can take seconds; clients see the thread working with its final model the whole time. A prompt that fails before pi starts settles it again.
- Every `entry_appended` event → insert into `entries`, bump `threads.updated_at`, push `Threads` update to clients.
- Runtime exits/crashes → clear `loaded`, respawn, log. In-flight prompts get an `Error`. This is the whole point of a stateless runtime.
- Idle unload: thread with no activity for 30 minutes → `unload`.
- Thread `name`: from pi's `session_info` entry, else the first line of the first user message.

### WebSocket server (`server.rs`)

- `axum` on port 7777 (`DIRIGENT_PORT`), bound to `127.0.0.1` plus every address `tailscale ip` reports — never a wildcard, there is no auth. `DIRIGENT_BIND=ip,ip` overrides the list. Route `/ws`.
- Each connection: send `Hello`, then a task that handles requests and a broadcast receiver for pushes.
- Pushes come through one `tokio::sync::broadcast<ServerMessage>`.

Manual test: `websocat ws://localhost:7777/ws`, send `{"type":"list_threads","id":1}`.

## 4. `dirigent_desktop` (Rust, gpui)

Deps: `gpui` + `gpui_platform` (git, zed main; `gpui_platform` needs `features = ["wayland", "x11"]` or it runs headless), `tokio` + `tokio-tungstenite`, `rusqlite`, `dirigent_protocol`.

Status: implemented; `scripts/ui.sh` + `testing.md` cover headless launch/screenshot/input (Xvfb).

### Startup

1. Open cache db at `$XDG_DATA_HOME/dirigent/desktop.db` and `hosts.toml`. Load hosts + cached threads. Render immediately.
2. For the `local` host: try to connect to `ws://127.0.0.1:7777/ws` (`DIRIGENT_PORT`); if refused, spawn `dirigent_service` _detached_ (look next to own exe, then `PATH`) and retry with backoff. The service outlives the desktop on purpose: it is the source of truth and keeps agents running. Respawn is rate-limited to once per 10s.
3. For every host: start a connection task. On connect → `ListThreads` → upsert cache → re-render.

Hosts: `local` (implicit, the embedded service) plus `$XDG_CONFIG_HOME/dirigent/hosts.toml` (`hosts.rs`):

```toml
[hosts]
hetzner = "hetzner.tail1234.ts.net"   # host[:port] or ws://host:port/ws; port defaults to 7777
```

The file is the truth. The Hosts section of the Settings page (gear at the sidebar's bottom right) lists hosts with
their status, adds one (name + address form) and removes one (hover ✕); both rewrite the file and then `sync_hosts` diffs it against
the running connections — the same path that picks up hand edits when the page is opened. Removing a host
(or changing its url) drops its connection and everything cached for it.

### Cache db

```sql
projects (host, id, path, origin, created_at, PRIMARY KEY (host, id))
links    (host, project_id, group_key)             -- manual cross-host links (desktop-owned)
threads  (host, id, project_id, cwd, name, model, thinking_level, created_at, updated_at, entry_count, last_run_ms, archived, PRIMARY KEY (host, id))
entries  (host, thread_id, seq, json, PRIMARY KEY (host, thread_id, seq))
models   (host PK, json)                           -- cached model list so the picker works offline
blobs    (id PK, mime, bytes)                      -- image bytes by content hash, host-independent
seen     (host, thread_id, updated_at)             -- the thread's updated_at when the user last opened it (unread tracking)
snoozed  (host, thread_id, updated_at)             -- the thread's updated_at when the user snoozed it (hidden from the sidebar until it runs again)
pinned   (host, thread_id)                         -- threads kept on top of their status group
pins     (host, thread_id, item)                   -- pinned messages, by timeline index
kv       (key PK, value)                           -- last used model / effort, panel widths, last pane
```

Same shape as the service db plus a `host` column. Writes happen off the UI thread.

### Project groups (`projects.rs`)

The sidebar groups projects across hosts: by normalized git origin automatically, or by a manual "link to…" (stored in `links`; `unlink` clears it). A group with members on several hosts asks which host when creating a thread. Threads whose project was removed appear under "orphaned".

### State

One `App` entity holding:

- `hosts: Vec<HostState>` — `{ name, url, status: Disconnected | Connecting | Connected, threads: Vec<ThreadSummary> }`
- `pane: Inbox | Workpool | Project { group_key } | NewProject { host } | Settings | Usage | Draft { target } | Thread(OpenThread)`
- `usage: usage::State`: the usage page's window, grouping, hidden series and each host's buckets
- `OpenThread` — entries + the `Timeline` folded from them + `streaming` (the in-flight message, running tools, echoed prompt)
- `pending: Option<(req_id, text)>` — a sent draft waiting for `ThreadCreated` before it can `Prompt`
- `sidebar_width` / `sidebar_hidden` / `sidebar_peek` — drag-resized width (persisted in `kv`), `[` toggles, hover on the left edge peeks

The pane is persisted in `kv` on every navigation (inbox / workpool / project / thread; drafts and the
new-project form are transient and skipped) and restored at startup, so the app reopens where it was left.
A restored thread renders from cache; its `GetThread` is sent when the host connects, since requests made
while offline are dropped.

A thread's status is derived, never stored: `running_since` set → Working, `archived` → Archived, else Done
(`Dirigent::status` reports a snoozed Done thread as Snoozed).
Inbox = Done threads across hosts, Workpool = Working ones, a project page = all of its threads with
archived ones in a trailing section. Sidebar, Inbox, Workpool and project rows are grouped by status (`sort_by_status`: working,
done, snoozed, archived), _pinned_ threads first in each group (the header's pin button, amber while pinned; an amber pin
before the name marks them everywhere), newest first otherwise. Inbox and Workpool are fixed queues, newest on top: the Workpool is
ordered by `running_since`, the Inbox by `updated_at` (= when the run finished, since only entries bump it),
so a thread moves only when it enters the list. Every on-screen clock is driven by one permanent 1s
`tick`; running spans show whole seconds (`elapsed`), finished ones `34ms` / `0.34s` / `12s` (`duration`). A Done thread is _unread_ (accent title) until the user opens it:
`seen` remembers each thread's `updated_at` as of the last look, and the open thread is marked seen on
every `Threads` push, so a run that finishes while you watch is not unread. The sidebar lists the whole
Inbox so threads are a click apart; _snooze_ (the moon on a card, a list row or the thread header) takes a
thread out of the sidebar without archiving it — the Inbox page still lists it, with a moon for its glyph:
`snoozed` remembers its `updated_at`, so the next run brings it back by itself, and the moon button (amber
while snoozed) wakes it by hand.

Network tasks push `ServerMessage`s into an mpsc channel; a gpui background task drains it and calls `cx.update(|app| ...)` + `notify()`.

### UI (all hand-rolled)

Linear-like: the window canvas (darkest) carries the sidebar; the main pane is a rounded, bordered panel
inset 8px from it. One indigo accent for live things (running, send, focus), IBM Plex Sans for UI and prose,
IBM Plex Mono only where the content is code (tool calls, results, model ids). Status has one glyph per
state (still accent dot / check / amber moon / archive box) reused everywhere; no spinners.

- `theme.rs` — palette, fonts, `Status`, and the shared widgets (icon button, button, chip, labeled chip, empty state).
- `sidebar.rs` — title row with the compose button, Inbox / Workpool, each listing its threads (finished and not snoozed / working) as cards (`project, host … run time` / title / model, the run time live while working and swapped for the snooze button on hover; a chevron on the nav row collapses them), project list (`+` on the section header adds one; `⋯` on a row: link/unlink, remove), removed-projects row, and a bottom bar with the hide/show button and the settings gear. `docked()` adds a drag handle on its right edge; `peek()` is the hidden state: a 10px hot zone on the window's left edge that grows into a floating copy while hovered.
- `lists.rs` — the 40px header bar (breadcrumb + actions), the three list pages, the new-project form and the settings pages' header with its tabs (Hosts here: a narrow centered column; Usage in `usage.rs`). Rows: glyph, title, project, model, age; hover reveals snooze and archive.
- `usage.rs` — Settings › Usage: what the agents cost over time, across hosts. One big chart of cost: a monotone-cubic smoothed line per series with a faint gradient under it, quarter gridlines labelled in dollars, time ticks aligned to local hours / midnights. The headline is the visible series' total for the window. Window: `1d / 7d / 30d` ending now (refreshed every 30s while open), `‹ ›` pages it, `custom` reveals two date fields. Grouping chip: by model / by provider / everything. Hovering the chart draws a crosshair with a dot per line and a card beside the cursor (flipped to the cursor's other side on the right half) with every line's cost in that slot and the split behind it: `1.7M in · 15k out · 98% cached`. The legend under the chart is a table of per-series totals (cost, in, out, cached %, messages); clicking a row hides its line (hollow swatch) and the headline follows. Slot width follows the window (≤ 60 points); every host gets the same `ListUsage` and `Model` folds the replies.
- `menu.rs` — the one popup (`Dirigent::menu`): model, effort, draft target, project actions, link target, host picker, delete confirm. Anchored to its trigger with `deferred(anchored())`.
- `thread.rs` — header (breadcrumb, status, snooze, pin, archive, delete) + `list()` of timeline items in a centered 720px reading column + the composer card. User messages are raised, replies are markdown prose, and each turn's _work_ is one line: `› model  effort  duration  n tool calls`, plus what it is doing right now while it runs (`· bash 3.2s`, `thinking`, `writing`, re-rendered every second). The transcript never shows the steps: clicking a work line (or `]`) opens `log_panel()`, a resizable panel split off the pane's right by a hairline, with the full log as a lazy `list()` of steps (`render_step`: thinking, interim prose, tool rows with elapsed time and the call's `timeout`; tool rows expand to all arguments and the output). On a live work the panel follows the latest step unless the user scrolled up (`FollowMode::Tail`), and an open panel follows the next prompt's run. `]` or Esc (when nothing runs) closes it; `]` with it closed opens the run in progress, else the last work. `draft_page()` is the same composer alone, centered in the pane, with a `Create thread in <project>` chip above it.
- `ruler.rs` — the ruler over the transcript's left edge: one tick per message (user blue, reply gray, pinned amber, compaction dotted), all the same length, vertically centered at a 7px pitch (squeezed once they'd overflow), the ones in view shorter and brighter. Hover previews the message, click scrolls it to the middle of the viewport (`Dirigent::jump_to` + `settle_jump`). Painted by one `canvas`, which also steps the tweens (marks grow in, the in-view state follows the scroll) and asks for the next frame only until they settle. Pins (`OpenThread::pins`, the `pin` button next to `copy`, an amber bar in the margin) are timeline indices stored in the cache's `pins` table.
- `markdown.rs` — `render(text, mono, keys)`: pulldown-cmark events → block `div`s (paragraph, heading, list, code block, blockquote, table, rule) and inline highlight runs on a `StyledText` (bold, italic, strikethrough, code in mono, links coloured but not clickable), each paragraph a selectable `select::Block`. Inherits size/colour/font from the caller, so it serves replies, user bubbles, and the mono work log alike. `committed(text)` is the prefix up to the last newline.
- `select.rs` — mouse selection over the transcript and the work log (two `Doc`s; a selection never spans both). Every paragraph, code block, tool argument / output, steer, error line is a `Block`: a `StyledText` keyed by `(doc, item, seq)` in document order, so a selection is two `(key, byte offset)` positions and blocks in between are selected whole. Blocks register their layout when painted (hit testing, highlight) and their text when constructed; the highlight (`paint_selection`, shared with the composer) is one lyon-filled shape per block, its rows only as wide as their selected text, with every corner rounded — including where a row steps in or out against its neighbours; copying (`ctrl+c`, and the primary selection on mouse up) constructs the selected items again, layout-free, to read off-screen blocks too. Click, double click (word), triple click (block), shift+click, drag with autoscroll at the document's edge; Esc or any other press clears. A press on a block keeps the composer focused. Hovering a user message or reply reveals `pin` and `copy` buttons in its corner (`copy` copies the raw markdown; `pin` see `ruler.rs`).
- `input.rs` — multiline text input. gpui has no text input widget; implements `EntityInputHandler` for IME + key handling (see zed's `crates/gpui/examples/input.rs`). Ctrl+Enter submits, shift+Enter submits with `queued` set, Enter inserts a newline (single-line inputs submit on Enter). Grows with its text up to 70% of the window, then scrolls with a thin bar; the cursor is kept in view. Double click selects a word, triple everything; a drag keeps following the mouse after it leaves the input (above = to the start, below = to the end). Undo / redo (`ctrl+z`, `ctrl+shift+z` / `ctrl+y`): typing a word is one step, so is a run of deletes; paste, newline and IME commits are steps of their own. `ctrl+c` with nothing selected hands the action on to the root, which copies the transcript selection. Also used single-line for the new-project path. Pasting an image emits `ImagePasted` instead of text; `[image-N]` tokens are shaped in the accent color.
- Composer: a draft (`ctrl+n`, the compose button, or a project page's button) opens the draft page; the first ctrl+Enter sends `CreateThread`, and `Prompt` follows once `ThreadCreated` arrives (`pending`); the created thread is listed as already working with the picked model, so it never flashes through the inbox. Navigating away keeps the text. Pasted images become `[image-N]` tokens in the text plus a thumbnail strip under it (click to preview full-size, hover ✕ to remove); the text is the truth — only images whose token is still present are sent (`attachments()`), so deleting the token drops the image. Chips: model, effort, context usage (hover it for the thread's totals so far: tokens in / out, cached share, message and compaction counts, cost; `thread::stats_card` over `view::Stats`). While running: ctrl+Enter steers, shift+Enter queues a follow-up, Esc / the stop button aborts. Model list is the host's cached `models`; the thread's own model wins when opening a thread, otherwise the last one used.
- `main.rs` — window, font via fontconfig (Lilex everywhere; `DIRIGENT_FONT` overrides), keybindings (`ctrl+n` new thread, `[` toggle sidebar, `]` toggle work log — types a bracket instead when the focused input already has text, `ctrl+q` quit).
- `view.rs` — pi entry/event JSON → `Timeline` of `Item`s (`User` with its image blob ids, rendered as thumbnails once `Dirigent::blobs` has them, `Work { model, effort, started, ended, steps }`, `Assistant`, `Error`, `Compaction`, `Notice`), folded entry by entry; tool results and timings (`durationMs`, timestamps) land in their `ToolCall` step. A user message that arrives while a work is open is a steer and becomes a `Step::Steer` of that work (rendered as a `steer` row under the work line, kept after the run), so one run stays one `Work`. `Streaming` holds the in-flight message's steps, the tools executing right now (with start times), and the optimistic echoes of sent messages with their `PromptMode`, oldest first (prompts show above the live work, steers inside it marked `queued`, follow-ups below it). pi drains steers one per turn after that turn's tool calls and persists nothing before, so an echo lives until its user entry lands (`take_echo`) or the run settles.

The thread list is `timeline.items.len() + 1` items: the last one is the live tail (the echoed prompt, the work in progress, and the reply in progress, rendered there instead of in their own slots so streamed steps join the persisted ones). New items `splice` in before it; deltas only `remeasure` it.

Streamed text shows a line at a time, not a word at a time: the view renders `markdown::committed()` of the buffer, and a delta only counts as a change (`Streaming::apply` → true) when it contains a newline. Since models emit one line per block, paragraphs and list items appear whole and nothing inline ever flickers. The model's latest prose (`Work::latest_text`, over the persisted and streamed steps) renders below the work line in reply style; the next prose, or the final reply once the turn ends, replaces it. So the reading column only ever grows while a run goes on.

Fast-UX rules: thread switch renders cached entries synchronously then requests fresh; deltas mutate the streaming buffer and `notify()` — no reparsing of the whole thread per token.

### Platform

- Linux/Wayland only for the MVP. Dev shell libs are in `flake.nix` (`gpuiLibs`).
- `TODO(macos)`: service spawn path (app bundle `Contents/MacOS`), code signing.
- `TODO(windows)`: service spawn path, `XDG_DATA_HOME` → `LOCALAPPDATA`, console window hiding.

## Phases

1. **Scaffold** — add `dirigent_protocol`, deps, `bun add` pi sdk. `cargo build` + `bun run typecheck` green. (desktop crate + flake libs already done)
2. **Runtime** — stdio loop, load/prompt/abort/events. Verify with `echo '{"type":"load",...}' | bun run ...`.
3. **Protocol** — types + round-trip tests.
4. **Service** — storage, supervisor, ws. Verify with websocat: create thread, prompt, watch events, restart service, thread still there.
5. **Desktop** — connect + cache + sidebar + thread view + composer, local service spawn. First real use.
6. **Hardening** — reconnect with backoff, runtime respawn, idle unload, error toasts.

All six phases are implemented.

Out of scope for now: clickable links, forking/branch navigation, extension UI dialogs, images, auth, multi-window, editing a host in place (remove and re-add, or edit `hosts.toml`).
