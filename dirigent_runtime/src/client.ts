// A ws client of a `dirigent_service` (the `dirigent_protocol` messages, typed loosely
// here) and `Dirigent`, the orchestration helpers on top of it. Used by the `dirigent`
// tool: a thread acting on its host, and on the hosts that host reaches (`local`, its
// own service at `DIRIGENT_URL`, plus its `hosts.toml`, from `list_hosts`).

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

export interface Project {
  id: string;
  path: string;
  /** Normalized git origin: the same repo on another host has the same one. */
  origin: string | null;
  created_at: number;
}

export interface HostInfo {
  name: string;
  url: string;
}

export interface SearchHit {
  thread_id: string;
  snippet: string;
}

/** A thread found on some host, with its project (when it still has one) and a search snippet. */
export interface Found {
  host: HostInfo;
  thread: ThreadSummary;
  project?: Project;
  snippet?: string;
}

/** What `list_models` reports. */
interface ModelInfo {
  provider: string;
  id: string;
}

type Entry = { seq: number; kind: string; json: any };
type Reply = { type: string; reply_to?: number | null; [k: string]: any };

/** The host's own service, as its agents (and its desktop) call it. */
export const LOCAL = "local";
/** A host that doesn't answer a connect this fast is reported unreachable (an offline tailscale peer never refuses). */
const CONNECT_MS = 3000;
/** Every reply is quick (a `prompt` or `create_thread` only waits for a workspace and the session to load), so this is a dead host or a lost reply. */
const REQUEST_MS = 30_000;
const LINK = "dirigent://thread/";

/** A request that got no reply in time: it may still have happened. */
class TimeoutError extends Error {}

/** One ws connection: numbered requests matched to their replies; pushes are ignored. */
export class Client {
  private next = 1;
  private pending = new Map<number, { resolve: (r: Reply) => void; reject: (e: Error) => void }>();
  private closed = false;

  private constructor(private ws: WebSocket) {}

  static connect(url: string, timeoutMs = CONNECT_MS): Promise<Client> {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(url);
      const client = new Client(ws);
      const timer = setTimeout(() => {
        reject(new TimeoutError(`cannot reach ${url} (no answer in ${timeoutMs / 1000}s)`));
        ws.close();
      }, timeoutMs);
      ws.addEventListener("open", () => {
        clearTimeout(timer);
        resolve(client);
      });
      ws.addEventListener("error", () => {
        clearTimeout(timer);
        reject(new Error(`cannot reach ${url}`));
      });
      ws.addEventListener("message", (ev) => client.receive(String(ev.data)));
      ws.addEventListener("close", () => {
        client.closed = true;
        for (const p of client.pending.values()) p.reject(new Error("connection closed"));
        client.pending.clear();
      });
    });
  }

  close(): void {
    this.ws.close();
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

  /** Send a request; resolves with its reply, rejects on an `error` reply or after `REQUEST_MS`. */
  request<T = Reply>(msg: Record<string, unknown>): Promise<T & Reply> {
    if (this.closed) return Promise.reject(new Error("connection closed"));
    const id = this.next++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new TimeoutError(`no reply to ${msg.type} in ${REQUEST_MS / 1000}s`));
      }, REQUEST_MS);
      this.pending.set(id, {
        resolve: (r) => {
          clearTimeout(timer);
          resolve(r as T & Reply);
        },
        reject: (e) => {
          clearTimeout(timer);
          reject(e);
        },
      });
      this.ws.send(JSON.stringify({ ...msg, id }));
    });
  }
}

/** Who is calling: the thread acting on the host, and the dirigent tools it has on. */
export interface Self {
  threadId: string;
  tools: string[];
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
  /** What the caller hears when it ends: its reply (default) or only that it finished. */
  report?: "reply" | "done";
  /** When the caller hears it: after its current tool calls (default) or once its turn has ended. */
  notify?: "asap" | "when_idle";
}

