// Live pi sessions, keyed by thread id. Everything is in memory: the service
// owns persistence by writing every `entry` message we emit to sqlite.
//
// Pi persists messages into its SessionManager silently (no event), so after
// every session event we diff the manager's entry count against what we have
// already forwarded and emit the new tail as `entry` messages.

import { existsSync } from "node:fs";
import { join } from "node:path";
import {
  getSupportedThinkingLevels,
  type Api,
  type ImageContent,
  type Model,
  type ThinkingLevel,
} from "@earendil-works/pi-ai";
import {
  type AgentSession,
  type AgentSessionEvent,
  createAgentSession,
  createBashToolDefinition,
  DefaultResourceLoader,
  type FileEntry,
  getAgentDir,
  ModelRuntime,
  SessionManager,
  type ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import {
  type Ask,
  createTools,
  deliverAnswers,
  deliverNotice,
  killJobs,
  type Notify,
  noticeDelivered,
  requeueNotices,
  TOOL_NAMES,
  type ToolHost,
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

/** Tried in order when the configured title model has no credentials here. */
const TITLE_FALLBACKS = ["anthropic/claude-haiku-5-5", "openai/gpt-6-luna"];

/**
 * Appended to pi's system prompt. The service reads the linked file into its blobs and
 * rewrites the link to `blob:<id>` when it persists the reply (`blobs.rs`); the desktop
 * renders those inline.
 */
const IMAGES_PROMPT = `## Showing images
Your replies are rendered as markdown in a desktop app. To show the user an image file on this machine (a screenshot you took, a chart you rendered), put it in your reply as a markdown image with its path: \`![what it shows](/absolute/path.png)\`. It appears inline in the reply. png, jpg, gif, webp and bmp work; a relative path is taken from the working directory.`;

export type Outbound =
  | { type: "loaded"; thread_id: string; model?: string; thinking_level: string; header: unknown }
  | { type: "models"; models: ModelInfo[] }
  | ({ type: "title" } & TitleResult)
  | { type: "entry"; thread_id: string; entry: unknown }
  | { type: "event"; thread_id: string; event: unknown }
  | { type: "ask"; thread_id: string; ask: Ask }
  | { type: "jobs"; thread_id: string; running: number }
  | { type: "warning"; thread_id: string; message: string }
  | { type: "error"; thread_id?: string; message: string };

interface Live extends ToolHost {
  unsubscribe: () => void;
  // How many of sessionManager's entries have been emitted as `entry` messages.
  forwarded: number;
  // Resolved once per load. `undefined` = no flake / nix failed → plain env.
  nixEnv?: Promise<Record<string, string> | undefined>;
}

export class Sessions {
  private live = new Map<string, Live>();
  // Shared across sessions; also answers `list_models`.
  private modelRuntime = ModelRuntime.create();

  constructor(private send: (msg: Outbound) => void) {}

  has(threadId: string): boolean {
    return this.live.has(threadId);
  }

  /**
   * Create the pi session. `model` ("provider/id") and `thinkingLevel` only apply to a fresh
   * thread (no entries): pi persists them as its first entries, so the thread never shows pi's
   * default model. A thread with history restores its own from the entries. `tools` names
   * the dirigent tools (`TOOL_NAMES`) the model gets; the rest are registered but inactive.
   */
  async load(
    threadId: string,
    cwd: string,
    entries: FileEntry[],
    model?: string,
    thinkingLevel?: string,
    tools: string[] = [],
  ): Promise<void> {
    if (this.live.has(threadId)) return;
    const fresh = entries.length === 0;

    // The tools hold the session they run in; it exists only after `createAgentSession`.
    const live = {
      threadId,
      cwd,
      unsubscribe: () => {},
      forwarded: entries.filter((e) => e.type !== "session").length,
      jobs: new Map(),
      notices: new Map(),
      onAsk: (ask: Ask) => this.send({ type: "ask", thread_id: threadId, ask }),
      onJobs: (running: number) => this.send({ type: "jobs", thread_id: threadId, running }),
    } as Live;
    // Bash runs through our spawnHook so a captured nix dev-shell env can be injected.
    const bash = createBashToolDefinition(cwd, {
      spawnHook: (ctx) => (live.env ? { ...ctx, env: { ...ctx.env, ...live.env } } : ctx),
    });

    // Pi's default loader, plus our note on showing images.
    const resourceLoader = new DefaultResourceLoader({
      cwd,
      agentDir: getAgentDir(),
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
    });
    live.session = session;
    // Hand every queued steer to the model in the next turn, not one per turn. Set on the
    // agent directly: `session.setSteeringMode` would also write pi's global settings.
    session.agent.steeringMode = "all";
    applyTools(session, tools);

    // Pi appends to the manager right *after* notifying listeners, so flush on a microtask.
    live.unsubscribe = session.subscribe((event) => {
      if (event.type === "message_start" && event.message.role === "custom")
        noticeDelivered(live, event.message);
      this.send({ type: "event", thread_id: threadId, event: slim(event) });
      queueMicrotask(() => this.flushEntries(threadId, live));
    });
    if (existsSync(join(cwd, "flake.nix"))) {
      live.nixEnv = captureNixEnv(cwd, threadId, this.send).then((e) => (live.env = e));
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

  private flushEntries(threadId: string, live: Live): void {
    const sm = live.session.sessionManager;
    if (sm.getEntryCount() <= live.forwarded) return;
    const all = sm.getEntries();
    for (const entry of all.slice(live.forwarded))
      this.send({ type: "entry", thread_id: threadId, entry });
    live.forwarded = all.length;
  }

  unload(threadId: string): void {
    const live = this.live.get(threadId);
    if (!live) return;
    this.live.delete(threadId);
    live.unsubscribe();
    killJobs(live);
    this.flushEntries(threadId, live);
    live.session.dispose();
  }

  /** Models this host has credentials for. */
  async listModels(): Promise<void> {
    const rt = await this.modelRuntime;
    const models = (await rt.getAvailable()).map((m) => ({
      provider: m.provider,
      id: m.id,
      name: m.name,
      context_window: m.contextWindow,
      thinking_levels: getSupportedThinkingLevels(m),
    }));
    this.send({ type: "models", models });
  }

  /**
   * Send a prompt, first switching the session to `model` ("provider/id") and
   * `thinkingLevel` if they differ from its current ones. Pi persists the switch
   * as entries, so it survives reloads. `attachments` are base64 images; pi resizes
   * them to the model's limits before persisting. `title` (a desktop thread's first
   * prompt; sub-agents are named by their spawner) names the thread from `text`
   * alongside the run: pi persists the name as a `session_info` entry, which is how
   * the service and clients learn it. `tools`, when given, is the dirigent tool set
   * from now on (see `load`).
   */
  async prompt(
    threadId: string,
    text: string,
    mode: "prompt" | "steer" | "follow_up",
    model?: string,
    thinkingLevel?: string,
    attachments: { mime_type: string; data: string }[] = [],
    title?: TitleGen,
    tools?: string[],
  ): Promise<void> {
    const live = this.live.get(threadId);
    if (!live) throw new Error(`thread not loaded: ${threadId}`);
    await live.nixEnv;
    const { session } = live;
    if (model && model !== `${session.model?.provider}/${session.model?.id}`) {
      await session.setModel(await this.resolveModel(model));
    }
    if (thinkingLevel && thinkingLevel !== session.thinkingLevel) {
      session.setThinkingLevel(thinkingLevel as ThinkingLevel);
    }
    if (tools) applyTools(session, tools);
    if (title) {
      this.generateTitle(title, text, session.model)
        .then((r) => session.setSessionName(r.title))
        .catch((e) =>
          this.send({
            type: "warning",
            thread_id: threadId,
            message: `title generation failed: ${(e as Error).message}`,
          }),
        );
    }
    const images: ImageContent[] = attachments.map((a) => ({
      type: "image",
      data: a.data,
      mimeType: a.mime_type,
    }));
    if (mode === "steer") await session.steer(text, images);
    else if (mode === "follow_up") await session.followUp(text, images);
    else await session.prompt(text, { images, streamingBehavior: "followUp" });
  }

  async abort(threadId: string): Promise<void> {
    await this.live.get(threadId)?.session.abort();
  }

  /** Drop one queued steer or follow-up by text. Pi only clears whole queues, so
   *  take both, remove the first match, and queue the rest again in order. */
  async cancelQueued(threadId: string, text: string): Promise<void> {
    const live = this.live.get(threadId);
    if (!live) return;
    const { session } = live;
    const { steering, followUp } = session.clearQueue();
    const drop = (list: string[]) => {
      const i = list.indexOf(text);
      if (i !== -1) list.splice(i, 1);
      return i !== -1;
    };
    if (!drop(steering)) drop(followUp);
    for (const t of steering) await session.steer(t);
    for (const t of followUp) await session.followUp(t);
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
   * One completion: `gen.prompt` as the system prompt, `text` as the user message. Uses
   * `gen.model` if this host has credentials for it, else the first available fallback,
   * else `fallback` (the thread's own model). The title is the reply's first line, unquoted.
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
      { systemPrompt: gen.prompt, messages: [{ role: "user", content: text, timestamp: started }] },
      { reasoning: gen.reasoning as ThinkingLevel },
    );
    if (reply.stopReason === "error" || reply.stopReason === "aborted") {
      throw new Error(reply.errorMessage ?? reply.stopReason);
    }
    const raw = reply.content
      .filter((c): c is { type: "text"; text: string } => c.type === "text")
      .map((c) => c.text)
      .join("\n");
    const title = (raw.trim().split("\n")[0] ?? "").replace(/^["'“”`]+|["'“”`.]+$/g, "").trim();
    if (!title) throw new Error("model returned an empty title");
    return {
      title,
      model: `${model.provider}/${model.id}`,
      input: reply.usage.input + reply.usage.cacheRead + reply.usage.cacheWrite,
      output: reply.usage.output,
      cost: reply.usage.cost.total,
      duration_ms: Date.now() - started,
    };
  }

  disposeAll(): void {
    for (const id of Array.from(this.live.keys())) this.unload(id);
  }
}

/** Activate the dirigent tools in `tools` (and no other dirigent tool), leaving pi's own tools as they are. */
function applyTools(session: AgentSession, tools: string[]): void {
  const ours = new Set<string>(TOOL_NAMES);
  const wanted = session
    .getActiveToolNames()
    .filter((n) => !ours.has(n))
    .concat(tools.filter((n) => ours.has(n)));
  const current = session.getActiveToolNames();
  if (wanted.length !== current.length || wanted.some((n) => !current.includes(n))) {
    session.setActiveToolsByName(wanted);
  }
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
