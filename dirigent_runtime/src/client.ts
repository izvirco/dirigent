// A ws client of a `dirigent_service` (the `dirigent_protocol` messages, typed loosely
// here) and `Dirigent`, the orchestration helpers on top of it. Used by the `dirigent`
// tool: a thread acting on its host.

export interface ThreadSummary {
  id: string;
  project_id: string | null;
  cwd: string;
  name: string | null;
  model: string | null;
  thinking_level: string | null;
  created_at: number;
  updated_at: number;
  entry_count: number;
  running_since: number | null;
  last_run_ms: number | null;
  archived: boolean;
  asks: { id: string; questions: { text: string }[] }[];
  workspace: string | null;
  workspace_missing: boolean;
  parent_id: string | null;
  snoozed: boolean;
  /** When it was last given a task (a prompt that started a turn); orders a parent's children. */
  prompted_at: number | null;
  /** `async_bash` jobs running in its live session. */
  jobs: number;
}

export interface SearchHit {
  thread_id: string;
  snippet: string;
}

type Entry = { seq: number; kind: string; json: any };
type Reply = { type: string; reply_to?: number | null; [k: string]: any };

/** One ws connection: numbered requests matched to their replies; pushes are ignored. */
export class Client {
  private next = 1;
  private pending = new Map<number, { resolve: (r: Reply) => void; reject: (e: Error) => void }>();
  private closed = false;

  private constructor(private ws: WebSocket) {}

  static connect(url = process.env.DIRIGENT_URL): Promise<Client> {
    if (!url) throw new Error("DIRIGENT_URL is not set");
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(url);
      const client = new Client(ws);
      ws.addEventListener("open", () => resolve(client));
      ws.addEventListener("error", () => reject(new Error(`cannot reach ${url}`)));
      ws.addEventListener("message", (ev) => client.receive(String(ev.data)));
      ws.addEventListener("close", () => {
        client.closed = true;
        for (const p of client.pending.values()) p.reject(new Error("connection closed"));
        client.pending.clear();
      });
    });
  }

  get alive(): boolean {
    return !this.closed;
  }

  private receive(text: string): void {
    let msg: Reply;
    try {
      msg = JSON.parse(text);
    } catch {
      return;
    }
    if (typeof msg.reply_to !== "number") return;
    const p = this.pending.get(msg.reply_to);
    if (!p) return;
    this.pending.delete(msg.reply_to);
    if (msg.type === "error") p.reject(new Error(msg.message));
    else p.resolve(msg);
  }

  /** Send a request; resolves with its reply, rejects on an `error` reply. */
  request<T = Reply>(msg: Record<string, unknown>): Promise<T & Reply> {
    const id = this.next++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (r: Reply) => void, reject });
      this.ws.send(JSON.stringify({ ...msg, id }));
    });
  }
}

/** Who is calling: the thread acting on the host. */
export interface Self {
  threadId: string;
}

export interface SpawnOptions {
  /** The thread's name, a few words on what it is for. */
  name: string;
  /** The sub-agent's prompt. */
  task: string;
  /** `provider/id`; defaults to the caller's model. */
  model?: string;
  /** Thinking level; defaults to the caller's. */
  effort?: string;
  /** Its own git worktree / jj workspace of the project (from `base`) instead of the caller's directory. */
  workspace?: boolean;
  base?: string;
}

/** Orchestration helpers over a `Client`, acting as the calling thread. */
export class Dirigent {
  constructor(
    readonly client: Client,
    readonly self: Self,
  ) {}

  private req<T = Reply>(msg: Record<string, unknown>): Promise<T & Reply> {
    return this.client.request<T>(msg);
  }

  async threads(): Promise<ThreadSummary[]> {
    const r = await this.req<{ threads: ThreadSummary[] }>({ type: "list_threads" });
    return r.threads;
  }

  async thread(id: string): Promise<ThreadSummary | undefined> {
    return (await this.threads()).find((t) => t.id === id);
  }

