// Live pi sessions, keyed by thread id. Everything is in memory: the service
// owns persistence by writing every `entry` message we emit to sqlite.
//
// Pi persists messages into its SessionManager silently (no event), so after
// every session event we diff the manager's entry count against what we have
// already forwarded and emit the new tail as `entry` messages.

import { createHash } from "node:crypto";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import {
  contentText,
  getSupportedThinkingLevels,
  type Api,
  type ImageContent,
  type Message,
  type Model,
  type ThinkingLevel,
  type UserMessage,
} from "@earendil-works/pi-ai";
import {
  type AgentSession,
  type AgentSessionEvent,
  createAgentSession,
  createAgentSessionServices,
  createCodemodeExtension,
  createMcpExtension,
  createToolSearchExtension,
  DefaultResourceLoader,
  type FileEntry,
  getAgentDir,
  ModelRuntime,
  SessionManager,
  type ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import {
  type Ask,
  createBash,
  createTools,
  dataDir,
  deliverAnswers,
  deliverNotice,
  killJobs,
  type Notify,
  noticeDelivered,
  requeueNotices,
  TOOL_NAMES,
  type ToolHost,
  wake,
} from "./tools";

export interface ModelInfo {
  provider: string;
  id: string;
  name: string;
  context_window: number;
  thinking_levels: string[];
}

/** How to name a thread from its first prompt (the desktop's `config.toml` `[title]`). */
export interface TitleGen {
  model: string;
  reasoning: string;
  prompt: string;
}

/** A generated title and what it cost. */
export interface TitleResult {
  title: string;
  model: string;
  input: number;
  output: number;
  cost: number;
  duration_ms: number;
}

/** An image sent with a prompt (`ImageAttachment`): base64 bytes, and the `N` of the
 *  `[image-N]` token naming it in the text (missing from older desktops). */
export interface Attachment {
  mime_type: string;
  data: string;
  n?: number | null;
}

/** File extensions of the image types whose mime subtype isn't one. */
const EXTENSIONS: Record<string, string> = { "image/jpeg": "jpg", "image/svg+xml": "svg" };

/**
 * Save each attachment as `<data dir>/attachments/<sha256>.<ext>` and return `text` with a
 * markdown reference definition per image, in the images' order: `[image-N]: /path`. The
 * model sees which file each `[image-N]` is and can use it (upload it, edit it, ...). The
 * desktop splits the definitions off for display and pairs images with tokens through
 * them (`view::split_image_refs`). Without `n` (an older desktop) a token is paired by
 * order, as that desktop shows them.
 */
function linkAttachments(text: string, attachments: Attachment[]): string {
  const dir = join(dataDir(), "attachments");
  mkdirSync(dir, { recursive: true });
  const tokens = [...new Set([...text.matchAll(/\[image-(\d+)\]/g)].map((m) => Number(m[1])))].sort(
    (a, b) => a - b,
  );
  const refs = attachments.map((a, i) => {
    const bytes = Buffer.from(a.data, "base64");
    const ext = EXTENSIONS[a.mime_type] ?? a.mime_type.split("/")[1] ?? "bin";
    const path = join(dir, `${createHash("sha256").update(bytes).digest("hex")}.${ext}`);
    // Rewritten even if it exists: same name, same bytes, and a half-written one heals.
    writeFileSync(path, bytes);
    // A destination with spaces or parens (a Windows user dir) must be in <>.
    return `[image-${a.n ?? tokens[i] ?? i + 1}]: ${/[\s()]/.test(path) ? `<${path}>` : path}`;
  });
  return `${text}\n\n${refs.join("\n")}`;
}

/** Tried in order when the configured title model has no credentials here. */
const TITLE_FALLBACKS = ["anthropic/claude-haiku-5-5", "openai/gpt-6-luna"];

/**
 * Appended to pi's system prompt. The service reads the linked file into its blobs when it
 * persists the reply, recording the blob next to the text it leaves as written (`blobs.rs`);
 * the desktop renders those inline. The last paragraph explains `linkAttachments`.
 */
const IMAGES_PROMPT = `## Showing images
Your replies are rendered as markdown in a desktop app. To show the user an image file on this machine (a screenshot you took, a chart you rendered), put it in your reply as a markdown image with its path: \`![what it shows](/absolute/path.png)\`. It appears inline in the reply. png, jpg, gif, webp and bmp work; a relative path is taken from the working directory.

Images the user attaches to a message are also saved as files on this machine: the \`[image-N]: /path\` lines at the end of the message give the file of each \`[image-N]\`, for when you need the image itself (to upload, edit or attach it).`;

/** Pi's name for the codemode tool, and ours for its switch in a thread's `tools`. */
const CODEMODE = "codemode";

/**
 * Pi's built-in codemode, tool search and MCP, which its CLI loads and an SDK session doesn't
 * (pi's `builtInExtensions`, not exported). As `builtin:<name>` resources, like the CLI's,
 * `-builtin:mcp` in the `extensions` setting turns one off and an installed extension that
 * registers the same tool or `/mcp` replaces it. MCP servers are pi's `<agent dir>/mcp.json`
 * (Settings › MCP servers, `mcp.ts`, or `dirigent pi mcp …`); the MCP extension activates `codemode` or
 * `tool_search` for them, and the `defaultTools` setting can activate `tool_search` without.
 * `codemode` follows the user's switch instead, on by default (`create`, `applyTools`).
 */
const BUILTIN_EXTENSIONS = [
  { name: "codemode", factory: createCodemodeExtension(), replaceable: true, builtin: true },
  { name: "tool-search", factory: createToolSearchExtension(), replaceable: true, builtin: true },
  { name: "mcp", factory: createMcpExtension(), replaceable: true, builtin: true },
];

/** How long the same extension notice stays quiet once sent (`extensionNotice`). */
const NOTICE_REPEAT_MS = 10 * 60_000;

export type Outbound =
  | { type: "loaded"; thread_id: string; model?: string; thinking_level: string; header: unknown }
  | { type: "entry"; thread_id: string; entry: unknown }
  | { type: "event"; thread_id: string; event: unknown }
  | { type: "ask"; thread_id: string; ask: Ask }
  | { type: "jobs"; thread_id: string; running: number; free: number }
  | { type: "nix_shell"; thread_id: string; active: boolean }
  | { type: "warning"; thread_id: string; message: string }
  | { type: "error"; thread_id?: string; message: string };

interface Live extends ToolHost {
  unsubscribe: () => void;
  // How many of sessionManager's entries have been emitted as `entry` messages.
  forwarded: number;
  // Resolved once per load. `undefined` = no flake / nix failed → plain env.
  nixEnv?: Promise<Record<string, string> | undefined>;
  // While pi takes a prompt in, up to a run starting: an extension command (`/mcp`) runs then.
  takingIn?: boolean;
}

/** A `load` in progress; `unloaded` once an `unload` came for it meanwhile. */
interface Loading {
  done: Promise<void>;
  unloaded: boolean;
}

export class Sessions {
  private live = new Map<string, Live>();
  // So a thread never has two sessions: a `load` joins the one in progress, and an `unload`
  // has it tear its session down instead of going live.
  private loading = new Map<string, Loading>();
  // Extension notices sent, by text, and when (`extensionNotice`).
  private noticed = new Map<string, number>();
  // Shared across sessions; also answers `list_models` and signs in (`Logins`).
  readonly modelRuntime = withExtensionProviders(ModelRuntime.create());

  constructor(private send: (msg: Outbound) => void) {}

  has(threadId: string): boolean {
    return this.live.has(threadId);
  }

  /**
   * Create the pi session. `model` ("provider/id") and `thinkingLevel` only apply to a fresh
   * thread (no entries): pi persists them as its first entries, so the thread never shows pi's
   * default model. A thread with history restores its own from the entries. `tools` names
   * the dirigent tools (`TOOL_NAMES`) the model gets; the rest are registered but inactive.
   * It also carries the `codemode` switch (`applyTools`).
   * A load of a thread already loading answers with it (its `loaded`, or its error).
   */
  async load(
    threadId: string,
    cwd: string,
    entries: FileEntry[],
    model?: string,
    thinkingLevel?: string,
    tools: string[] = [],
  ): Promise<void> {
    for (let pending; (pending = this.loading.get(threadId));) {
      if (!pending.unloaded) return pending.done.catch(() => {});
      // Unloaded meanwhile (moved to another cwd, say): load anew once it's torn down.
      await pending.done.catch(() => {});
    }
    if (this.live.has(threadId)) return;
    const loading = { unloaded: false } as Loading;
    // Done only once out of `loading`, so a load waiting on it finds the way clear.
    loading.done = this.create(
      threadId,
      cwd,
      entries,
      model,
      thinkingLevel,
      tools,
      loading,
    ).finally(() => this.loading.delete(threadId));
    this.loading.set(threadId, loading);
    return loading.done;
  }

  private async create(
    threadId: string,
    cwd: string,
    entries: FileEntry[],
    model: string | undefined,
    thinkingLevel: string | undefined,
    tools: string[],
    loading: Loading,
  ): Promise<void> {
    const fresh = entries.length === 0;

    // The tools hold the session they run in; it exists only after `createAgentSession`.
    const live = {
      threadId,
      cwd,
      unsubscribe: () => {},
      forwarded: entries.filter((e) => e.type !== "session").length,
      tools: [] as string[],
      jobs: new Map(),
      notices: new Map(),
      waiters: new Set(),
      dirigentCalls: new Set(),
      onAsk: (ask: Ask) => this.send({ type: "ask", thread_id: threadId, ask }),
      onJobs: (running: number, free: number) =>
        this.send({ type: "jobs", thread_id: threadId, running, free }),
    } as Live;
    // Bash runs through our spawnHook so a captured nix dev-shell env can be injected, and
    // moves a long command to the background (`createBash`).
    const bash = createBash(live, {
      spawnHook: (ctx) => (live.env ? { ...ctx, env: { ...ctx.env, ...live.env } } : ctx),
    });

    // Pi's default loader, plus its built-in extensions and our note on showing images.
    const resourceLoader = new DefaultResourceLoader({
      cwd,
      agentDir: getAgentDir(),
      extensionFactories: BUILTIN_EXTENSIONS,
      appendSystemPromptOverride: (base) => [...base, IMAGES_PROMPT],
    });
    await resourceLoader.reload();

    const { session } = await createAgentSession({
      cwd,
      resourceLoader,
      modelRuntime: await this.modelRuntime,
      model: fresh && model ? await this.resolveModel(model) : undefined,
      thinkingLevel: fresh ? (thinkingLevel as ThinkingLevel | undefined) : undefined,
      sessionManager: SessionManager.inMemory(cwd, { id: threadId }, entries),
      customTools: [bash as ToolDefinition, ...createTools(live)],
      // The `codemode` switch, as a change to the tools pi starts with (its `defaultTools`
      // setting, or the ones the transcript records). Off has to be said here: `applyTools`
      // only sees the switch go off in a loaded thread, and a thread's next load would bring
      // codemode back from its transcript. Before `session_start`, so the MCP extension can
      // still turn it on for its servers.
      tools: [(tools.includes(CODEMODE) ? "+" : "-") + CODEMODE],
    });
    live.session = session;
    // Extensions' warnings and errors (an MCP server that fails to connect or needs a
    // sign-in) reach the user as thread warnings, which the desktop toasts. Info is chatter
    // and goes nowhere, as in pi's print mode, unless it answers a command the user typed
    // (`/mcp` and its status). The rest is pi's no-UI context, and extensions
    // are still told there is no UI (`hasUI`, before `session_start`): nobody answers their
    // dialogs, so a permission gate should follow its no-UI policy and an MCP sign-in not
    // open a browser on this host.
    const runner = session.extensionRunner;
    runner.hasUI = () => false;
    const notify = (message: string, type?: "info" | "warning" | "error") => {
      if (live.takingIn) this.extensionNotice(threadId, message, true);
      else if (type === "warning" || type === "error") this.extensionNotice(threadId, message);
    };
    // Emits `session_start`, which extensions start on, as in pi: the MCP one connects its
    // servers, in the background (cliproxyapi's turns on its proactive compaction).
    await session.bindExtensions({
      uiContext: { ...runner.getUIContext(), notify },
      onError: (e) => {
        console.error(`extension ${e.extensionPath} (${e.event}): ${e.error}`);
        this.extensionNotice(threadId, `${e.extensionPath} failed on ${e.event}: ${e.error}`);
      },
    });
    // The last await: from here on the session goes live.
    if (loading.unloaded) {
      // Nobody waits for it: the service gave up on this load when it unloaded.
      await shutdownBuiltins(session);
      session.dispose();
      return;
    }
    // Hand every queued steer to the model in the next turn, not one per turn. Set on the
    // agent directly: `session.setSteeringMode` would also write pi's global settings.
    session.agent.steeringMode = "all";
    applyTools(live, tools);

    // Pi appends to the manager right *after* notifying listeners, so flush on a microtask.
    live.unsubscribe = session.subscribe((event) => {
      if (event.type === "message_start" && event.message.role === "custom")
        noticeDelivered(live, event.message);
      // Pi records nothing when compacting the context fails: a custom entry (outside the
      // model's context) keeps the error in the thread. An aborted compaction has none.
      if (event.type === "compaction_end" && event.errorMessage)
        session.sessionManager.appendCustomEntry("dirigent:compaction_failed", {
          error: event.errorMessage,
        });
      this.send({ type: "event", thread_id: threadId, event: slim(event) });
      queueMicrotask(() => this.flushEntries(threadId, live));
    });
    // Not on Windows: nix doesn't run there, so a flake is for other machines (or WSL).
    if (process.platform !== "win32" && existsSync(join(cwd, "flake.nix"))) {
      // Prompts wait for it (`prompt`); clients show the thread activating it meanwhile.
      this.send({ type: "nix_shell", thread_id: threadId, active: true });
      live.nixEnv = captureNixEnv(cwd, threadId, this.send)
        .then((e) => (live.env = e))
        .finally(() => this.send({ type: "nix_shell", thread_id: threadId, active: false }));
    }
    this.live.set(threadId, live);

    this.send({
      type: "loaded",
      thread_id: threadId,
      model: session.model ? `${session.model.provider}/${session.model.id}` : undefined,
      thinking_level: session.thinkingLevel,
      header: session.sessionManager.getHeader(),
    });
    // createAgentSession appends model/thinking entries for a fresh session.
    this.flushEntries(threadId, live);
  }

  private async resolveModel(model: string): Promise<Model<Api>> {
    const i = model.indexOf("/");
    const resolved = (await this.modelRuntime).getModel(model.slice(0, i), model.slice(i + 1));
    if (!resolved) throw new Error(`unknown model: ${model}`);
    return resolved;
  }

  /**
   * An extension's warning or error, as a warning on the thread. Every thread's extensions
   * say the same things (each load reports the same broken MCP server), so a text already
   * sent goes again only after a while, unless it answers what the user typed (`always`).
   */
  private extensionNotice(threadId: string, message: string, always = false): void {
    const now = Date.now();
    if (!always && now - (this.noticed.get(message) ?? 0) < NOTICE_REPEAT_MS) return;
    this.noticed.set(message, now);
    this.send({ type: "warning", thread_id: threadId, message });
  }

  private flushEntries(threadId: string, live: Live): void {
    const sm = live.session.sessionManager;
    if (sm.getEntryCount() <= live.forwarded) return;
    const all = sm.getEntries();
    for (const entry of all.slice(live.forwarded))
      this.send({ type: "entry", thread_id: threadId, entry });
    live.forwarded = all.length;
  }

  /** Resolves once the session's extensions have shut down (MCP servers stopped). */
  async unload(threadId: string): Promise<void> {
    const pending = this.loading.get(threadId);
    if (pending) {
      pending.unloaded = true;
      return pending.done.catch(() => {});
    }
    const live = this.live.get(threadId);
    if (!live) return;
    this.live.delete(threadId);
    live.unsubscribe();
    // Stop the run first: no tool starts (or job, which killJobs would miss) while the
    // built-ins shut down.
    live.session.agent.abort();
    killJobs(live);
    this.flushEntries(threadId, live);
    await shutdownBuiltins(live.session);
    live.session.dispose();
  }

  /** Models this host has credentials for. */
  async listModels(): Promise<ModelInfo[]> {
    const rt = await this.modelRuntime;
    return (await rt.getAvailable()).map((m) => ({
      provider: m.provider,
      id: m.id,
      name: m.name,
      context_window: m.contextWindow,
      thinking_levels: getSupportedThinkingLevels(m),
    }));
  }

  /**
   * Send a prompt, first switching the session to `model` ("provider/id") and
   * `thinkingLevel` if they differ from its current ones. Pi persists the switch
   * as entries, so it survives reloads. `attachments` are base64 images; pi resizes
   * them to the model's limits before persisting, and the text links them to files
   * (`linkAttachments`). `title` (a desktop thread's first
   * prompt; sub-agents are named by their spawner) names the thread from `text`
   * alongside the run: pi persists the name as a `session_info` entry, which is how
   * the service and clients learn it. `tools`, when given, is the dirigent tool set
   * from now on (see `load`). `edit` is the id of a user message the prompt rewrites
   * (`EditMessage`): the session first goes back to just before it (`rewind`), and comes
   * back if the prompt fails before the new branch has an entry.
   *
   * Resolves when the run it starts ends. `taken` is called before that, once pi has the
   * message: its run started, it is queued, or it was a command, which has run by then. A
   * throw before it means pi refused the message and its session has nothing of it; one
   * after it is the run's. (Pi takes a prompt that comes in while it settles a run without
   * telling, and this resolves at once: not called then.)
   */
  async prompt(
    threadId: string,
    text: string,
    mode: "prompt" | "steer" | "follow_up",
    model?: string,
    thinkingLevel?: string,
    attachments: Attachment[] = [],
    title?: TitleGen,
    tools?: string[],
    edit?: string,
    taken: () => void = () => {},
  ): Promise<void> {
    const live = this.live.get(threadId);
    if (!live) throw new Error(`thread not loaded: ${threadId}`);
    await live.nixEnv;
    // Unloaded while its dev shell was captured: the session is disposed.
    if (this.live.get(threadId) !== live) throw new Error("the thread was unloaded");
    const { session } = live;
    const switchTo =
      model && model !== `${session.model?.provider}/${session.model?.id}`
        ? await this.resolveModel(model)
        : undefined;
    const undo = edit ? rewind(session, edit) : undefined;
    try {
      await this.deliver(
        live,
        threadId,
        text,
        mode,
        switchTo,
        thinkingLevel,
        attachments,
        title,
        tools,
        taken,
      );
    } catch (e) {
      undo?.();
      throw e;
    }
  }

  /** `prompt` with the model resolved (`switchTo`, if it changes) and an edit's rewind done. */
  private async deliver(
    live: Live,
    threadId: string,
    text: string,
    mode: "prompt" | "steer" | "follow_up",
    switchTo: Model<Api> | undefined,
    thinkingLevel: string | undefined,
    attachments: Attachment[],
    title: TitleGen | undefined,
    tools: string[] | undefined,
    taken: () => void,
  ): Promise<void> {
    const { session } = live;
    if (switchTo) await session.setModel(switchTo);
    if (thinkingLevel && thinkingLevel !== session.thinkingLevel) {
      session.setThinkingLevel(thinkingLevel as ThinkingLevel);
    }
    if (tools) applyTools(live, tools);
    // The thread is named once pi has the message (a refused one may leave no thread to
    // name), from the text as typed.
    const typed = text;
    const took = () => {
      taken();
      if (!title) return;
      this.generateTitle(title, typed, session.model)
        .then((r) => session.setSessionName(r.title))
        .catch((e) =>
          this.send({
            type: "warning",
            thread_id: threadId,
            message: `title generation failed: ${(e as Error).message}`,
          }),
        );
    };
    const images: ImageContent[] = attachments.map((a) => ({
      type: "image",
      data: a.data,
      mimeType: a.mime_type,
    }));
    if (attachments.length > 0) {
      try {
        text = linkAttachments(text, attachments);
      } catch (e) {
        this.send({
          type: "warning",
          thread_id: threadId,
          message: `saving attached images failed: ${(e as Error).message}`,
        });
      }
    }
    // A steer or follow-up only queues in pi: on an idle model (the sender's view was
    // stale: the run ended, or the service restarted under it) it would sit there until
    // some later turn delivers it, so it is a prompt instead.
    if (mode === "steer" && session.isStreaming) {
      // The model is meant to see it now, not after a `wait` times out. (The user's, or a
      // parent's `dirigent send`: hence no who.)
      if ((await session.steer(text, images)) === "queued") wake(live, "a message came in");
      took();
    } else if (mode === "follow_up" && session.isStreaming) {
      await session.followUp(text, images);
      took();
    } else {
      // An extension command (`/mcp`) runs right here and starts no run, so nothing would
      // settle the run the service began for the message: it is settled here.
      let handled = false;
      live.takingIn = true;
      try {
        await session.prompt(text, {
          images,
          streamingBehavior: "followUp",
          preflightResult: (d) => {
            live.takingIn = false;
            handled = d === "handled";
            took();
          },
        });
      } finally {
        live.takingIn = false;
      }
      if (handled && !session.isStreaming)
        this.send({ type: "event", thread_id: threadId, event: { type: "agent_settled" } });
    }
  }

  /**
   * The user stopped the run. The jobs the model waits on go with it: left running, the
   * end of one would start a new turn, and the user would have to stop it again.
   */
  async abort(threadId: string): Promise<void> {
    const live = this.live.get(threadId);
    if (!live) return;
    killJobs(live, true);
    await live.session.abort();
  }

  /** Drop one queued steer or follow-up by text. Pi only clears whole queues, so
   *  take both, remove the first match, and queue the rest again in order, images and
   *  all. */
  async cancelQueued(threadId: string, text: string): Promise<void> {
    const live = this.live.get(threadId);
    if (!live) return;
    const { session } = live;
    const { steering, followUp } = queuedMessages(session);
    session.clearQueue();
    const drop = (list: Queued[]) => {
      // The desktop's echo of a steer lacks the image links pi's text has until pi's
      // `queue_update` replaces it (`linkAttachments`).
      const i = list.findIndex((m) => m.text === text || m.text.startsWith(`${text}\n\n[image-`));
      if (i !== -1) list.splice(i, 1);
      return i !== -1;
    };
    if (!drop(steering)) drop(followUp);
    for (const m of steering) await session.steer(m.text, m.images);
    for (const m of followUp) await session.followUp(m.text, m.images);
    await requeueNotices(live);
  }

  /** The service has a notice for the model (a sub-agent ended). */
  async notify(
    threadId: string,
    customType: string,
    content: string,
    details: Record<string, unknown>,
    notify: Notify,
  ): Promise<void> {
    const live = this.live.get(threadId);
    if (!live) throw new Error(`thread not loaded: ${threadId}`);
    await live.nixEnv;
    await deliverNotice(live, customType, content, details, notify);
  }

  /** The user answered (or dismissed: `answers == null`) an `ask_user` call; tell the model. */
  async answer(threadId: string, ask: Ask, answers: string[][] | null): Promise<void> {
    const live = this.live.get(threadId);
    if (!live) throw new Error(`thread not loaded: ${threadId}`);
    await live.nixEnv;
    await deliverAnswers(live, ask, answers);
  }

  /**
   * One completion: `gen.prompt` as the system prompt, `text` between `<message>` tags as
   * the user turn (so it reads as data to name, not as a request; the default prompt says
   * so). Uses `gen.model` if this host has credentials for it, else the first available
   * fallback, else `fallback` (the thread's own model). The title is the reply's first line
   * (`cleanTitle`); a reply that is no title throws, so the thread keeps its placeholder.
   */
  async generateTitle(gen: TitleGen, text: string, fallback?: Model<Api>): Promise<TitleResult> {
    const rt = await this.modelRuntime;
    const available = await rt.getAvailable();
    const find = (key: string) => available.find((m) => `${m.provider}/${m.id}` === key);
    const model = find(gen.model) ?? TITLE_FALLBACKS.map(find).find((m) => m) ?? fallback;
    if (!model) throw new Error(`no credentials for ${gen.model}`);
    const started = Date.now();
    const reply = await rt.completeSimple(
      model,
      {
        systemPrompt: gen.prompt,
        messages: [{ role: "user", content: `<message>\n${text}\n</message>`, timestamp: started }],
      },
      { reasoning: gen.reasoning as ThinkingLevel },
    );
    if (reply.stopReason === "error" || reply.stopReason === "aborted") {
      throw new Error(reply.errorMessage ?? reply.stopReason);
    }
    const raw = reply.content
      .filter((c): c is { type: "text"; text: string } => c.type === "text")
      .map((c) => c.text)
      .join("\n");
    const title = cleanTitle(raw);
    if (!title)
      throw new Error(`model replied with no title: ${JSON.stringify(raw.trim().slice(0, 200))}`);
    return {
      title,
      model: `${model.provider}/${model.id}`,
      input: reply.usage.input + reply.usage.cacheRead + reply.usage.cacheWrite,
      output: reply.usage.output,
      cost: reply.usage.cost.total,
      duration_ms: Date.now() - started,
    };
  }

  /** Unload every thread, waiting at most a few seconds for their extensions to stop. */
  async disposeAll(): Promise<void> {
    const ids = new Set([...this.live.keys(), ...this.loading.keys()]);
    const unloads = Array.from(ids, (id) => this.unload(id));
    await Promise.race([Promise.allSettled(unloads), Bun.sleep(3000)]);
  }
}

/**
 * Why a prompt was refused, as said to whoever sent it. Pi's words for missing or expired
 * credentials are for its terminal's user (`/login`, then paths into its docs): here the
 * way to sign in is the desktop's.
 */
export function refusal(e: unknown): string {
  // A string whatever was thrown: the service waits for these words.
  const message = String((e as Error | undefined)?.message ?? e);
  if (!/^(No API key|Authentication failed) /.test(message)) return message;
  const said = message
    .split("\n\n")[0]!
    .replace(/\s*Run '\/login[^']*' to re-authenticate\.$/, "")
    .replace(/\.$/, "");
  return `${said}. Sign in under Settings › Providers.`;
}

/** Longer than this, a reply is the model talking (commentary on its instructions), not a title. */
const TITLE_MAX_WORDS = 10;

/**
 * The title in a title model's reply: its first line without a `Title:` label, quotes or a
 * trailing period. Null when that is no title: empty, too long, the model speaking in the
 * first person (`I can't…`, `I'll treat…`) or leading in to something else (`Here it is:`).
 */
function cleanTitle(raw: string): string | null {
  const title = (raw.trim().split("\n")[0] ?? "")
    .replace(/^(thread )?title:\s*/i, "")
    .replace(/^["'“”`]+|["'“”`.]+$/g, "")
    .trim();
  const words = title.split(/\s+/).length;
  if (!title || words > TITLE_MAX_WORDS || /^I[\s'’]/.test(title) || title.endsWith(":"))
    return null;
  return title;
}

/**
 * Take the session back to just before the user message `edit`, for a prompt that rewrites
 * it (`EditMessage`): pi's leaf, the entry the next one is appended to, moves to the
 * message's parent, and the model's context becomes that branch. Nothing is persisted: the
 * prompt is the new branch's first entry and, as the last entry, the leaf on every reload;
 * the old branch stays in the history. Returns how to put the session back, for a prompt
 * refused before the new branch has an entry (pi checks the model's credentials before it
 * appends anything). Not pi's `navigateTree`, which does nothing when the message is the
 * leaf itself (its run died before a reply).
 */
function rewind(session: AgentSession, edit: string): () => void {
  if (session.isStreaming || session.isCompacting)
    throw new Error("the thread is running; stop it first");
  const sm = session.sessionManager;
  const entry = sm.getEntry(edit);
  if (entry?.type !== "message" || entry.message.role !== "user")
    throw new Error(`not a user message: ${edit}`);
  const [leaf, count] = [sm.getLeafId(), sm.getEntryCount()];
  const moveTo = (id: string | null) => {
    if (id) sm.branch(id);
    else sm.resetLeaf();
    session.refreshContext();
  };
  moveTo(entry.parentId);
  return () => {
    if (sm.getEntryCount() === count) moveTo(leaf);
  };
}

/**
 * `session_shutdown` to pi's built-in extensions only: `dispose` alone leaves the MCP one's
 * servers running. Not to the others: pi runs one session per process, so an extension may
 * clean up the whole process on it (cliproxyapi closes every session's sockets, cutting off
 * other threads' replies). `dispose` closes this session's own (`cleanupSessionResources`).
 */
async function shutdownBuiltins(session: AgentSession): Promise<void> {
  const event = { type: "session_shutdown", reason: "quit" };
  const ctx = session.extensionRunner.createContext();
  for (const ext of session.resourceLoader.getExtensions().extensions) {
    if (!ext.path.startsWith("builtin:")) continue;
    for (const handler of ext.handlers.get("session_shutdown") ?? []) {
      await handler(event, ctx).catch((e) => console.error(`${ext.path} shutdown failed:`, e));
    }
  }
}

/**
 * Activate the dirigent tools in `tools` (and no other dirigent tool), leaving pi's own tools
 * as they are, and keep `tools` for the ones that read it (`ToolHost.tools`).
 *
 * `codemode` is pi's tool with a switch of ours: in `tools` it is activated, and it is
 * deactivated only when it leaves `tools` (the switch going off in a loaded thread; `create`
 * does it at a load). Not at every prompt without it: pi's MCP extension turns codemode on
 * at `session_start` for servers whose tools only scripts reach, and that has to last. The
 * switch going off mid-thread takes it from those servers too, until the thread's next load.
 */
function applyTools(live: Live, tools: string[]): void {
  const ours = new Set<string>(TOOL_NAMES);
  if (tools.includes(CODEMODE) || live.tools.includes(CODEMODE)) ours.add(CODEMODE);
  live.tools = tools;
  const session = live.session;
  const wanted = session
    .getActiveToolNames()
    .filter((n) => !ours.has(n))
    .concat(tools.filter((n) => ours.has(n)));
  const current = session.getActiveToolNames();
  if (wanted.length !== current.length || wanted.some((n) => !current.includes(n))) {
    session.setActiveToolsByName(wanted);
  }
}

/** A queued steer / follow-up: its text as pi lists it (`queue_update`), and its images. */
interface Queued {
  text: string;
  images: ImageContent[];
}

/**
 * The user messages pi has queued and not yet handed to the model, for `cancelQueued`:
 * `clearQueue` hands back their text alone and drops the messages, images and all. Read
 * from pi's agent queues, which are private: should pi rename them, this throws before
 * anything is cleared. A message the agent already took for its next turn is not in them,
 * so it isn't queued twice.
 */
function queuedMessages(session: AgentSession): Record<"steering" | "followUp", Queued[]> {
  const agent = session.agent as unknown as Record<
    "steeringQueue" | "followUpQueue",
    { messages: Message[] }
  >;
  // Custom messages (notices) sit in the same queues; `requeueNotices` handles those.
  const users = (queue: Message[]) =>
    queue
      .filter((m): m is UserMessage => m.role === "user")
      .map((m) => ({
        text: contentText(m.content, ""),
        images:
          typeof m.content === "string"
            ? []
            : m.content.filter((c): c is ImageContent => c.type === "image"),
      }));
  return {
    steering: users(agent.steeringQueue.messages),
    followUp: users(agent.followUpQueue.messages),
  };
}

// `message_update` carries the whole partial message on every delta; the client
// only needs the delta (it gets the full message in `message_end`).
function slim(event: AgentSessionEvent): unknown {
  if (event.type !== "message_update") return event;
  const { partial: _, ...ev } = event.assistantMessageEvent as { partial?: unknown };
  return { type: "message_update", assistantMessageEvent: ev };
}

// `nix develop <cwd> --command env -0`, parsed into a map. Failures are warnings:
// the thread keeps working with the plain environment.
async function captureNixEnv(
  cwd: string,
  threadId: string,
  send: (msg: Outbound) => void,
): Promise<Record<string, string> | undefined> {
  if (!Bun.which("nix")) {
    send({
      type: "warning",
      thread_id: threadId,
      message: "flake.nix found but nix is not on PATH",
    });
    return undefined;
  }
  try {
    const proc = Bun.spawn(["nix", "develop", cwd, "--command", "env", "-0"], {
      cwd,
      stdout: "pipe",
      stderr: "pipe",
    });
    const [out, err, code] = await Promise.all([
      new Response(proc.stdout).text(),
      new Response(proc.stderr).text(),
      proc.exited,
    ]);
    if (code !== 0) throw new Error(err.trim().split("\n").pop() ?? `exit ${code}`);
    const env: Record<string, string> = {};
    for (const kv of out.split("\0")) {
      const i = kv.indexOf("=");
      if (i > 0) env[kv.slice(0, i)] = kv.slice(i + 1);
    }
    return env;
  } catch (e) {
    send({
      type: "warning",
      thread_id: threadId,
      message: `nix develop failed: ${(e as Error).message}`,
    });
    return undefined;
  }
}

/**
 * `rt` with the global extensions' providers (cliproxyapi…) registered. Pi registers them
 * when it loads extensions, which it otherwise does only per session: without this, models
 * and Settings › Providers would lack them until a thread opened. Each session loads its
 * own extensions again and re-registers over these. A failing extension only costs its
 * provider (stderr), never the runtime.
 */
async function withExtensionProviders(rt: Promise<ModelRuntime>): Promise<ModelRuntime> {
  const modelRuntime = await rt;
  try {
    const { diagnostics } = await createAgentSessionServices({
      cwd: getAgentDir(),
      modelRuntime,
    });
    for (const d of diagnostics) if (d.type === "error") console.error(d.message);
  } catch (e) {
    console.error("loading extension providers failed:", e);
  }
  return modelRuntime;
}
