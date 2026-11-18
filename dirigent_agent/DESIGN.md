# dirigent_agent

Dirigent's agent: what turns a thread's history and a message into a model's replies and tool
calls. A library the service embeds, with no process, protocol or config scopes of its own.

## What it is, and is not

- One way to a model: a CLIProxyAPI the user runs. Two wires behind it: Anthropic Messages
  for Claude, Codex Responses for every other model.
- The loop (steer, follow-up, abort, retry), compaction, `read` / `bash` / `edit` / `write`,
  `async_bash`, `ask_user`, codemode (QuickJS: `tools`, `store` / `load`, `searchTools`), MCP
  servers (stdio and HTTP, OAuth), computer use, skills, `AGENTS.md`, images.
- Not there, on purpose: a terminal UI, extensions or packages, providers to sign in to,
  per-project configuration (a repo's own skills, MCP servers or settings), prompt templates
  and `/skill:name`. What a user sets is the app's Settings or `agent.toml`.

## The crates

```
dirigent_protocol   wire types, and `transcript`: Entry, Block, Event (one format: memory, db, wire, UI)
dirigent_agent      this crate
dirigent_service    threads, storage, workspaces, hosts, the ws server; embeds dirigent_agent
dirigent_desktop    the app
```

`dirigent_agent` depends on `dirigent_protocol` only. What it needs of the service it asks
through the `Host` trait, and the model through `Llm` (both `lib.rs`), so it runs without
either: `examples/run.rs` is a session with a printer for a host, the tests script the model
(`testing.rs`).

## Modules

| module     | what                                                                                                         |
| ---------- | ------------------------------------------------------------------------------------------------------------ |
| `proxy`    | the CLIProxyAPI client: the models and their prices, the two wires (`anthropic`, `codex`), what a failure is |
| `session`  | a live thread: its branch, queues (steer / follow-up / notices), jobs, MCP servers; `prompt`, `abort`, …     |
| `run`      | the loop of one run: request, stream, tool calls side by side, queues, compaction, retry                     |
| `context`  | a branch as request messages; compaction                                                                     |
| `prompt`   | the system prompt: tools, rules, `AGENTS.md` files, skills                                                   |
| `tool`     | `Tool`, `ToolSpec`, `ToolOutput`                                                                             |
| `tools`    | `read`, `bash`, `edit`, `write`, `async_bash` (jobs), `ask_user`, `computer`                                 |
| `codemode` | the QuickJS sandbox and the `codemode` tool                                                                  |
| `mcp`      | the MCP client, a session's servers as tools, what Settings does to them                                     |
| `config`   | `agent.toml` and `proxy.json`                                                                                |

## How a run goes

```
Host (service)                          dirigent_agent
  ensure_loaded ───────────────► Agent::load(thread: id, cwd, entries, model, effort, tools)
  prompt ──────────────────────► Session::prompt(text, images, mode, …)
                                   run: loop {
                                     steers / notices in  → Out::Entry
                                     compact if the context is near the window
                                     llm.stream(request) → Out::Event (deltas) → Out::Entry (the reply)
                                     tool calls, side by side → Out::Event, Out::Entry (results)
                                     nothing left to do and no follow-up → Event::RunEnd
                                   }
  Host::emit(thread, Out) ◄─────
```

- An entry exists once the host has it (`Out::Entry`): the session appends to its own branch
  and hands the entry over in the same step. What a session puts out goes through one
  channel, so the host gets it in order, and no session lock is held while it does.
- A request is built from the branch every turn (`context`): notices become user messages,
  images are loaded from the host's blobs, a tool call left without a result (a crash, an
  abort) gets one saying so. Nothing is repaired in the stored history.
- Thinking is replayed only to the model that made it, on the wire that made it
  (`Assistant::api`), and only when signed.
- Steers go in after the current tool calls, all that are queued at once; follow-ups once
  the model would stop. Notices (a job ended, a sub-agent ended, the user's answers) ride
  the same queues and start a run when the model is idle.
- Abort cancels a `CancellationToken`: the request ends with what it had (`Stop::Aborted`),
  tools end what they started.
- A failed request is retried when the proxy says it is worth it (`Failure::Transient`);
  one too long for the window compacts and goes again (`Failure::Overflow`).
- Every command is a job with a log (`tools/shell.rs`): `bash` waits for its own, and hands
  one that takes long to the background when the thread allows it; `async_bash` starts them
  there. A job's end is a notice.
- An MCP server's tools are tools like the others. Those of a server set to reach the model
  through codemode are left out of what the model is told while the thread has codemode
  (`ToolSpec::deferred`), and a script calls them.

## Storage (the service's)

`<data dir>/service.db`: `entries(thread_id, seq, id, parent_id, at, kind, json)` with `json`
the `transcript::Body`. Usage, spend and the search index are filled as entries are appended.
A thread's model is its id alone (`claude-opus-5-5`); usage rows keep a `provider` column,
which is who makes the model (`anthropic`, `openai`).

## Files

```
<config dir>/agent.toml        compaction, per-model settings, MCP servers (0600); no key of ours, so it can be shared
<config dir>/AGENTS.md         the user's instructions for every thread
<config dir>/skills/<name>/SKILL.md
<data dir>/proxy.json          the proxy's url, API key and management key (Usage) (0600); a test instance gets no management key
<data dir>/mcp-auth.json       MCP OAuth tokens (0600)
<data dir>/models.json, prices.json   what the proxy serves and what it costs, as last seen
<data dir>/jobs/<thread>/      the logs of commands
<data dir>/attachments/        images sent with prompts, as files the model can use
<data dir>/screens/<thread>/   a thread's screen (computer use)
```

`agent.toml` and `proxy.json` are read when they are asked for, so an edit applies to the next request. Project
instructions are the `AGENTS.md` (or `CLAUDE.md`) files from the thread's directory up.
Lowercase `agents.md` and `skill.md` are read too.

## Working on it

- Tools keep the names, inputs and result texts models know from other coding harnesses:
  change them like an interface, not like a string.
- A real proxy spends a real subscription: `claude-haiku-5-5` and short prompts. Never
  print its key or write it into the repo. Tests that need it, the network or a driver are
  `#[ignore]`d and say how to run them; everything else runs offline in `cargo test`.
- `cargo run -p dirigent_agent --example run -- --cwd /tmp/x "…"` is the agent without a
  service; `--example ask` is one request to the proxy.
- Checks: `cargo fmt --all`, `cargo clippy -p dirigent_agent --all-targets`, `cargo test -p dirigent_agent`.
