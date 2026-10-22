// The dirigent SDK: a ws client of a `dirigent_service` (the `dirigent_protocol` messages,
// typed loosely here) and `Dirigent`, the orchestration helpers on top of it. Used by the
// `dirigent` tool (a thread acting on its host) and by workflow scripts (`workflow.ts`),
// which also report every helper call as a step of their run.

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
  run_id: string | null;
  snoozed: boolean;
  /** When it was last given a task (a prompt that started a turn); orders a parent's children. */
  prompted_at: number | null;
}

export interface Workflow {
  name: string;
  path: string;
  description: string;
  input: { name: string; kind: string; description: string; default?: unknown }[];
  error: string | null;
}

export interface WorkflowRun {
  id: string;
  project_id: string | null;
  name: string;
  input: Record<string, unknown>;
  parent_id: string | null;
  started_at: number;
  ended_at: number | null;
  exit_code: number | null;
  steps: number;
  /** What the script does when run against placeholders (`Dirigent.plan`), step by step. */
  plan: Record<string, unknown>[];
}

export interface SearchHit {
  thread_id: string;
  snippet: string;
}

type Entry = { seq: number; kind: string; json: any };
type Reply = { type: string; reply_to?: number | null; [k: string]: any };

/** One ws connection: numbered requests matched to their replies, pushes to listeners. */
export class Client {
  private next = 1;
  private pending = new Map<number, { resolve: (r: Reply) => void; reject: (e: Error) => void }>();
  private listeners = new Set<(msg: Reply) => void>();
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
    if (typeof msg.reply_to === "number") {
      const p = this.pending.get(msg.reply_to);
      if (p) {
        this.pending.delete(msg.reply_to);
        if (msg.type === "error") p.reject(new Error(msg.message));
        else p.resolve(msg);
        return;
      }
    }
    for (const l of this.listeners) l(msg);
  }

  /** Send a request; resolves with its reply, rejects on an `error` reply. */
  request<T = Reply>(msg: Record<string, unknown>): Promise<T & Reply> {
    const id = this.next++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (r: Reply) => void, reject });
      this.ws.send(JSON.stringify({ ...msg, id }));
    });
  }

  /** Listen to pushes (and replies nobody waited for); returns the unsubscribe. */
  on(fn: (msg: Reply) => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  close(): void {
    this.ws.close();
  }
}

/** Who is calling: the thread (tool) or the run (workflow) acting on the host. */
export interface Self {
  threadId?: string;
  runId?: string;
  projectId?: string;
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
  /** Project to spawn in; defaults to the caller's. */
  projectId?: string;
}

/** A plan can't go on forever: a loop that never sees real data stops here. */
const PLAN_LIMIT = 200;

/**
 * Orchestration over a `Client`. Every call that changes something is reported to the
 * run through `report` (a workflow's trace); the tool passes none.
 *
 * `Dirigent.plan` makes one with no client at all: every call is recorded and answered
 * with a placeholder (fake thread ids, empty replies, instant waits), so running a
 * workflow against it yields the path it will take. That plan is what the desktop
 * shows before the real run fills it in. Branches on real results take the empty
 * path; the real run's steps replace the plan's as they land.
 */
export class Dirigent {
  private planned = 0;

  constructor(
    readonly client: Client | null,
    readonly self: Self,
    private readonly report?: (step: Record<string, unknown>) => Promise<void>,
  ) {}

  static plan(self: Self, report: (step: Record<string, unknown>) => Promise<void>): Dirigent {
    return new Dirigent(null, self, report);
  }

  get planning(): boolean {
    return this.client === null;
  }

  private async step(step: Record<string, unknown>): Promise<void> {
    if (this.planning && ++this.planned > PLAN_LIMIT)
      throw new Error(`plan exceeds ${PLAN_LIMIT} steps`);
    if (this.report) await this.report(step);
  }

  private req<T = Reply>(msg: Record<string, unknown>): Promise<T & Reply> {
    if (!this.client) throw new Error("not connected (planning)");
    return this.client.request<T>(msg);
  }

  /** A stand-in thread for a plan. */
  private fake(id: string): ThreadSummary {
    return {
      id,
      project_id: this.self.projectId ?? null,
      cwd: "",
      name: null,
      model: null,
      thinking_level: null,
      created_at: 0,
      updated_at: 0,
      entry_count: 0,
      running_since: null,
      last_run_ms: null,
      archived: false,
      asks: [],
      workspace: null,
      workspace_missing: false,
      parent_id: this.self.threadId ?? null,
      run_id: this.self.runId ?? null,
      snoozed: false,
      prompted_at: null,
    };
  }

  async threads(): Promise<ThreadSummary[]> {
    if (this.planning) return [];
    const r = await this.req<{ threads: ThreadSummary[] }>({ type: "list_threads" });
    return r.threads;
  }