export interface NewOptions {
  name: string;
  /** The thread's whole prompt, as a user would write it. */
  task: string;
  /** A host name (`local` or one from `hosts.toml`); default `local`. */
  host?: string;
  /** A project id or path on that host; default the caller's project (on another host: the one with its origin). */
  project?: string;
  model?: string;
  effort?: string;
  workspace?: boolean;
  base?: string;
}

/** The answer of each host to one request: its value, or why it had none. */
type Each<T> = { host: HostInfo; value?: T; error?: string };

/** Which host each thread lives on, as learned; threads never move, so it can't go stale. */
const owners = new Map<string, HostInfo>();

/**
 * Orchestration helpers, acting as the calling thread. Connects to hosts as it needs them;
 * make one per tool call and `close` it: an open connection receives every push of its
 * host, which nobody here reads.
 */
export class Dirigent {
  private clients = new Map<string, Promise<Client>>();
  private hostList?: Promise<HostInfo[]>;
  private hostsError?: string;
  private projectLists = new Map<string, Promise<Project[]>>();
  readonly local: HostInfo;

  constructor(readonly self: Self) {
    const url = process.env.DIRIGENT_URL;
    if (!url) throw new Error("DIRIGENT_URL is not set");
    this.local = { name: LOCAL, url };
  }

  close(): void {
    for (const c of this.clients.values()) c.then((c) => c.close()).catch(() => {});
    this.clients.clear();
  }

  private req<T = Reply>(host: HostInfo, msg: Record<string, unknown>): Promise<T & Reply> {
    let client = this.clients.get(host.url);
    if (!client) {
      client = Client.connect(host.url);
      this.clients.set(host.url, client);
    }
    return client.then((c) => c.request<T>(msg));
  }

  /**
   * `local` first, then the other hosts its `hosts.toml` lists. A broken file leaves just
   * `local` (`hostsError` says why, and `each` reports it), so it never blocks work here.
   */
  hosts(): Promise<HostInfo[]> {
    this.hostList ??= this.req<{ hosts: HostInfo[] }>(this.local, { type: "list_hosts" }).then(
      (r) => [this.local, ...r.hosts.filter((h) => h.url !== this.local.url)],
      (e) => {
        this.hostsError = (e as Error).message;
        return [this.local];
      },
    );
    return this.hostList;
  }

  async host(name: string = LOCAL): Promise<HostInfo> {
    const hosts = await this.hosts();
    const h = hosts.find((h) => h.name === name);
    if (!h) throw new Error(`no host "${name}"; hosts: ${hosts.map((h) => h.name).join(", ")}`);
    return h;
  }

  /** `fn` on every host at once; a host that fails is reported, not fatal. */
  private async each<T>(fn: (h: HostInfo) => Promise<T>): Promise<Each<T>[]> {
    const answers: Each<T>[] = await Promise.all(
      (await this.hosts()).map((host) =>
        fn(host).then(
          (value) => ({ host, value }),
          (e) => ({ host, error: (e as Error).message }),
        ),
      ),
    );
    if (this.hostsError)
      answers.push({ host: { name: "hosts.toml", url: "" }, error: this.hostsError });
    return answers;
  }

  async threads(host = this.local): Promise<ThreadSummary[]> {
    const r = await this.req<{ threads: ThreadSummary[] }>(host, { type: "list_threads" });
    for (const t of r.threads) owners.set(t.id, host);
    return r.threads;
  }

  projects(host = this.local): Promise<Project[]> {
    let p = this.projectLists.get(host.url);
    if (!p) {
      p = this.req<{ projects: Project[] }>(host, { type: "list_projects" }).then(
        (r) => r.projects,
      );
      this.projectLists.set(host.url, p);
    }
    return p;
  }

  /** `t` with its host and project. */
  private async found(host: HostInfo, t: ThreadSummary, snippet?: string): Promise<Found> {
    const projects = await this.projects(host).catch(() => []);
    return { host, thread: t, project: projects.find((p) => p.id === t.project_id), snippet };
  }

