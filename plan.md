# Dirigent MVP plan

Dirigent is a desktop client for pi threads running on one or more hosts (local or over tailscale).
Each host runs a `dirigent_service` that owns its threads; the desktop talks to every host over WebSocket.

```
âââââââââââââââââââââââ   ws (dirigent_protocol)   âââââââââââââââââââââââ   stdio jsonl   ââââââââââââââââââââ
â dirigent_desktop    â ââââââââââââââââââââââââââº â dirigent_service    â âââââââââââââââº â dirigent_runtime â
â gpui, cache sqlite  â        one per host        â sqlite = truth      â   (bun child)   â pi sdk, no state â
âââââââââââââââââââââââ                            âââââââââââââââââââââââ                 ââââââââââââââââââââ
```

Principles:

- Host is the source of truth for its threads. Desktop only caches.
- Runtime is stateless: it can be killed at any time; the service reloads sessions from sqlite on demand.
- Show cached data instantly, replace with fresh data when it arrives. Never block the UI on the network.
- No component library. Everything hand-rolled in gpui (markdown included: pulldown-cmark â divs and text runs).
- No auth. Service binds to localhost + tailscale interface only.

## Crates / packages

| name                | kind     | role                                                 |
| ------------------- | -------- | ---------------------------------------------------- |
| `dirigent_protocol` | rust lib | client â service message types (serde JSON)          |
| `dirigent_service`  | rust bin | ws server, sqlite storage, runtime supervisor        |
| `dirigent_runtime`  | bun/ts   | wraps `@earendil-works/pi-coding-agent`, stdio JSONL |
| `dirigent_desktop`  | rust bin | gpui app, cache sqlite, ws client per host           |

Add `dirigent_protocol` to the workspace.

## 1. `dirigent_runtime` (TypeScript, bun)

One long-lived process per service. Holds any number of live `AgentSession`s keyed by `thread_id`, all in memory. Nothing is written to disk (`SessionManager.inMemory(cwd, opts, entries)`).

Messages in (one JSON object per line on stdin):

```ts
{ type: "load",   thread_id, cwd, entries: FileEntry[], model?, thinking_level?, tools? }   // header + entries from sqlite; model/level seed a fresh thread
{ type: "unload", thread_id }
{ type: "prompt", thread_id, text, mode: "prompt" | "steer" | "follow_up", images?: [{ mime_type, data /* base64 */ }], title?: TitleGen, tools? }
{ type: "answer", thread_id, ask: Ask, answers: string[][] | null }   // the user's answers to an `ask_user` call (null = dismissed)
{ type: "notify", thread_id, custom_type, content, details, notify }  // a notice from the service (a sub-agent ended), queued like a job's
{ type: "abort",  thread_id }
{ type: "generate_title", title: TitleGen, text }   // TitleGen = { model, reasoning, prompt }
```

Messages out (stdout):

```ts
{ type: "loaded",  thread_id, model, thinking_level, header }  // header = pi session header to persist
{ type: "entry",   thread_id, entry: SessionEntry }             // the persistence path (see below)
{ type: "event",   thread_id, event: AgentSessionEvent }        // raw pi event (message_update stripped of `partial`)
{ type: "ask",     thread_id, ask: Ask }                        // the model called `ask_user`; Ask = { id, questions, notify }
{ type: "jobs",    thread_id, running }                         // the thread's count of running `async_bash` jobs changed
{ type: "warning", thread_id, message }
{ type: "error",   thread_id?, message }
```

Notes:

- `entry` messages are the persistence path: the service writes every one of them to sqlite. Pi persists messages into its `SessionManager` silently (`entry_appended` only fires for custom entries), so after every session event the runtime diffs `getEntryCount()` against what it has forwarded and emits the new tail. The runtime never needs to be asked "what's the state".
- `prompt` carries optional `model` (`provider/id`) and `thinking_level`; the runtime switches the session first if they differ. Pi persists the switch as entries, so it survives reloads. `load` carries the same for a thread with no entries yet, so a fresh session is created with that model and never persists pi's default one.
- `list_models` â `{ type: "models", models: [{ provider, id, name, context_window, thinking_levels }] }`: models this host has credentials for.
- **Titles**: a `prompt` with `title` (the desktop sends it with a thread's first prompt, from its `config.toml`) runs one `completeSimple` alongside the agent run â `title.prompt` as the system prompt, the message as the user turn, `title.model` if this host has credentials for it (else haiku 5.5, else gpt 6 luna, else the thread's own model) â and calls `session.setSessionName()`. Pi persists that as a `session_info` entry, so the name reaches the service and every client through the normal entry path; a failure is a `warning`. `generate_title` is the same completion on its own, answered with `{ type: "title", title, model, input, output, cost, duration_ms }` (the settings page's `Test`).
- Load = `createAgentSession({ cwd, sessionManager: SessionManager.inMemory(cwd, {}, entries) })` then `session.subscribe(...)`.
- A `load` for an already-loaded thread is a no-op. Service decides when to `unload` (idle timeout).
- **Dirigent tools** (`tools.ts`): three tools of our own on top of pi's, registered on every session; `load.tools` / `prompt.tools` (`["async_bash", "ask_user", "dirigent"]`, from the desktop's `[tools]` config) say which are active (`setActiveToolsByName`). All return at once and report later through `session.sendCustomMessage` (`customType` `dirigent:job` / `dirigent:answer` / `dirigent:child`, `triggerTurn`), queued as a steer (`notify: "asap"`, the default) or a follow-up (`"when_idle"`); an idle agent gets a new turn. The custom message is persisted as a `custom_message` entry like any other, so clients see it through the entry path. Notices pi hasn't delivered yet are remembered and queued again after `cancel_queued` (which clears pi's queues wholesale).
  - `async_bash { action: start|status|kill, command?, job_id?, notify? }`: `start` spawns `bash -c` in its own process group (nix env included), stdout+stderr to `$XDG_DATA_HOME/dirigent/jobs/<thread>/<job>.log`, and returns the job id and log path; a command that ends within 2s is reported inline instead. When the job ends the model gets exit code, duration, log path and the last 30 lines. Jobs die with the session (`unload`) and the log dir with the thread; a job the model `kill`ed sends no notice. Whenever a thread's count of running background jobs changes (one outlives the 2s, ends, is killed, the session unloads) the runtime sends `jobs`; the service keeps the counts in memory (they die with the runtime) and puts them on `ThreadSummary.jobs`, so the desktop can show a thread whose turn ended with jobs running as Waiting.
  - `ask_user { questions: [{ text, options?, allow_custom?, multi? }] }`: emits `ask` (the service persists it in `asks` and lists it on the thread) and returns at once. `answer` delivers the answers as the user's message, always as a steer: they are user input, so they go at the earliest point pi allows (like a typed steer).
  - `dirigent { action: spawn|send|read|status|search|archive|snooze|abort|open, ... }`: the orchestrator. It is a **client of the service it runs under** (`client.ts`: a ws `Client` on `DIRIGENT_URL`, which the service sets on the runtime child, plus `Dirigent`, the helpers on top of it), so it needs no runtime→service channel of its own. `spawn { name, task, model?, effort?, workspace?, base? }` is `CreateThread { parent: me, name }` + `Prompt` (the task, then a `---` note saying whose sub-agent it is; the model and effort default to the caller's; the caller names the thread, so nothing generates a title for it), so **a sub-agent is just a thread with a `parent_id`**: peeking, lineage, archive, workspaces, the inbox all come for free. `send` prompts a thread (a new turn if idle, else a steer / follow-up), `read` returns its last final reply (or the whole conversation), `search` is the host's full-text search, `open` asks every desktop to show the thread (`FocusThread`). Nothing waits: when a sub-agent ends, the **service** (not the runtime, which may be restarted meanwhile) sends the parent a `notify` (`dirigent:child` with the child's reply, or that it is waiting on the user), which starts a turn if the parent is idle.
- Pi's own extension UI requests (`select`/`confirm`/`input`) are still auto-cancelled; our tools don't use them.
- **Nix dev shell**: on `load`, if `<cwd>/flake.nix` exists and `nix` is on `PATH`, capture the dev shell environment once with `nix develop <cwd> --command env -0`, and inject it via the bash tool's `spawnHook` (`BashToolOptions.spawnHook: ctx => ({ ...ctx, env })`). Captured lazily on the first prompt, cached per live session. Failure to enter the shell is reported as a `warning` event, not an error â the thread still works with the plain environment.
- Logs go to stderr so stdout stays a clean protocol stream.
- Why stdio and not WebSocket here: both are a byte pipe with newline-framed JSON on top; neither is measurably faster for our message sizes. stdio wins on simplicity: no port, no handshake, no reconnect logic, and the runtime's lifetime is tied to the service (child dies with parent, EOF on stdin = shutdown). The overhead that matters â JSON serialization and the LLM round trip â is identical either way.
- Dev: `bun run dirigent_runtime/src/index.ts`. Later: `bun build --compile` and ship next to the service binary.

Deps: `@earendil-works/pi-coding-agent`. Keep `dirigent_runtime/src` tiny: `index.ts` (stdio loop), `sessions.ts` (map of live sessions).

## 2. `dirigent_protocol` (Rust)

Plain serde types, `#[serde(tag = "type", rename_all = "snake_case")]`. Requests carry an `id`; replies carry `reply_to`; pushes have neither.

```rust
// client â service
enum ClientMessage {
    ListProjects { id },
    AddProject   { id, path },                       // host validates the directory exists, reads git origin
    RemoveProject{ id, project_id },                 // soft delete; threads become orphans
    ListThreads  { id },
    GetThread    { id, thread_id },                  // full entries
    CreateThread { id, project_id, workspace, base, parent?, name? },  // cwd = the project's path, or a fresh workspace of it at `base`
    MoveToWorkspace { id, thread_id, base },         // into its own worktree / jj workspace (also recreates a missing one)
    RemoveWorkspace { id, thread_id, detach },       // back to the project dir; `detach` leaves the workspace on disk
    VcsStatus    { id, path },                       // is it a repo, is it dirty, what branch
    Prompt       { id, thread_id, text, mode: PromptMode, model?, thinking_level?, images: Vec<ImageAttachment>, title?: TitleGen, tools?: Vec<String> },
    Answer       { id, thread_id, ask_id, answers: Option<Vec<Vec<String>>> },   // one list per question (options + typed text); None = dismissed
    GenerateTitle{ id, title: TitleGen, text },      // name `text` as a first prompt would be; Settings âº Config's Test
    Abort        { id, thread_id },
    ArchiveThread{ id, thread_id, archived },        // hide from the inbox; still replayable
    SnoozeThread { id, thread_id, snoozed },         // out of the sidebar until the thread's next run
    FocusThread  { id, thread_id },                  // ask every client to open the thread (an agent's `open`)
    SearchThreads{ id, query, limit },               // full-text over messages; best hit per thread
    DeleteThread { id, thread_id },                  // gone, entries included
    ListModels   { id },
    GetBlob      { id, blob_id },                   // image bytes behind an entry's `blob` ref
    ListUsage    { id, since, until, bucket_ms },   // spend per time slot and model, for the usage page
}

// service â client
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
    Title        { reply_to, title, model, input, output, cost, duration_ms },
    VcsStatus    { reply_to, path, vcs: Option<Vcs>, dirty, branch: Option<String>, refs: Vec<String> },
    Focus        { thread_id },                     // pushed on `FocusThread`
    Search       { reply_to, hits: Vec<SearchHit> },
}

struct TitleGen { model /* provider/id */, reasoning /* pi thinking level */, prompt /* system prompt */ }

struct Project       { id, path, origin: Option<String> /* normalized git origin */, created_at }
struct ThreadSummary { id, project_id: Option<String> /* None = orphan */, cwd, name, model, thinking_level, created_at, updated_at, entry_count, running_since: Option<i64> /* ms, while an agent works */, last_run_ms: Option<i64>, archived, asks: Vec<Ask> /* open ask_user calls */, workspace: Option<String>, workspace_missing, parent_id: Option<String> /* the thread that spawned it */, snoozed, prompted_at: Option<i64> /* the last Prompt that started a turn */, jobs: u32 /* running async_bash jobs, runtime state */ }
struct SearchHit     { thread_id, snippet }
enum Vcs { Git, Jj }
struct Ask { id, questions: Vec<Question> }   // Question { text, options, allow_custom, multi }
struct ModelInfo     { provider, id, name, context_window, thinking_levels }
struct Entry { seq: u64, id, parent_id: Option<String>, kind: String, json: Value }
struct UsageBucket { start, provider, model, input, output, cache_read, cache_write, cost: f64, messages }
```

Pi entries and events stay `serde_json::Value` on the wire. The desktop has a small `view` module that extracts what it renders (user text, assistant text/thinking/tool_call blocks, tool results, `text_delta`, `tool_execution_*`). Anything it doesn't understand is rendered as a one-line "unknown entry: <type>" placeholder. This keeps the protocol crate pi-version-agnostic.

No subscribe/unsubscribe for the MVP: the service pushes all events to all clients, clients drop what they aren't viewing.

`CreateThread { parent }` makes a sub-agent (it starts in the parent's cwd unless `workspace`, inherits the parent's tools, and may nest at most 3 deep). `name` names it from the start: sub-agents are named by whoever spawns them, never titled from their prompt.

Fields added after a host or client shipped are `#[serde(default)]` (`Prompt.tools`, `ThreadSummary.asks`, `CreateThread.workspace`, `CreateThread.name`, `ThreadSummary.parent_id`, `ThreadSummary.prompted_at`, `ThreadSummary.jobs`), so a desktop and a service of different versions keep talking.

Tests: one round-trip test per message enum (serialize â deserialize equals), plus one that old-shaped messages still parse.

## 3. `dirigent_service` (Rust)

Deps: `tokio`, `axum` (ws), `rusqlite` (bundled), `serde`/`serde_json`, `dirigent_protocol`.

### Storage (`storage.rs`)

One sqlite file per host at `$XDG_DATA_HOME/dirigent/service.db` (WAL mode).

```sql
projects (id TEXT PK, path TEXT, origin TEXT, created_at INTEGER, deleted_at INTEGER)
threads  (id TEXT PK, project_id TEXT, cwd TEXT, name TEXT, header TEXT, model TEXT, thinking_level TEXT, created_at INTEGER, updated_at INTEGER, last_run_ms INTEGER, archived_at INTEGER, tools TEXT /* json list, from the last Prompt */, workspace TEXT /* the thread's isolated dir, while attached */, parent_id TEXT, snoozed_at INTEGER /* updated_at as of the snooze; snoozed while updated_at hasn't moved past it */, prompted_at INTEGER /* the last Prompt that started a turn */)
entries  (thread_id TEXT, seq INTEGER, id TEXT, parent_id TEXT, kind TEXT, json TEXT, PRIMARY KEY (thread_id, seq))
entries_fts (thread_id UNINDEXED, text)  -- fts5 over user / assistant text, derived like `usage` (backfilled once when empty)
blobs    (id TEXT PK /* sha256 */, mime TEXT, bytes BLOB)
usage    (at, provider, model, input, output, cache_read, cache_write, cost)   -- one row per assistant message
asks     (thread_id, id, json, created_at)                                     -- open ask_user calls; a row leaves with its Answer
```

`seq` is the append order; replay to the runtime is `SELECT json ORDER BY seq`. **Images** (pasted into prompts, or in tool results) never sit in `entries.json`: `append_entry` moves every `{type:"image", data, mimeType}` block's bytes into `blobs` (content-addressed, so duplicates share a row) and stores `blob: <id>` in its place; that stripped form is what clients get. Replaying into the runtime inflates them back (`blobs.rs`). Clients fetch bytes lazily with `GetBlob` and cache them. Prompts arrive with images inline; pi may resize them, so only the entry pi writes back feeds `blobs`. A reply can also _show_ a local image: the model writes `![caption](/path/on/host.png)` (the runtime appends a note saying so to pi's system prompt) and `append_entry` reads the file into `blobs`, on the host where it lives, rewriting the link to `blob:<id>` (`blobs::link_images`; relative paths from the thread's cwd, png / jpg / gif / webp / bmp up to 16 MiB, anything else left as written). `threads.model` / `thinking_level` are kept current from `model_change` / `thinking_level_change` entries. **Usage** is derived: every assistant message's `usage` (tokens, pi's cost) becomes a `usage` row at append time, by the same `json_extract` select that backfills the table once when it is empty on open, so spend history survives the table being new. Rows outlive their thread. `ListUsage` is one `GROUP BY` over it. **Asks**: the runtime's `ask` message is stored until the `Answer` that takes it (`take_ask`, so a second client answering the same ask is refused); `list_threads` attaches the open ones to their thread, and an `Answer` loads the thread if it isn't live (with `threads.tools`, remembered from its last prompt) and hands the answers to the runtime. **Upgrades**: an existing db must keep working, so schema changes are additive and self-applying: `CREATE TABLE IF NOT EXISTS` for tables, `ensure_column` (a guarded `ALTER TABLE ADD COLUMN`, listed in `COLUMNS`) for columns, and nothing is ever renamed or dropped (older dbs still have the removed workflow feature's `workflow_runs` / `workflow_steps` tables and `threads.run_id`; unused and harmless).

### Projects

A project is a directory on a host; the user clones it there and adds the absolute path. The host validates the path and records the normalized git `origin` (if any). Removing a project is a soft delete: its threads stay and are shown as orphans. No git management beyond workspaces (below).

### Version control and workspaces (`vcs.rs`)

Everything shells out to `git` / `jj`; jj wins where both apply (a colocated repo is driven by jj). A jj workspace goes stale when its commit is rewritten from another workspace (the user rebases in a terminal) and every jj command in it refuses until `jj workspace update-stale`; `vcs::run` does that once on that error and retries, and `ensure_loaded` runs it before an agent starts in a workspace, so a thread's files follow what was done to its history without anyone noticing. `VcsStatus { path }` answers
whether a directory is a repo, whether it is dirty (git: `status --porcelain`; jj: `@` is not empty), its branch (jj: the change
id of `@`) and its `refs` (git branches and tags, jj bookmarks: what a workspace can start from); a directory the vcs can't answer for (a forgotten jj workspace) reads as no repo, never an error: it is a background probe. A thread can work in an **isolated workspace** instead of the project directory: a git worktree on branch
`dirigent-<last 8 chars of the thread id>`, or a jj workspace of that name (colocated when the repo is), at
`$XDG_DATA_HOME/dirigent/workspaces/<that name>`, so the directory, the branch and the jj workspace all read the same. A thread in a workspace is just a thread whose `cwd` points there, with
`threads.workspace` recording the directory it owns. `CreateThread { workspace: true, base }` makes one at `base` (the draft's `isolated` chip and its `from` chip; `base` is any text
git / jj resolve, a branch, tag, bookmark, commit or revset, default `HEAD` / `@-`; a bad one fails the create and leaves nothing
behind); `MoveToWorkspace { base }` moves a live thread, refused while it runs or while the main checkout is dirty (its own uncommitted edits
would stay behind), and recreates a missing workspace (the branch is still in the repo; a leftover directory the vcs forgot is deleted first). `RemoveWorkspace`
takes the thread back to the project directory: `detach: false` removes the worktree / forgets the jj workspace (one the repo already forgot is fine) and deletes the
directory (uncommitted changes go with it), `detach: true` leaves it on disk for the user. The git branch is never deleted. Every
move unloads the session (the next prompt starts in the new cwd) and, for a thread with history, appends a `custom_message` entry
(`dirigent:workspace`, parent = the last entry) telling the model where it works now. `DeleteThread` removes the thread's
workspace with it. A workspace is _missing_ (`workspace_missing`) when its directory is gone or the vcs no longer knows it as a
working copy (`jj workspace forget` keeps the files; a pruned git worktree): `vcs::is_workspace`, one ~15 ms `jj` / `git` call, so it
runs in `sweep_workspaces` (at start, every 10s and on `GetThread`), not on every `threads()`; the sweep keeps the set and pushes
`Threads` when it changes, so a workspace removed outside dirigent shows up. A move clears the thread from the set at once.

Test: insert thread + entries, read back in order.

### Sub-agents (`lib.rs`)

The runtime's `dirigent` tool is a ws client of the service (it passes its own url as `DIRIGENT_URL`), so
everything it does is an ordinary request. What the service adds on top: `CreateThread { parent }` records the lineage, starts the
child in the parent's cwd and copies the parent's tools (a spawned thread's prompts come from the tool, which knows no settings)
and stores the `name` its spawner gave it. On `agent_settled` of a thread with a parent,
`on_child_settled` sends the parent a `dirigent:child` notice (the child's last final reply after its latest prompt, or that it
stopped without one, or that it is waiting on the user's answer) through `notify_thread`, the same load-and-queue path as an
`Answer`.

### Runtime supervisor (`runtime.rs`)

- Spawns `bun run <runtime path>` (path from `DIRIGENT_RUNTIME` env / config). Reads stdout lines, writes stdin lines.
- Tracks `loaded: HashSet<thread_id>`. On `Prompt` for an unloaded thread: load entries from sqlite â send `load` â wait for `loaded` â send `prompt`.
- A thread is `running` from the moment a `Prompt` (mode `prompt`) arrives â the service also writes the prompt's model / thinking level and `prompted_at` (when the thread was last given a task; steers, follow-ups and answers don't count) to `threads` right then and pushes `Threads` â until pi's `agent_settled`. Loading a session (and capturing a nix shell) can take seconds; clients see the thread working with its final model the whole time. A prompt that fails before pi starts settles it again.
- Every `entry_appended` event â insert into `entries`, bump `threads.updated_at`, push `Threads` update to clients.
- Runtime exits/crashes â clear `loaded`, respawn, log. In-flight prompts get an `Error`. This is the whole point of a stateless runtime.
- `jobs` messages: the running-job count per thread, kept beside `running` and cleared with it when the runtime exits; `list_threads` fills `ThreadSummary.jobs` from it.
- Idle unload (never while it has jobs running, which would kill them): thread with no activity for 30 minutes â `unload`.
- Thread `name`: `CreateThread.name` (a sub-agent's), else pi's `session_info` entry (which is how the runtime's generated title arrives), else the first line of the first user message (shown meanwhile).

### WebSocket server (`server.rs`)

- `axum` on port 7777 (`DIRIGENT_PORT`), bound to `127.0.0.1` plus every address `tailscale ip` reports â never a wildcard, there is no auth. `DIRIGENT_BIND=ip,ip` overrides the list. Route `/ws`.
- Each connection: send `Hello`, then a task that handles requests and a broadcast receiver for pushes.
- Pushes come through one `tokio::sync::broadcast<ServerMessage>`.

Manual test: `websocat ws://localhost:7777/ws`, send `{"type":"list_threads","id":1}`.

## 4. `dirigent_desktop` (Rust, gpui)

Deps: `gpui` + `gpui_platform` (git, zed main; `gpui_platform` needs `features = ["wayland", "x11"]` or it runs headless), `tokio` + `tokio-tungstenite`, `rusqlite`, `dirigent_protocol`.

Status: implemented; `scripts/ui.sh` + `testing.md` cover headless launch/screenshot/input (Xvfb).

### Startup

1. Open cache db at `$XDG_DATA_HOME/dirigent/desktop.db`, `config.toml` and `hosts.toml`. Load hosts + cached threads. Render immediately.
2. For the `local` host: try to connect to `ws://127.0.0.1:7777/ws` (`DIRIGENT_PORT`); if refused, spawn `dirigent_service` _detached_ (look next to own exe, then `PATH`) and retry with backoff. The service outlives the desktop on purpose: it is the source of truth and keeps agents running. Respawn is rate-limited to once per 10s.
3. For every host: start a connection task. On connect â `ListThreads` â upsert cache â re-render.

Hosts: `local` (implicit, the embedded service) plus `$XDG_CONFIG_HOME/dirigent/hosts.toml` (`hosts.rs`):

```toml
[hosts]
hetzner = "hetzner.tail1234.ts.net"   # host[:port] or ws://host:port/ws; port defaults to 7777
```

The file is the truth. Settings âº Hosts lists hosts with
their status, adds one (name + address form) and removes one (hover â); both rewrite the file and then `sync_hosts` diffs it against
the running connections â the same path that picks up hand edits when the page is opened. Removing a host
(or changing its url) drops its connection and everything cached for it.

### Config

`$XDG_CONFIG_HOME/dirigent/config.toml` (`config.rs`) holds everything else the user can set; the gear at the sidebar's
bottom right opens Settings âº Config (`settings.rs`), its UI. The file is the truth: the page writes it, and a 1s
mtime poll (`config::Watch`, from `tick`) picks up hand edits to it or to the theme dir and applies them live.
A missing file is written out with every default.

```toml
theme = "dirigent"       # built-in, or ~/.config/dirigent/theme/<name>.toml (a user file with a built-in's name replaces it; default.example.toml is the reference, rewritten from the embedded default on every start and never listed)
telemetry = false        # recorded, not wired to anything yet
[fonts]                  # families as fontconfig knows them; a missing one falls back to the generic and toasts
ui = "Lilex"             # chrome
prose = "Lilex"          # the transcript's messages
code = "Lilex"           # code, tool calls, model ids
[title]                  # how a thread is named from its first prompt (see TitleGen); one model, so titles read alike
model = "anthropic/claude-haiku-5-5"
reasoning = "low"
prompt = "..."
[tools]                  # dirigent's own agent tools (runtime `tools.ts`); the enabled set goes out with every Prompt
async_bash = true
ask_user = true
dirigent = true          # the orchestrator: sub-agents, thread management
```

Themes (`theme.rs`): a `Theme` is 23 colours in a toml (surfaces incl. `popup` for menus / toasts / cards, text, accent, bubble, danger, status), each
`#rrggbb` or `#rrggbbaa` (stored as `0xRRGGBBAA`, painted with `rgba`). A translucent `canvas` makes the window
see-through (`theme::background` → `WindowBackgroundAppearance::Blurred`, set at open and on every theme change). Ten
are built in (`assets/themes/`): dark `dirigent` (default), `dirigent-transparent` (pure black canvas at 90%, main panel at 20%, translucent
panel, inputs and bubbles), `midnight`, `carbon`; middle `gruvbox-dark-hard`, `tokyo-night`,
`kanagawa`; light `dirigent-light`, `gruvbox-light`, `rose-pine-dawn`. The current one is a global read with
`theme::t()` at render time, so a switch is just `theme::set` + notify.

Titles: the desktop sends `[title]` as `Prompt.title` with a thread's first prompt (`entry_count == 0`) and marks the
thread as _titling_ until its `session_info` entry (the name) or a `warning` event arrives, or 60s pass; while titling
the placeholder title (the first line of the prompt) breathes in the header and the sidebar card (`theme::title_text`).
The page's `Test` opens a sample first message, sends it as `GenerateTitle` to `local`, and shows the title with
`model Â· in Â· out Â· cost Â· time`.

### Cache db

```sql
projects (host, id, path, origin, created_at, PRIMARY KEY (host, id))
links    (host, project_id, group_key)             -- manual cross-host links (desktop-owned)
threads  (host, id, project_id, cwd, name, model, thinking_level, created_at, updated_at, entry_count, last_run_ms, archived, workspace, PRIMARY KEY (host, id))
entries  (host, thread_id, seq, json, PRIMARY KEY (host, thread_id, seq))
models   (host PK, json)                           -- cached model list so the picker works offline
blobs    (id PK, mime, bytes)                      -- image bytes by content hash, host-independent
seen     (host, thread_id, updated_at)             -- the thread's updated_at when the user last opened it (unread tracking)
snoozed  (host, thread_id, updated_at)             -- legacy: snoozes from before the hosts kept them, handed over on connect (`take_legacy_snoozes`)
pinned   (host, thread_id)                         -- threads kept on top of their status group
pins     (host, thread_id, item)                   -- pinned messages, by timeline index
drafts   (host, thread_id, text, sel_start, sel_end, reversed)  -- the composer per thread; host = thread_id = '' is the new-thread draft
draft_images (host, thread_id, n, mime, bytes)     -- its pasted images, by their `[image-N]` number
kv       (key PK, value)                           -- last used model / effort / isolate, panel widths, last pane, project order
```

Same shape as the service db plus a `host` column. Writes happen off the UI thread. Columns added later go through `COLUMNS` / `ensure_column`, like the service db.

The composer is one `TextInput`; `Dirigent::set_pane` saves its text, selection and images under the thread (or draft) being left and loads the one being entered (`save_draft` / `load_composer`). Every edit saves too, so drafts survive a restart; sending, emptying or deleting the thread removes the row.

### Project groups (`projects.rs`)

The sidebar groups projects across hosts: by normalized git origin automatically, or by a manual "link toâ¦" (stored in `links`; `unlink` clears it). A group with members on several hosts asks which host when creating a thread. Threads whose project was removed appear under "orphaned".

Order: by most recent activity until the user drags a row. Grabbing a project row (no handle) lifts a ghost of it under the cursor and leaves a gap that moves row by row with the cursor (`Dirigent::move_project`, on every `on_drag_move`), so the others shuffle around it live; the whole list's keys are then frozen in `kv` (`project_order`), and groups not in it (added later) follow by activity.

### State

One `App` entity holding:

- `hosts: Vec<HostState>` â `{ name, url, status: Disconnected | Connecting | Connected, threads: Vec<ThreadSummary> }`
- `pane: Inbox | Workpool | Project { group_key } | NewProject { host } | Config | Settings | Usage | Draft { target } | Thread(OpenThread)`
- `config: config::Config` + `fonts` + `watch`; `settings: settings::State` (the Config page's inputs and title test); `titling` (threads whose name is on its way)
- `usage: usage::State`: the usage page's window, grouping, hidden series and each host's buckets
- `OpenThread` â entries + the `Timeline` folded from them + `streaming` (the in-flight message, running tools, echoed prompt)
- `pending: Option<(req_id, text)>` â a sent draft waiting for `ThreadCreated` before it can `Prompt`
- `isolate` (the draft's `isolated` chip, remembered in `kv`) and `vcs: (host, path) -> VcsInfo`, what hosts last said about the draft's project, the open thread's project and its workspace unless missing (`request_vcs`: on draft open / target change, thread open, every `Threads` push, when a workspace menu opens, and every `VCS_REFRESH_SECS` (2s) from `tick` while the draft page or a thread is on screen, so branches made and commits moved in a terminal show up without a reopen and the toggle ungreys as soon as the checkout is committed)
- `sidebar_width` / `sidebar_hidden` / `sidebar_peek` â drag-resized width (persisted in `kv`), `[` toggles, hover on the left edge peeks

The pane is persisted in `kv` on every navigation (inbox / workpool / project / thread; drafts and the
new-project form are transient and skipped) and restored at startup, so the app reopens where it was left.
A restored thread renders from cache; its `GetThread` is sent when the host connects, since requests made
while offline are dropped.

A thread's status is derived, never stored: `running_since` set â Working, `archived` â Archived, open `asks` â
Asking (an amber question mark and an amber title in the sidebar and lists: the model is waiting on the user), `snoozed` → Snoozed, else Done
(`Dirigent::status` reports a Done thread with `async_bash` jobs running (`jobs`) or sub-agents still at it as Waiting, an hourglass: their results will wake it).
Inbox = Asking, Done and Snoozed threads across hosts (`inbox_all`), Workpool = Working and Waiting ones (`busy_threads`), a project page = all of its threads with
archived ones in a trailing section. Sidebar, Inbox, Workpool and project rows are grouped by status (`sort_by_status`: working,
asking, done, snoozed, archived), _pinned_ threads first in each group (the header's pin button, amber while pinned; an amber pin
before the name marks them everywhere), newest first otherwise. Inbox and Workpool are fixed queues, newest on top: the Workpool is
ordered by `running_since`, the Inbox by `updated_at` (= when the run finished, since only entries bump it),
so a thread moves only when it enters the list. Every on-screen clock is driven by one permanent 1s
`tick`; running spans show whole seconds (`elapsed`), finished ones `34ms` / `0.34s` / `12s` (`duration`). A Done thread is _unread_ (accent title) until the user opens it:
`seen` remembers each thread's `updated_at` as of the last look, and the open thread is marked seen on
every `Threads` push, so a run that finishes while you watch is not unread. The sidebar lists the whole
Inbox so threads are a click apart; _snooze_ (the moon on a card, a list row or the thread header) takes a
thread out of the sidebar without archiving it â the Inbox page still lists it, with a moon for its glyph:
the host remembers its `updated_at` (`SnoozeThread`; agents snooze through the same request), so the next run brings it back by itself, and the moon button (amber
while snoozed) wakes it by hand. A `Focus` push (an agent's `open`) selects the thread.

Network tasks push `ServerMessage`s into an mpsc channel; a gpui background task drains it and calls `cx.update(|app| ...)` + `notify()`.

### UI (all hand-rolled)

Linear-like: the window canvas (darkest) carries the sidebar; the main pane is a rounded, bordered panel
inset 8px from it. One indigo accent for live things (running, send, focus), IBM Plex Sans for UI and prose,
IBM Plex Mono only where the content is code (tool calls, results, model ids). Status has one glyph per
state (still accent dot / check / amber moon / archive box) reused everywhere; no spinners.

- `theme.rs` â the `Theme` palette (global, loaded from toml; see Config above), `Fonts`, `Status`, and the shared widgets (icon button, button, header tab, chip, labeled chip, empty state, breathing title text).
- `config.rs` â `config.toml`: load / save / defaults, font resolution through `fc-match`, and the mtime `Watch`.
- `settings.rs` â Settings âº Config: theme chip, the three font fields (an amber warning on one whose family is not installed), title model / reasoning chips, the prompt, `Test`, a switch per agent tool (`[tools]`), the telemetry switch. No save button: text fields apply 300ms after the last keystroke (`input::Changed` → `schedule_apply`), chips and switches at once.
- `sidebar.rs` â title row with the compose button, Inbox / Workpool (label and count in amber / the working blue), each listing its threads (finished and not snoozed / working) as cards (`project, host â¦ run time` / title / model, the run time live while working and swapped for the snooze button on hover; a chevron on the nav row collapses them), project list (`+` on the section header adds one; `â¯` on a row: link/unlink, remove; drag a row to reorder, see Project groups), removed-projects row, and a bottom bar with the hide/show button and the settings gear. `docked()` adds a drag handle on its right edge; `peek()` is the hidden state: a 10px hot zone on the window's left edge that grows into a floating copy while hovered.
- `orchestra.rs` — the lineage tree, the top section of the thread's right panel (`thread::side_panel`: one panel, `log_width` wide, split off by a hairline with a drag handle, no title row; the tree over the work log, each only when there is one, the tree capped at a third of the height and scrolling past that; a word about sub-agents when there is neither). The panel is open while there is something to show (a family or a log) unless the header's panel icon button (or `]`) said otherwise (`OpenThread::panel`, `Dirigent::panel_open`); opening it by hand with no log picks the run in progress, else the last work. Clicking a work line shows its log (opening the panel) and clears the by-hand choice; clicking the shown one drops the log, so the panel stays for the family or closes. **Sub-agents never appear in the sidebar or the lists** (`ThreadSummary::top_level` filters `all_threads` and `projects::groups`): this tree is their place. It shows the open thread's family, `orchestra::lineage`: up from the thread to its topmost ancestor through `parent_id`, then depth-first down through every thread's sub-agents. A thread's children come in the order they were last given a task (`prompted_at`, else `created_at`; ties by creation, then id): a new child lands last, one re-used for a new turn moves last, nothing else ever moves, so the tree reads as a log of who was asked to do what, in order. Rows are indented per level with the status glyph and the title, the open thread highlighted, the working / asking ones bright and the finished ones dim; a click opens the thread.
- `lists.rs` â the 40px header bar (breadcrumb + actions), the three list pages, the new-project form and the settings pages' header with its tabs (Config in `settings.rs`, Hosts here: a narrow centered column; Usage in `usage.rs`). Rows: glyph, title, project, model, age; hover reveals snooze and archive.
- `usage.rs` â Settings âº Usage: what the agents cost over time, across hosts. One big chart of cost: a monotone-cubic smoothed line per series with a faint gradient under it, quarter gridlines labelled in dollars, time ticks aligned to local hours / midnights. The headline is the visible series' total for the window. Window: `1d / 7d / 30d` ending now (refreshed every 30s while open), `â¹ âº` pages it, `custom` reveals two date fields. Grouping chip: by model / by provider / everything. Hovering the chart draws a crosshair with a dot per line and a card beside the cursor (flipped to the cursor's other side on the right half) with every line's cost in that slot and the split behind it: `1.7M in Â· 15k out Â· 98% cached`. The legend under the chart is a table of per-series totals (cost, in, out, cached %, messages); clicking a row hides its line (hollow swatch) and the headline follows. Slot width follows the window (â¤ 60 points); every host gets the same `ListUsage` and `Model` folds the replies.
- `menu.rs` â the one popup (`Dirigent::menu`): model, effort, draft target, project actions, link target, host picker, delete confirm. Anchored to its trigger with `deferred(anchored())`.
- `thread.rs` â header (breadcrumb — `project › parent › title` for a sub-agent, the parent a click away, with the status glyph and the pin mark before the title —, snooze, pin, archive, delete, then the right panel's icon button) + `list()` of timeline items in a centered 720px reading column + the composer card, with an `ask_card` per open `ask_user` call floating above it (a zero-height layer at the composer's top edge that grows upward, so it covers the transcript instead of pushing it), nine tenths of the composer's width, a stack: the question being answered is a card (each option on its own row, circle = one choice, square = several, plus a row holding the text field where the question takes typed text) and the others are one-line rows in their order, answered ones above it with a check and their answer, the rest below, each a step further in and fainter; clicking a row opens it. `Next` (or Enter in the field) opens the next unanswered question, `Answer` replaces it once every question has something, `Dismiss` sits in the header. `OpenThread::asks` holds the forms (`AskForm`: picked indices + inputs + the open question), reconciled against the summary's `asks` by `sync_asks` on open and on every `Threads` push; `Answer` goes out once (`sent` greys the card) and the host's next push removes it. User messages are raised, replies are markdown prose, and each turn's _work_ is one line: `âº model  effort  duration  n tool calls`, plus what it is doing right now while it runs (`Â· bash 3.2s`, `thinking`, `writing`, re-rendered every second). The transcript never shows the steps: clicking a work line (or `]`) opens the right panel's log section, `log_body()`, with the full log as a lazy `list()` of steps (`render_step`: thinking, interim prose, tool rows with elapsed time and the call's `timeout`; tool rows expand to all arguments and the output, with a thumbnail per image in the result: what the model saw when it `read` a png). On a live work the log follows the latest step unless the user scrolled up (`FollowMode::Tail`), and an open log follows the next prompt's run. `]` toggles the panel; Esc (when nothing runs) drops the log. `draft_page()` is the same composer alone, centered in the pane, with a `Create thread in <project>` chip above it.
- `ruler.rs` â the ruler over the transcript's left edge: one tick per message (user blue, reply gray, pinned amber, compaction dotted), all the same length, vertically centered at a 7px pitch (squeezed once they'd overflow), the ones in view shorter and brighter. Hover previews the message, click scrolls it to the middle of the viewport (`Dirigent::jump_to` + `settle_jump`). Painted by one `canvas`, which also steps the tweens (marks grow in, the in-view state follows the scroll) and asks for the next frame only until they settle. Pins (`OpenThread::pins`, the `pin` button next to `copy`, an amber bar in the margin) are timeline indices stored in the cache's `pins` table.
- `markdown.rs` â `render(text, mono, keys, image)`: pulldown-cmark events â block `div`s (paragraph, heading, list, code block, blockquote, table, rule) and inline highlight runs on a `StyledText` (bold, italic, strikethrough, code in mono, links coloured but not clickable), each paragraph a selectable `select::Block`. Inherits size/colour/font from the caller, so it serves replies, user bubbles, and the mono work log alike. An image whose target is `blob:<id>` is drawn by the caller's `ImageFn` after its paragraph's text (`thread::inline_image`: scaled into the column, click for full size); replies and the log's prose pass one, so a reply shows the screenshot it links to. `committed(text)` is the prefix up to the last newline.
- `select.rs` â mouse selection over the transcript and the work log (two `Doc`s; a selection never spans both). Every paragraph, code block, tool argument / output, steer, error line is a `Block`: a `StyledText` keyed by `(doc, item, seq)` in document order, so a selection is two `(key, byte offset)` positions and blocks in between are selected whole. Blocks register their layout when painted (hit testing, highlight) and their text when constructed; the highlight (`paint_selection`, shared with the composer) is one lyon-filled shape per block, its rows only as wide as their selected text, with every corner rounded â including where a row steps in or out against its neighbours; copying (`ctrl+c`, and the primary selection on mouse up) constructs the selected items again, layout-free, to read off-screen blocks too. Click, double click (word), triple click (block), shift+click, drag with autoscroll at the document's edge; Esc or any other press clears. A press on a block keeps the composer focused. Hovering a user message or reply reveals `pin` and `copy` buttons in its corner (`copy` copies the raw markdown; `pin` see `ruler.rs`).
- `input.rs` â multiline text input. gpui has no text input widget; implements `EntityInputHandler` for IME + key handling (see zed's `crates/gpui/examples/input.rs`). Ctrl+Enter submits, shift+Enter submits with `queued` set, Enter inserts a newline (single-line inputs submit on Enter). Grows with its text up to 70% of the window, then scrolls with a thin bar; the cursor is kept in view. Double click selects a word, triple everything; a drag keeps following the mouse after it leaves the input (above = to the start, below = to the end). Undo / redo (`ctrl+z`, `ctrl+shift+z` / `ctrl+y`): typing a word is one step, so is a run of deletes; paste, newline and IME commits are steps of their own. `ctrl+c` with nothing selected hands the action on to the root, which copies the transcript selection. Also used single-line for the new-project path. Pasting an image emits `ImagePasted` instead of text; `[image-N]` tokens are shaped in the accent color.
- Composer: a draft (`ctrl+n`, the compose button, or a project page's button) opens the draft page; the first ctrl+Enter sends `CreateThread`, and `Prompt` follows once `ThreadCreated` arrives (`pending`); the created thread is listed as already working with the picked model, so it never flashes through the inbox. Navigating away keeps the text. Pasted images become `[image-N]` tokens in the text plus a thumbnail strip under it (click to preview full-size, hover â to remove); the text is the truth â only images whose token is still present are sent (`attachments()`), so deleting the token drops the image. Chips: workspace (below, first), model, effort, context usage (hover it for the thread's totals so far: tokens in / out, cached share, message and compaction counts, cost; `thread::stats_card` over `view::Stats`). **Workspaces** (`workspace_chip`, only when the project is a git / jj repo): one chip. `in project` (a draft, or a thread still in its project directory) opens the workspace popup (`base_picker`): `in project` alone on top (selected while that is the state), a hairline, then `New workspace from`: a search over the repo's refs with the default (`HEAD` / `@-`) first, Enter takes the top match or the typed text as is (`main~3`, a revset). Picking on a draft sets `Dirigent::isolate` + `base` (the chip reads `workspace from <base>`; the `in project` row undoes it) and goes out with the `CreateThread`; picking on a thread moves it at once; the draft page warns under the composer (centered, in a zero-height layer so the composer never moves) when another thread is working in that checkout (amber, the louder one) or the working copy is dirty, each line followed by an `isolate it` link. A thread in a dirty checkout can't move (its changes would stay behind): the chevron goes faint, the popup stays shut, hovering says why. A thread in a workspace shows its name in the accent colour (`dirigent-abcd1234`, also after the title in the breadcrumb), opening a menu: copy the workspace path, or back to the project with the workspace removed (red) or kept on disk. Nothing while the thread runs. The header's archive button on a thread with a workspace opens `ConfirmArchive` (remove / keep on disk, stating whether the workspace has uncommitted changes); a list row's archive opens the thread with that menu. A workspace that disappeared outside dirigent (`workspace_missing`: deleted, or forgotten by the vcs) turns the breadcrumb's branch amber and puts an amber banner over the composer with `Recreate` and `Back to project`; sending is refused until one is picked. While running: ctrl+Enter steers, shift+Enter queues a follow-up, Esc / the stop button aborts. Model list is the host's cached `models`; the thread's own model wins when opening a thread, otherwise the last one used.
- `main.rs` â window, config load, keybindings (`ctrl+n` new thread, `[` toggle sidebar, `]` toggle the right panel â types a bracket instead when the focused input already has text, `ctrl+q` quit).
- `view.rs` â pi entry/event JSON â `Timeline` of `Item`s (`User` with its image blob ids, rendered as thumbnails once `Dirigent::blobs` has them; `blobs_shown` lists every blob a timeline needs, those, tool-result images and the `blob:` links in prose, for `fetch_blobs`, `Work { model, effort, started, ended, steps }`, `Assistant`, `Error`, `Compaction`, `Notice`), folded entry by entry; tool results and timings (`durationMs`, timestamps) land in their `ToolCall` step. A user message that arrives while a work is open is a steer and becomes a `Step::Steer` of that work, so one run stays one `Work`. Steers render as user bubbles with a `steer` label: pending ones (pi still holds them) below the work line at the end of the tail, with `pending` and, for typed ones, `cancel` on hover; taken ones above the work line, kept after the run. Answers to an ask are steers too: `Streaming::answers` echoes them (same markdown as the persisted entry) until their `dirigent:answer` entry lands. `custom_message` entries from our tools follow the same rule: a `dirigent:answer` is the user's words (a bubble, or a steer step mid-run; the question in bold, the answer under it) and its ISO `timestamp` starts the work it triggers; a `dirigent:job` is a one-line notice (`job cargo check, exit 1, 12s`: an `Item::Notice`, or a `Step::Notice` row in the log panel mid-run); a `dirigent:child` (a sub-agent settled: `sub-agent "Review" finished` / `stopped without a reply` / `is asking the user`) is an `Item::Child` that opens the sub-agent on click, or a `Step::Child` of the open work mid-run, rendered the same way under the work line (and in the log). A sub-agent's start gets the same line: a `dirigent` spawn call's result (`Spawned sub-agent thread <id>`) adds a `Step::Child` `sub-agent "Review" started` after the call (`ToolCall::spawned` carries the name), so each child has its start and end in the parent's transcript. `Streaming` holds the in-flight message's steps, the tools executing right now (with start times), and the optimistic echoes of sent messages with their `PromptMode`, oldest first (prompts show above the live work, steers as pending bubbles after it, follow-ups below the work). pi drains steers one per turn after that turn's tool calls and persists nothing before, so an echo lives until its user entry lands (`take_echo`) or the run settles.

The thread list is `timeline.items.len() + 1` items: the last one is the live tail (the echoed prompt, the work in progress, and the reply in progress, rendered there instead of in their own slots so streamed steps join the persisted ones). New items `splice` in before it; deltas only `remeasure` it.

Streamed text shows a line at a time, not a word at a time: the view renders `markdown::committed()` of the buffer, and a delta only counts as a change (`Streaming::apply` â true) when it contains a newline. Since models emit one line per block, paragraphs and list items appear whole and nothing inline ever flickers. The model's latest prose (`Work::latest_text`, over the persisted and streamed steps) renders below the work line in reply style; the next prose, or the final reply once the turn ends, replaces it. So the reading column only ever grows while a run goes on.

Fast-UX rules: thread switch renders cached entries synchronously then requests fresh; deltas mutate the streaming buffer and `notify()` â no reparsing of the whole thread per token.

### Platform

- Linux/Wayland only for the MVP. Dev shell libs are in `flake.nix` (`gpuiLibs`).
- `TODO(macos)`: service spawn path (app bundle `Contents/MacOS`), code signing.
- `TODO(windows)`: service spawn path, `XDG_DATA_HOME` â `LOCALAPPDATA`, console window hiding.

## Phases

1. **Scaffold** â add `dirigent_protocol`, deps, `bun add` pi sdk. `cargo build` + `bun run typecheck` green. (desktop crate + flake libs already done)
2. **Runtime** â stdio loop, load/prompt/abort/events. Verify with `echo '{"type":"load",...}' | bun run ...`.
3. **Protocol** â types + round-trip tests.
4. **Service** â storage, supervisor, ws. Verify with websocat: create thread, prompt, watch events, restart service, thread still there.
5. **Desktop** â connect + cache + sidebar + thread view + composer, local service spawn. First real use.
6. **Hardening** â reconnect with backoff, runtime respawn, idle unload, error toasts.

All six phases are implemented.

Out of scope for now: clickable links, forking/branch navigation, extension UI dialogs, auth, multi-window, editing a host in place (remove and re-add, or edit `hosts.toml`).