  async thread(id: string): Promise<ThreadSummary | undefined> {
    return (await this.threads()).find((t) => t.id === id);
  }

  /** The caller's own thread, when it is one. */
  async me(): Promise<ThreadSummary | undefined> {
    return this.self.threadId ? this.thread(this.self.threadId) : undefined;
  }

  /** Create a sub-agent thread called `name` and send it `task`. Returns the new thread's id. */
  async spawn(opts: SpawnOptions): Promise<string> {
    if (this.planning) {
      const id = `plan-${this.planned + 1}`;
      await this.step({
        kind: "spawn",
        thread_id: id,
        name: opts.name,
        task: head(opts.task),
        model: opts.model ?? null,
      });
      return id;
    }
    const me = await this.me();
    const projectId = opts.projectId ?? this.self.projectId ?? me?.project_id;
    if (!projectId) throw new Error("no project to spawn in (pass projectId)");
    const created = await this.req<{ thread: ThreadSummary }>({
      type: "create_thread",
      project_id: projectId,
      workspace: opts.workspace ?? false,
      base: opts.base ?? null,
      parent: this.self.threadId ?? null,
      run: this.self.runId ?? null,
      name: opts.name,
    });
    const id = created.thread.id;
    const from = this.self.threadId
      ? `thread ${this.self.threadId}`
      : `workflow run ${this.self.runId ?? "?"}`;
    const text = `${opts.task.trim()}\n\n---\nYou are a sub-agent (thread ${id}) spawned by ${from}. When you finish, your final reply is reported to it, so make that reply a complete, self-contained summary of what you did, found and left undone.`;
    await this.req({
      type: "prompt",
      thread_id: id,
      text,
      mode: "prompt",
      model: opts.model ?? me?.model ?? null,
      thinking_level: opts.effort ?? me?.thinking_level ?? null,
      images: [],
      title: null,
      tools: null,
    });
    await this.step({
      kind: "spawn",
      thread_id: id,
      name: opts.name,
      task: head(opts.task),
      model: opts.model ?? me?.model ?? null,
    });
    return id;
  }

  /** Message a thread: a new turn if it is idle, a steer (default) or follow-up if it runs. */
  async send(threadId: string, text: string, mode: "steer" | "follow_up" = "steer"): Promise<void> {
    if (this.planning) {
      await this.step({ kind: "send", thread_id: threadId, mode, text: head(text) });
      return;
    }
    const t = await this.thread(threadId);
    if (!t) throw new Error(`no thread ${threadId}`);
    const actual = t.running_since ? mode : "prompt";
    await this.req({
      type: "prompt",
      thread_id: threadId,
      text,
      mode: actual,
      model: null,
      thinking_level: null,
      images: [],
      title: null,
      tools: null,
    });
    await this.step({ kind: "send", thread_id: threadId, mode: actual, text: head(text) });
  }

  /** A thread's final reply (its last turn-ending message), or with `full` the whole conversation's text. */
  async read(
    threadId: string,
    full = false,
  ): Promise<{ thread: ThreadSummary; reply?: string; transcript?: string }> {
    if (this.planning) {
      await this.step({ kind: "read", thread_id: threadId });
      return { thread: this.fake(threadId), reply: "", transcript: full ? "" : undefined };
    }
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
    await this.step({ kind: "read", thread_id: threadId });
    return {
      thread,
      reply: reply ? text(reply) : undefined,
      transcript: full ? turns.map((m) => `${m.role}: ${text(m)}`).join("\n\n") : undefined,
    };
  }

  /** Threads whose messages contain every word of `query`, best match first. */
  async search(query: string, limit = 20): Promise<(SearchHit & { thread?: ThreadSummary })[]> {
    if (this.planning) {
      await this.step({ kind: "search", query });
      return [];
    }
    const [r, threads] = await Promise.all([
      this.req<{ hits: SearchHit[] }>({ type: "search_threads", query, limit }),
      this.threads(),
    ]);
    return r.hits.map((h) => ({ ...h, thread: threads.find((t) => t.id === h.thread_id) }));
  }

  /**
   * Resolve once every thread in `ids` is idle (its run settled), with their summaries.
   * Rejects after `timeoutMs`, if given.
   */
  async wait(ids: string | string[], timeoutMs?: number): Promise<ThreadSummary[]> {
    const wanted = Array.isArray(ids) ? ids : [ids];
    await this.step({ kind: "wait", thread_ids: wanted });
    if (this.planning) {
      await this.step({ kind: "done", thread_ids: wanted, names: wanted.map(() => null) });
      return wanted.map((id) => this.fake(id));
    }
    const settled = (threads: ThreadSummary[]) =>
      wanted.every((id) => threads.find((t) => t.id === id)?.running_since == null);
    let threads = await this.threads();
    if (!settled(threads)) {
      threads = await new Promise<ThreadSummary[]>((resolve, reject) => {
        const timer = timeoutMs
          ? setTimeout(() => {
              off();
              reject(new Error(`timed out waiting for ${wanted.join(", ")}`));
            }, timeoutMs)
          : undefined;
        const off = this.client!.on((msg) => {
          if (msg.type !== "threads" || !settled(msg.threads)) return;
          off();
          clearTimeout(timer);
          resolve(msg.threads);
        });
      });
    }
    const got = threads.filter((t) => wanted.includes(t.id));
    await this.step({ kind: "done", thread_ids: wanted, names: got.map((t) => t.name) });
    return got;
  }