  /** The thread with `id` (or its `dirigent://thread/` link) and the host it lives on. Every host is asked at once; the first that has it answers. */
  async locate(id: string): Promise<Found> {
    id = threadId(id);
    const known = owners.get(id);
    if (known) {
      const t = (await this.threads(known)).find((t) => t.id === id);
      if (t) return this.found(known, t);
      owners.delete(id);
    }
    const hosts = await this.hosts();
    const hit = await new Promise<{ host: HostInfo; t: ThreadSummary } | string[]>((resolve) => {
      const errors: string[] = [];
      let left = hosts.length;
      for (const host of hosts)
        this.threads(host)
          .then(
            (ts) => {
              const t = ts.find((t) => t.id === id);
              if (t) resolve({ host, t });
            },
            (e) => errors.push(`${host.name}: ${(e as Error).message}`),
          )
          .finally(() => --left === 0 && resolve(errors));
    });
    if (Array.isArray(hit))
      throw new Error(`no thread ${id}${hit.length ? ` (not asked: ${hit.join("; ")})` : ""}`);
    return this.found(hit.host, hit.t);
  }

  /** Create a sub-agent thread called `name` here and send it `task`. Returns the new thread's id. */
  async spawn(opts: SpawnOptions): Promise<string> {
    const me = await this.me();
    if (!me.project_id) throw new Error("no project to spawn in");
    const model = opts.model ?? me.model;
    if (opts.model) await this.checkModel(this.local, opts.model);
    const tail =
      opts.report === "done"
        ? "When you finish, it is told that you did and may read your final reply"
        : "When you finish, your final reply is reported to it";
    // Picked here so the prompt, which goes with the create, can tell the child its id.
    const id = Bun.randomUUIDv7();
    const text = `${opts.task.trim()}\n\n---\nYou are a sub-agent (thread ${id}) spawned by thread ${this.self.threadId}. ${tail}, so make that reply a complete, self-contained summary of what you did, found and left undone.`;
    try {
      const created = await this.req<{ thread: ThreadSummary }>(this.local, {
        type: "create_thread",
        thread_id: id,
        project_id: me.project_id,
        workspace: opts.workspace ?? false,
        base: opts.base ?? null,
        parent: this.self.threadId,
        name: opts.name,
        report: opts.report ?? "reply",
        notify: opts.notify ?? "asap",
        // The parent's tools reach the child through the service (`CreateThread { parent }`).
        prompt: firstPrompt(text, model, opts.effort ?? me.thinking_level, null),
      });
      // A host from before `thread_id` picks an id of its own: the reply's is the one.
      return created.thread.id;
    } catch (e) {
      if (e instanceof TimeoutError)
        throw new TimeoutError(`${e.message}; it may have started anyway, as thread ${id}`);
      throw e;
    }
  }

  /**
   * Start a top-level thread on any reachable host: a normal thread in the user's sidebar,
   * prompted with `task` as is. Nobody is told how it goes.
   */
  async create(opts: NewOptions): Promise<Found> {
    const me = await this.me();
    const host = await this.host(opts.host);
    const project = await this.pickProject(host, opts.project, me);
    const model = opts.model ?? me.model;
    if (!model) throw new Error("no model: pass `model`");
    await this.checkModel(host, model);
    const created = await this.req<{ thread: ThreadSummary }>(host, {
      type: "create_thread",
      project_id: project.id,
      workspace: opts.workspace ?? false,
      base: opts.base ?? null,
      parent: null,
      name: opts.name,
      // No parent to inherit tools from: it gets the ones the caller has.
      prompt: firstPrompt(
        opts.task.trim(),
        model,
        opts.effort ?? me.thinking_level,
        this.self.tools,
      ),
    });
    const t = created.thread;
    owners.set(t.id, host);
    return { host, thread: t, project };
  }

  private async me(): Promise<ThreadSummary> {
    const me = (await this.threads()).find((t) => t.id === this.self.threadId);
    if (!me) throw new Error(`no thread ${this.self.threadId} here`);
    return me;
  }