  /** Create a sub-agent thread called `name` and send it `task`. Returns the new thread's id. */
  async spawn(opts: SpawnOptions): Promise<string> {
    const me = await this.thread(this.self.threadId);
    if (!me?.project_id) throw new Error("no project to spawn in");
    const created = await this.req<{ thread: ThreadSummary }>({
      type: "create_thread",
      project_id: me.project_id,
      workspace: opts.workspace ?? false,
      base: opts.base ?? null,
      parent: this.self.threadId,
      name: opts.name,
    });
    const id = created.thread.id;
    const text = `${opts.task.trim()}\n\n---\nYou are a sub-agent (thread ${id}) spawned by thread ${this.self.threadId}. When you finish, your final reply is reported to it, so make that reply a complete, self-contained summary of what you did, found and left undone.`;
    await this.req({
      type: "prompt",
      thread_id: id,
      text,
      mode: "prompt",
      model: opts.model ?? me.model ?? null,
      thinking_level: opts.effort ?? me.thinking_level ?? null,
      images: [],
      title: null,
      tools: null,
    });
    return id;
  }

  /** Message a thread: a new turn if it is idle, a steer (default) or follow-up if it runs. */
  async send(threadId: string, text: string, mode: "steer" | "follow_up" = "steer"): Promise<void> {
    const t = await this.thread(threadId);
    if (!t) throw new Error(`no thread ${threadId}`);
    await this.req({
      type: "prompt",
      thread_id: threadId,
      text,
      mode: t.running_since ? mode : "prompt",
      model: null,
      thinking_level: null,
      images: [],
      title: null,
      tools: null,
    });
  }

  /** A thread's final reply (its last turn-ending message), or with `full` the whole conversation's text. */
  async read(
    threadId: string,
    full = false,
  ): Promise<{ thread: ThreadSummary; reply?: string; transcript?: string }> {
    const thread = await this.thread(threadId);
    if (!thread) throw new Error(`no thread ${threadId}`);
    const r = await this.req<{ entries: Entry[] }>({
      type: "get_thread",
      thread_id: threadId,
    });
    const turns = r.entries
      .filter((e) => e.kind === "message")
      .map((e) => e.json.message)
      .filter(
        (m) => m && (m.role === "user" || (m.role === "assistant" && m.stopReason !== "toolUse")),
      );
    const text = (m: any) =>
      typeof m.content === "string"
        ? m.content
        : (m.content ?? [])
            .filter((b: any) => b.type === "text")
            .map((b: any) => b.text)
            .join("\n");
    const reply = [...turns].reverse().find((m) => m.role === "assistant");
    return {
      thread,
      reply: reply ? text(reply) : undefined,
      transcript: full ? turns.map((m) => `${m.role}: ${text(m)}`).join("\n\n") : undefined,
    };
  }

  /** Threads whose messages contain every word of `query`, best match first. */
  async search(query: string, limit = 20): Promise<(SearchHit & { thread?: ThreadSummary })[]> {
    const [r, threads] = await Promise.all([
      this.req<{ hits: SearchHit[] }>({ type: "search_threads", query, limit }),
      this.threads(),
    ]);
    return r.hits.map((h) => ({ ...h, thread: threads.find((t) => t.id === h.thread_id) }));
  }

  async archive(threadId: string, archived = true): Promise<void> {
    await this.req({ type: "archive_thread", thread_id: threadId, archived });
  }

  async snooze(threadId: string, snoozed = true): Promise<void> {
    await this.req({ type: "snooze_thread", thread_id: threadId, snoozed });
  }

  async abort(threadId: string): Promise<void> {
    await this.req({ type: "abort", thread_id: threadId });
  }

  /** Open the thread in the user's desktop. */
  async open(threadId: string): Promise<void> {
    await this.req({ type: "focus_thread", thread_id: threadId });
  }
}

/** One line per thread for the model: status, name, id, model, project. */
export function describeThread(t: ThreadSummary, now = Date.now()): string {
  const status = t.running_since
    ? `working ${Math.round((now - t.running_since) / 1000)}s`
    : t.asks.length
      ? "asking the user"
      : t.archived
        ? "archived"
        : t.snoozed
          ? "snoozed"
          : "done";
  const jobs = t.jobs > 0 ? `, ${t.jobs} job${t.jobs === 1 ? "" : "s"} running` : "";
  const parent = t.parent_id ? ` parent=${t.parent_id}` : "";
  return `- ${t.id} "${t.name ?? "untitled"}" [${status}${jobs}] model=${t.model ?? "?"}${parent} cwd=${t.cwd}`;
}