  async archive(threadId: string, archived = true): Promise<void> {
    if (!this.planning) await this.req({ type: "archive_thread", thread_id: threadId, archived });
    await this.step({ kind: "archive", thread_id: threadId, archived });
  }

  async snooze(threadId: string, snoozed = true): Promise<void> {
    if (!this.planning) await this.req({ type: "snooze_thread", thread_id: threadId, snoozed });
    await this.step({ kind: "snooze", thread_id: threadId, snoozed });
  }

  async abort(threadId: string): Promise<void> {
    if (!this.planning) await this.req({ type: "abort", thread_id: threadId });
    await this.step({ kind: "abort", thread_id: threadId });
  }

  /** Open the thread in the user's desktop. */
  async open(threadId: string): Promise<void> {
    if (!this.planning) await this.req({ type: "focus_thread", thread_id: threadId });
    await this.step({ kind: "open", thread_id: threadId });
  }

  async workflows(projectId?: string): Promise<Workflow[]> {
    if (this.planning) return [];
    const pid = projectId ?? this.self.projectId ?? (await this.me())?.project_id;
    if (!pid) throw new Error("no project (pass projectId)");
    const r = await this.req<{ workflows: Workflow[] }>({
      type: "list_workflows",
      project_id: pid,
    });
    return r.workflows;
  }

  /** Start a workflow of the project; the caller is told when it ends. */
  async runWorkflow(
    name: string,
    input: Record<string, unknown> = {},
    projectId?: string,
  ): Promise<WorkflowRun> {
    if (this.planning) {
      const id = `plan-run-${this.planned + 1}`;
      await this.step({ kind: "workflow", run_id: id, name });
      return fakeRun(id, name, input);
    }
    const pid = projectId ?? this.self.projectId ?? (await this.me())?.project_id;
    if (!pid) throw new Error("no project (pass projectId)");
    const r = await this.req<{ run: WorkflowRun }>({
      type: "run_workflow",
      project_id: pid,
      name,
      input,
      parent: this.self.threadId ?? null,
    });
    await this.step({ kind: "workflow", run_id: r.run.id, name });
    return r.run;
  }

  async runs(): Promise<WorkflowRun[]> {
    if (this.planning) return [];
    return (await this.req<{ runs: WorkflowRun[] }>({ type: "list_runs" })).runs;
  }

  /** Resolve once the run's process has ended. */
  async waitRun(runId: string, timeoutMs?: number): Promise<WorkflowRun> {
    await this.step({ kind: "wait_run", run_id: runId });
    if (this.planning) return fakeRun(runId, "", {});
    const find = (runs: WorkflowRun[]) => runs.find((r) => r.id === runId);
    const now = find(await this.runs());
    if (!now) throw new Error(`no run ${runId}`);
    if (now.ended_at != null) return now;
    return new Promise((resolve, reject) => {
      const timer = timeoutMs
        ? setTimeout(() => {
            off();
            reject(new Error(`timed out waiting for run ${runId}`));
          }, timeoutMs)
        : undefined;
      const off = this.client!.on((msg) => {
        const r = msg.type === "runs" ? find(msg.runs) : undefined;
        if (!r || r.ended_at == null) return;
        off();
        clearTimeout(timer);
        resolve(r);
      });
    });
  }

  /** A line in the run's trace (and on stdout). */
  async log(text: string): Promise<void> {
    if (!this.planning) console.log(text);
    await this.step({ kind: "log", text });
  }
}

function fakeRun(id: string, name: string, input: Record<string, unknown>): WorkflowRun {
  return {
    id,
    project_id: null,
    name,
    input,
    parent_id: null,
    started_at: 0,
    ended_at: 0,
    exit_code: 0,
    steps: 0,
    plan: [],
  };
}

/** The first line, at most 120 chars. */
export function head(text: string): string {
  const line = text.trim().split("\n")[0] ?? "";
  return line.length > 120 ? `${line.slice(0, 120)}…` : line;
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
  const parent = t.parent_id ? ` parent=${t.parent_id}` : "";
  return `- ${t.id} "${t.name ?? "untitled"}" [${status}] model=${t.model ?? "?"}${parent} cwd=${t.cwd}`;
}