  /** `want` (an id or a path) on `host`; by default the caller's project, which on another host is the one with the same origin. */
  private async pickProject(
    host: HostInfo,
    want: string | undefined,
    me: ThreadSummary,
  ): Promise<Project> {
    const projects = await this.projects(host);
    const listing = () =>
      projects.length
        ? `; its projects: ${projects.map((p) => p.path).join(", ")}`
        : "; it has no projects";
    if (want) {
      const path = want.replace(/\/+$/, "");
      const p = projects.find((p) => p.id === want || p.path.replace(/\/+$/, "") === path);
      if (!p) throw new Error(`${host.name} has no project "${want}"${listing()}`);
      return p;
    }
    const mine = (await this.projects()).find((p) => p.id === me.project_id);
    if (host.url === this.local.url && mine) return mine;
    const same = mine?.origin ? projects.filter((p) => p.origin === mine.origin) : [];
    const [only] = same;
    if (only && same.length === 1) return only;
    throw new Error(
      `pass \`project\`: no single project on ${host.name} matches yours${listing()}`,
    );
  }

  /** Refuse a model `host` has no credentials for, naming the ones it has. */
  private async checkModel(host: HostInfo, model: string): Promise<void> {
    const r = await this.req<{ models: ModelInfo[] }>(host, { type: "list_models" });
    const keys = r.models.map((m) => `${m.provider}/${m.id}`);
    if (!keys.includes(model))
      throw new Error(`${host.name} has no credentials for ${model}; it has: ${keys.join(", ")}`);
  }

  /** Message a thread: a new turn if it is idle, a steer (default) or follow-up if it runs. */
  async send(id: string, text: string, mode: "steer" | "follow_up" = "steer"): Promise<Found> {
    const f = await this.locate(id);
    await this.req(f.host, {
      type: "prompt",
      thread_id: f.thread.id,
      text,
      mode: f.thread.running_since ? mode : "prompt",
      model: null,
      thinking_level: null,
      images: [],
      title: null,
      tools: null,
    });
    return f;
  }

  /** A thread's final reply (its last turn-ending message), or with `full` the whole conversation's text. */
  async read(id: string, full = false): Promise<Found & { reply?: string; transcript?: string }> {
    const f = await this.locate(id);
    const r = await this.req<{ entries: Entry[] }>(f.host, {
      type: "get_thread",
      thread_id: f.thread.id,
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
      ...f,
      reply: reply ? text(reply) : undefined,
      transcript: full ? turns.map((m) => `${m.role}: ${text(m)}`).join("\n\n") : undefined,
    };
  }

  /**
   * Threads on every host whose name contains every word of `query` (first) or whose
   * messages do (the host's full-text search). Hosts' ranks don't compare, so their lists
   * are interleaved, local first, up to `limit`.
   */
  async search(query: string, limit = 10): Promise<{ found: Found[]; failed: string[] }> {
    const words = query.toLowerCase().split(/\s+/).filter(Boolean);
    if (words.length === 0) throw new Error("`query` is empty");
    const each = await this.each(async (host) => {
      const [r, threads] = await Promise.all([
        this.req<{ hits: SearchHit[] }>(host, { type: "search_threads", query, limit }),
        this.threads(host),
      ]);
      const named = threads
        .filter((t) => t.name && words.every((w) => t.name!.toLowerCase().includes(w)))
        .map((t) => ({ t, snippet: undefined as string | undefined }));
      const texts = r.hits.flatMap((h) => {
        const t = threads.find((t) => t.id === h.thread_id);
        return t && !named.some((n) => n.t.id === t.id) ? [{ t, snippet: h.snippet }] : [];
      });
      return Promise.all(
        [...named, ...texts].slice(0, limit).map((x) => this.found(host, x.t, x.snippet)),
      );
    });
    const lists = each.map((e) => e.value ?? []);
    const found: Found[] = [];
    for (let i = 0; found.length < limit && lists.some((l) => i < l.length); i++)
      for (const l of lists) {
        const f = l[i];
        if (f && found.length < limit) found.push(f);
      }
    return { found, failed: failures(each) };
  }

  /** The most recently active top-level threads (what the sidebar shows) on every host. */
  async list(limit = 20): Promise<{ found: Found[]; failed: string[] }> {
    const each = await this.each(async (host) => {
      const threads = (await this.threads(host)).filter((t) => !t.parent_id && !t.archived);
      return Promise.all(threads.slice(0, limit).map((t) => this.found(host, t)));
    });
    const found = each
      .flatMap((e) => e.value ?? [])
      .sort((a, b) => b.thread.updated_at - a.thread.updated_at)
      .slice(0, limit);
    return { found, failed: failures(each) };
  }

  /** Every host's projects. */
  allProjects(): Promise<Each<Project[]>[]> {
    return this.each((host) => this.projects(host));
  }

  async archive(id: string, archived = true): Promise<Found> {
    const f = await this.locate(id);
    await this.req(f.host, { type: "archive_thread", thread_id: f.thread.id, archived });
    return f;
  }

  async snooze(id: string, snoozed = true): Promise<Found> {
    const f = await this.locate(id);
    await this.req(f.host, { type: "snooze_thread", thread_id: f.thread.id, snoozed });
    return f;
  }

  /** Keep the thread on top of its status group in the user's lists (`pinned: false` unpins). */
  async pin(id: string, pinned = true): Promise<Found> {
    const f = await this.locate(id);
    await this.req(f.host, { type: "pin_thread", thread_id: f.thread.id, pinned });
    return f;
  }

  async abort(id: string): Promise<Found> {
    const f = await this.locate(id);
    await this.req(f.host, { type: "abort", thread_id: f.thread.id });
    return f;
  }

  /** Ask the desktops connected to the thread's host to open it. */
  async open(id: string): Promise<Found> {
    const f = await this.locate(id);
    await this.req(f.host, { type: "focus_thread", thread_id: f.thread.id });
    return f;
  }
}

/** A thread id, also accepted as its `dirigent://thread/<id>` link. */
export function threadId(s: string): string {
  s = s.trim();
  return s.startsWith(LINK) ? s.slice(LINK.length) : s;
}

/** The markdown link the desktop opens the thread from. */
export function threadLink(t: ThreadSummary): string {
  const name = (t.name ?? "untitled").replace(/[[\]]/g, "");
  return `[${name}](${LINK}${t.id})`;
}

/**
 * A new thread's first prompt, sent with its `create_thread` (a `FirstPrompt`): the host
 * starts the run as it creates the thread, so nobody sees it idle meanwhile, and deletes it
 * again if the prompt can't be delivered, so no empty thread is left behind.
 */
function firstPrompt(
  text: string,
  model: string | null,
  effort: string | null,
  tools: string[] | null,
) {
  return { text, model, thinking_level: effort, images: [], title: null, tools };
}

function failures(each: Each<unknown>[]): string[] {
  return each.flatMap((e) => (e.error ? [`${e.host.name} (${e.error})`] : []));
}

/** One line per thread for the model: link (name and id), status, host, project, model, age. */
export function describeThread(f: Found, now = Date.now()): string {
  const t = f.thread;
  const status = t.running_since
    ? `working ${ago(now - t.running_since)}`
    : t.asks.length
      ? "asking the user"
      : t.archived
        ? "archived"
        : t.snoozed
          ? "snoozed"
          : "done";
  const jobs = t.jobs > 0 ? `, ${t.jobs} job${t.jobs === 1 ? "" : "s"} running` : "";
  const parent = t.parent_id ? ` parent=${t.parent_id}` : "";
  const project = f.project ? ` project=${f.project.path}` : " project=(removed)";
  // A thread in its own workspace works somewhere else than its project.
  const cwd = t.cwd !== f.project?.path ? ` cwd=${t.cwd}` : "";
  const snippet = f.snippet ? `\n    \u2026${f.snippet}\u2026` : "";
  return `- ${threadLink(t)} [${status}${jobs}] host=${f.host.name}${project}${cwd} model=${t.model ?? "?"}${parent} updated ${ago(now - t.updated_at)} ago${snippet}`;
}

function ago(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}
