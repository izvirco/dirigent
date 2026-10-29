// Dirigent's own tools: `async_bash` runs a command in the background and tells the model
// when it ends; `ask_user` puts questions in front of the user as a form in the desktop;
// `dirigent` is the orchestrator: spawn sub-agent threads, start top-level threads,
// message / read / find / manage threads on every host this one reaches and open one for
// the user. All of them return at once. The outcome
// reaches the model later as a custom message through pi's steer / follow-up queues, which
// also wakes an idle agent for a new turn. A job picks how it interrupts (`notify`); the
// user's answers are user input and always steer; a sub-agent ending is delivered by the
// service (`deliverNotice`).

import { spawn } from "node:child_process";
import { closeSync, mkdirSync, openSync, readFileSync } from "node:fs";
import { homedir, constants as osConstants } from "node:os";
import { join } from "node:path";
import {
  type AgentSession,
  defineTool,
  getShellConfig,
  type ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { Dirigent, describeThread, threadLink } from "./client";

export const TOOL_NAMES = ["async_bash", "ask_user", "dirigent"] as const;

const WINDOWS = process.platform === "win32";

/** When a notice reaches the model: after the current tool batch, or once it is idle. */
export type Notify = "asap" | "when_idle";

export interface Question {
  text: string;
  options?: string[];
  allow_custom?: boolean;
  multi?: boolean;
}

/** A pending `ask_user` call; the service persists it until the user answers. */
export interface Ask {
  id: string;
  questions: Question[];
}

/** A background command. Lives as long as its session; the log file outlives both. */
interface Job {
  id: string;
  command: string;
  log: string;
  pid: number | undefined;
  started: number;
  exit?: number;
  ended?: number;
  /** The model killed it: no notice, it knows. */
  killed?: boolean;
  /** Outlived `GRACE_MS`: the model was told it runs on, and the host counts it (`reportJobs`). */
  background?: boolean;
  notify: Notify;
  exited: Promise<number>;
}

/** What the tools need from their session; filled in by `Sessions.load`. */
export interface ToolHost {
  threadId: string;
  cwd: string;
  /** Set once pi has created the session. */
  session: AgentSession;
  /** Captured nix dev-shell environment, once there is one. */
  env?: Record<string, string>;
  jobs: Map<string, Job>;
  /** Notices queued in pi that it has not delivered yet, by job / ask id (see `requeueNotices`). */
  notices: Map<string, Notice>;
  onAsk: (ask: Ask) => void;
  /** Tell the host how many jobs are running (`reportJobs`). */
  onJobs: (running: number) => void;
}

interface Notice {
  customType: string;
  content: string;
  details: Record<string, unknown>;
  notify: Notify;
}

/** A started command that ends within this long is answered inline, with no notice. */
const GRACE_MS = 2000;
const TAIL_LINES = 30;
const TAIL_CHARS = 4000;

const notifyParam = Type.Optional(
  Type.Union([Type.Literal("asap"), Type.Literal("when_idle")], {
    description:
      'When the result reaches you: "asap" (default) after your current tool calls, "when_idle" once your turn has ended.',
  }),
);

const asyncBashParams = Type.Object({
  action: Type.Optional(
    Type.Union([Type.Literal("start"), Type.Literal("status"), Type.Literal("kill")], {
      description:
        '"start" (default) runs `command`; "status" reports on `job_id` (or all jobs); "kill" stops `job_id`.',
    }),
  ),
  command: Type.Optional(Type.String({ description: "Bash command to run in the background." })),
  job_id: Type.Optional(Type.String({ description: "The job to report on or kill." })),
  notify: notifyParam,
});

const askUserParams = Type.Object({
  questions: Type.Array(
    Type.Object({
      text: Type.String({ description: "The question." }),
      options: Type.Optional(
        Type.Array(Type.String(), {
          description: "Choices to pick from. Omit for a free-text answer.",
        }),
      ),
      allow_custom: Type.Optional(
        Type.Boolean({
          description: "Also allow a typed answer next to the options. Default false.",
        }),
      ),
      multi: Type.Optional(
        Type.Boolean({ description: "Several options may be picked. Default false." }),
      ),
    }),
    { minItems: 1 },
  ),
});

const dirigentParams = Type.Object({
  action: Type.Union(
    [
      Type.Literal("spawn"),
      Type.Literal("new"),
      Type.Literal("send"),
      Type.Literal("read"),
      Type.Literal("status"),
      Type.Literal("list"),
      Type.Literal("search"),
      Type.Literal("projects"),
      Type.Literal("archive"),
      Type.Literal("snooze"),
      Type.Literal("pin"),
      Type.Literal("abort"),
      Type.Literal("open"),
    ],
    {
      description:
        "spawn: a sub-agent called `name` for `task`. " +
        "new: a top-level thread for the user (not a sub-agent) called `name` for `task`, in any project on any host. " +
        "send: message `thread_id` with `text`. " +
        "read: a thread's final reply (`full`: the whole conversation). " +
        "status: your sub-agents, or one `thread_id`. " +
        "list: the latest top-level threads on every host. " +
        "search: threads on every host whose name or messages contain `query`. " +
        "projects: every host's projects. " +
        "archive / snooze / pin / abort / open (in the user's desktop): act on `thread_id`.",
    },
  ),
  thread_id: Type.Optional(
    Type.String({ description: "A thread id, or its dirigent://thread/<id> link." }),
  ),
  task: Type.Optional(
    Type.String({
      description:
        "spawn / new: the thread's full prompt. A `new` thread knows nothing of you or this conversation: make it self-contained.",
    }),
  ),
  host: Type.Optional(
    Type.String({
      description: 'new: a host name from `projects` (default "local", the host you run on).',
    }),
  ),
  project: Type.Optional(
    Type.String({
      description:
        "new: a project id or path on that host (default: your project; on another host, the one with the same git origin).",
    }),
  ),
  report: Type.Optional(
    Type.Union([Type.Literal("reply"), Type.Literal("done")], {
      description:
        'spawn: what you are told when it finishes: "reply" (default) its final reply, "done" only that it finished (use `read` for the replies you need; keeps many sub-agents\' replies out of your context).',
    }),
  ),
  notify: notifyParam,
  text: Type.Optional(Type.String({ description: "send: the message." })),
  mode: Type.Optional(
    Type.Union([Type.Literal("steer"), Type.Literal("follow_up")], {
      description:
        "send, when the thread is running: interrupt it (steer, default) or wait for its turn to end.",
    }),
  ),
  model: Type.Optional(
    Type.String({
      description: "spawn / new: `provider/id`; default: your own model.",
    }),
  ),
  effort: Type.Optional(
    Type.String({ description: "spawn / new: thinking level; default: your own." }),
  ),
  workspace: Type.Optional(
    Type.Boolean({
      description:
        "spawn / new: give it its own git worktree / jj workspace (from `base`, default HEAD) instead of your directory (spawn) or the project's (new). Use it when it edits files while others work in the same checkout.",
    }),
  ),
  base: Type.Optional(Type.String()),
  query: Type.Optional(Type.String()),
  limit: Type.Optional(Type.Number()),
  full: Type.Optional(Type.Boolean()),
  archived: Type.Optional(Type.Boolean({ description: "archive: false to unarchive." })),
  snoozed: Type.Optional(Type.Boolean({ description: "snooze: false to wake." })),
  pinned: Type.Optional(Type.Boolean({ description: "pin: false to unpin." })),
  name: Type.Optional(
    Type.String({
      description: "spawn / new (required): the thread's name, a few words on what it is for.",
    }),
  ),
});

/** The three tools, bound to `host`. All are registered on every session; `tools` picks the active ones. */
export function createTools(host: ToolHost): ToolDefinition[] {
  const asyncBash = defineTool({
    name: "async_bash",
    label: "Background command",
    description:
      "Run a long command in the background (builds, test suites, servers) and keep working; a message tells you " +
      "when it ends. Use `bash` when you'd only wait for the output.",
    promptSnippet:
      "Run long commands (builds, tests, servers) in the background while you keep working",
    parameters: asyncBashParams,
    async execute(_id, params) {
      const action = params.action ?? "start";
      if (action === "start") {
        if (!params.command?.trim()) throw new Error("`command` is required to start a job");
        return text(await startJob(host, params.command, params.notify ?? "asap"));
      }
      if (action === "kill") {
        const job = getJob(host, params.job_id);
        if (job.exit !== undefined)
          return text(`job ${job.id} already ended with exit ${job.exit}`);
        killJob(job);
        reportJobs(host);
        return text(`job ${job.id} killed`);
      }
      if (params.job_id) return text(describe(getJob(host, params.job_id), true));
      const jobs = [...host.jobs.values()];
      return text(jobs.length === 0 ? "no jobs" : jobs.map((j) => describe(j, false)).join("\n"));
    },
  });

  const askUser = defineTool({
    name: "ask_user",
    label: "Ask the user",
    description:
      "Ask the user questions. Use it when you're blocked, or have any open questions on a plan or design " +
      "(anything you'd otherwise list as questions in your reply). Tool runs async, so use it before your final " +
      "reply, don't repeat questions in prose. Not for offers to do more (Want me to fix it?), ask those in your reply.",
    promptSnippet:
      "Ask the user questions in a form, when you're blocked on their decisions (several questions, or a choice between options)",
    parameters: askUserParams,
    async execute(_id, params) {
      const ask: Ask = { id: shortId(), questions: params.questions };
      host.onAsk(ask);
      return text(
        `Asked the user (ask ${ask.id}). Their answers arrive as a message; continue with other work or end your turn to wait.`,
      );
    },
  });

  const dirigent = defineTool({
    name: "dirigent",
    label: "Dirigent",
    description:
      "Work with dirigent threads when the user asks you to: spawn sub-agents, start a top-level thread (`new`), " +
      "or find, read, message and manage other threads.",
    promptSnippet: "Spawn sub-agents, start threads, or manage other threads, when the user asks",
    parameters: dirigentParams,
    async execute(_id, p) {
      const ours = new Set<string>(TOOL_NAMES);
      const d = new Dirigent({
        threadId: host.threadId,
        tools: host.session.getActiveToolNames().filter((n) => ours.has(n)),
      });
      const need = (v: string | undefined, what: string) => {
        if (!v) throw new Error(`\`${what}\` is required for ${p.action}`);
        return v;
      };
      /** Found threads, one per line, then the hosts that didn't answer. */
      const listing = (r: { found: Parameters<typeof describeThread>[0][]; failed: string[] }) =>
        (r.found.length ? r.found.map((f) => describeThread(f)).join("\n") : "No threads.") +
        (r.failed.length ? `\nNot reached: ${r.failed.join("; ")}` : "");
      try {
        switch (p.action) {
          case "spawn": {
            const name = need(p.name, "name");
            const id = await d.spawn({
              name,
              task: need(p.task, "task"),
              model: p.model,
              effort: p.effort,
              workspace: p.workspace,
              base: p.base,
              report: p.report,
              notify: p.notify,
            });
            const what = p.report === "done" ? "that it finished" : "its reply";
            const when = p.notify === "when_idle" ? "once your turn has ended" : "";
            return text(
              // The desktop finds the id by this prefix (`spawned_id`): keep it.
              `Spawned sub-agent thread ${id}. You will get a message with ${what} when it finishes${when ? ` (${when})` : ""}; ` +
                `use status / read to look in on it, send to steer it. Put ${threadLink({ id, name })} in your reply so the user can open it.`,
            );
          }
          case "new": {
            const f = await d.create({
              name: need(p.name, "name"),
              task: need(p.task, "task"),
              host: p.host,
              project: p.project,
              model: p.model,
              effort: p.effort,
              workspace: p.workspace,
              base: p.base,
            });
            return text(
              `Started ${threadLink(f.thread)} on ${f.host.name} in ${f.project?.path}. It is the user's now: ` +
                `you won't hear from it. Put that link in your reply so the user can open it.`,
            );
          }
          case "send": {
            const f = await d.send(need(p.thread_id, "thread_id"), need(p.text, "text"), p.mode);
            return text(`Sent to ${threadLink(f.thread)}.`);
          }
          case "read": {
            const r = await d.read(need(p.thread_id, "thread_id"), p.full);
            const body = p.full ? (r.transcript ?? "(empty)") : (r.reply ?? "(no final reply yet)");
            return text(`${describeThread(r)}\n\n${body}`);
          }
          case "status": {
            if (p.thread_id) return text(describeThread(await d.locate(p.thread_id)));
            // In the desktop panel's order: by when each was last given a task.
            const kids = (await d.threads())
              .filter((t) => t.parent_id === host.threadId)
              .sort(
                (a, b) =>
                  (a.prompted_at ?? a.created_at) - (b.prompted_at ?? b.created_at) ||
                  a.created_at - b.created_at ||
                  a.id.localeCompare(b.id),
              );
            const projects = await d.projects();
            const line = (t: (typeof kids)[number]) =>
              describeThread({
                host: d.local,
                thread: t,
                project: projects.find((p) => p.id === t.project_id),
              });
            return text(
              `You are thread ${host.threadId}.\n` +
                (kids.length ? `Sub-agents:\n${kids.map(line).join("\n")}` : "No sub-agents."),
            );
          }
          case "list":
            return text(listing(await d.list(p.limit ?? 20)));
          case "search": {
            const r = await d.search(need(p.query, "query"), p.limit ?? 10);
            return text(r.found.length || r.failed.length ? listing(r) : "No thread matches.");
          }
          case "projects": {
            const each = await d.allProjects();
            return text(
              each
                .map(({ host: h, value, error }) =>
                  error
                    ? `${h.name}: not reached (${error})`
                    : `${h.name}:\n` +
                      (value?.length
                        ? value
                            .map(
                              (p) => `  - ${p.path}${p.origin ? ` (${p.origin})` : ""} id=${p.id}`,
                            )
                            .join("\n")
                        : "  (no projects)"),
                )
                .join("\n"),
            );
          }
          case "archive": {
            const f = await d.archive(need(p.thread_id, "thread_id"), p.archived ?? true);
            return text(
              `${p.archived === false ? "Unarchived" : "Archived"} ${threadLink(f.thread)}.`,
            );
          }
          case "snooze": {
            const f = await d.snooze(need(p.thread_id, "thread_id"), p.snoozed ?? true);
            return text(`${p.snoozed === false ? "Woke" : "Snoozed"} ${threadLink(f.thread)}.`);
          }
          case "pin": {
            const f = await d.pin(need(p.thread_id, "thread_id"), p.pinned ?? true);
            return text(`${p.pinned === false ? "Unpinned" : "Pinned"} ${threadLink(f.thread)}.`);
          }
          case "abort": {
            const f = await d.abort(need(p.thread_id, "thread_id"));
            return text(`Aborted ${threadLink(f.thread)}.`);
          }
          case "open": {
            const f = await d.open(need(p.thread_id, "thread_id"));
            return text(
              `Asked the desktops connected to ${f.host.name} to open ${threadLink(f.thread)} ` +
                `(a desktop that doesn't connect to ${f.host.name} won't).`,
            );
          }
        }
      } finally {
        d.close();
      }
    },
  });

  return [asyncBash as ToolDefinition, askUser as ToolDefinition, dirigent as ToolDefinition];
}

/**
 * The user's answers (one list per question, picked options and / or typed text) or `null` when dismissed.
 * Each answer is its own `→` line: joining them would make a typed "Kiwi, sliced" look like two picks.
 */
export function deliverAnswers(
  host: ToolHost,
  ask: Ask,
  answers: string[][] | null,
): Promise<void> {
  const lines = answers
    ? ask.questions.map((q, i) => {
        const a = answers[i] ?? [];
        const got = a.length ? a.map((x) => `  → ${x}`) : ["  (no answer)"];
        return [`- ${q.text}`, ...got].join("\n");
      })
    : [];
  const content = answers
    ? `The user answered ask ${ask.id}:\n${lines.join("\n")}`
    : `The user dismissed ask ${ask.id} without answering.`;
  return deliver(host, `ask-${ask.id}`, {
    customType: "dirigent:answer",
    content,
    details: { ask_id: ask.id, questions: ask.questions, answers },
    notify: "asap",
  });
}

/** The service's notice (a sub-agent ended): hand it to the model. */
export function deliverNotice(
  host: ToolHost,
  customType: string,
  content: string,
  details: Record<string, unknown>,
  notify: Notify,
): Promise<void> {
  return deliver(host, `${customType}-${shortId()}`, { customType, content, details, notify });
}

/** Pi's `clearQueue` (used by `cancelQueued`) drops queued custom messages too: queue ours again. */
export async function requeueNotices(host: ToolHost): Promise<void> {
  for (const [key, n] of host.notices) await deliver(host, key, n);
}

/** Every job's process group, when the session goes. */
export function killJobs(host: ToolHost): void {
  for (const job of host.jobs.values()) if (job.exit === undefined) killJob(job);
  reportJobs(host);
}

async function startJob(host: ToolHost, command: string, notify: Notify): Promise<string> {
  // The shell pi's bash tool runs: bash, Git Bash on Windows (throws, saying how to get
  // one, when there is none).
  const shell = getShellConfig();
  const id = shortId();
  const dir = join(dataDir(), "jobs", host.threadId);
  mkdirSync(dir, { recursive: true });
  const log = join(dir, `${id}.log`);
  // One fd for both streams so their lines interleave in order; the child keeps its own copy.
  const fd = openSync(log, "w");
  const child = spawn(shell.shell, [...shell.args, command], {
    cwd: host.cwd,
    // PI_SESSION_ID as pi's bash tool sets it (scripts/ui.sh keys its default session on it).
    env: { ...process.env, ...host.env, PI_SESSION_ID: host.threadId },
    // Its own process group, for `killJob`. Not on Windows: `taskkill /T` finds the tree
    // anyway, and a detached child has no console there, so every console program it
    // starts would open a window of its own.
    detached: !WINDOWS,
    windowsHide: true,
    stdio: ["ignore", fd, fd],
  });
  closeSync(fd);
  const job: Job = {
    id,
    command,
    log,
    pid: child.pid,
    started: Date.now(),
    notify,
    exited: new Promise((resolve) => {
      child.on("error", (e) => resolve(finish(job, 127, e.message)));
      // A signal-killed shell has no exit code; use the shell convention (128 + signal).
      child.on("exit", (code, signal) =>
        resolve(finish(job, code ?? 128 + (signal ? (osConstants.signals[signal] ?? 0) : 0))),
      );
    }),
  };
  host.jobs.set(id, job);
  const quick = await Promise.race([job.exited, sleep(GRACE_MS).then(() => undefined)]);
  if (quick !== undefined) return describe(job, true);
  job.background = true;
  reportJobs(host);
  job.exited.then(() => {
    reportJobs(host);
    if (job.killed) return;
    return deliver(host, `job-${id}`, {
      customType: "dirigent:job",
      content: describe(job, true),
      details: {
        job_id: id,
        command,
        exit_code: job.exit,
        duration_ms: (job.ended ?? 0) - job.started,
        log,
      },
      notify,
    });
  });
  return `job ${id} started: \`${command}\`\nlog: ${log}\nYou will be told when it ends.`;
}

/**
 * Send the host the thread's count of running background jobs: started past the grace
 * window (an inline one never counts), not ended, not killed. Called wherever that changes.
 */
function reportJobs(host: ToolHost): void {
  let running = 0;
  for (const j of host.jobs.values())
    if (j.background && j.exit === undefined && !j.killed) running++;
  host.onJobs(running);
}

function finish(job: Job, code: number, error?: string): number {
  job.exit = code;
  job.ended = Date.now();
  if (error) console.error(`job ${job.id} failed to spawn: ${error}`);
  return code;
}

/** One line per job for a listing, with the log tail for a single one. */
function describe(job: Job, withTail: boolean): string {
  const head =
    job.exit === undefined
      ? `job ${job.id} running for ${duration(Date.now() - job.started)}: \`${job.command}\``
      : `job ${job.id} ${job.killed ? "was killed" : job.exit === 0 ? "succeeded" : `failed with exit ${job.exit}`} after ${duration((job.ended ?? 0) - job.started)}: \`${job.command}\``;
  if (!withTail) return head;
  const tail = logTail(job.log);
  return `${head}\nlog: ${job.log}${tail ? `\nlast lines:\n${tail}` : "\n(no output)"}`;
}

function logTail(path: string): string {
  let text: string;
  try {
    text = readFileSync(path, "utf8");
  } catch {
    return "";
  }
  const lines = text.trimEnd().split(/\r?\n/).slice(-TAIL_LINES).join("\n");
  return lines.length > TAIL_CHARS ? `…${lines.slice(-TAIL_CHARS)}` : lines;
}

function getJob(host: ToolHost, id: string | undefined): Job {
  if (!id) throw new Error("`job_id` is required");
  const job = host.jobs.get(id);
  if (!job) throw new Error(`no job ${id} in this thread`);
  return job;
}

/**
 * SIGTERM to the job's process group (`detached` made it one), SIGKILL if it lingers. On
 * Windows `taskkill /T /F` ends the whole tree at once: console programs have no gentler
 * stop to send them.
 */
function killJob(job: Job): void {
  const pid = job.pid;
  if (!pid) return;
  job.killed = true;
  if (WINDOWS) {
    // From System32, like pi does: not whatever `taskkill` the PATH has.
    const taskkill = join(process.env.SystemRoot ?? "C:\\Windows", "System32", "taskkill.exe");
    spawn(taskkill, ["/T", "/F", "/PID", String(pid)], { stdio: "ignore", windowsHide: true }).on(
      "error",
      () => {},
    );
    return;
  }
  const signal = (s: NodeJS.Signals) => {
    try {
      process.kill(-pid, s);
    } catch {
      /* already gone */
    }
  };
  signal("SIGTERM");
  setTimeout(() => job.exit === undefined && signal("SIGKILL"), 2000).unref();
}

/**
 * Hand a notice to the model: queued as a steer or follow-up while it works (and, with
 * `steeringMode = "all"`, delivered with every other one waiting), else a new turn.
 * Remembered until pi delivers it, in case the queue is cleared meanwhile.
 */
async function deliver(host: ToolHost, key: string, n: Notice): Promise<void> {
  host.notices.set(key, n);
  await host.session.sendCustomMessage(
    { customType: n.customType, content: n.content, display: true, details: { ...n.details, key } },
    { triggerTurn: true, deliverAs: n.notify === "asap" ? "steer" : "followUp" },
  );
}

/** Pi started delivering a custom message: forget it (see `deliver`). */
export function noticeDelivered(host: ToolHost, message: unknown): void {
  const key = (message as { details?: { key?: string } })?.details?.key;
  if (key) host.notices.delete(key);
}

function text(s: string) {
  return { content: [{ type: "text" as const, text: s }], details: undefined };
}

function shortId(): string {
  return Math.random().toString(36).slice(2, 8);
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

function duration(ms: number): string {
  const s = Math.round(ms / 1000);
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s`;
}

/** The service's data dir (`DIRIGENT_DATA_DIR`, which it sets for us); run by hand, the default. */
function dataDir(): string {
  if (process.env.DIRIGENT_DATA_DIR) return process.env.DIRIGENT_DATA_DIR;
  const base = WINDOWS
    ? process.env.LOCALAPPDATA || join(homedir(), "AppData", "Local")
    : process.env.XDG_DATA_HOME || join(homedir(), ".local/share");
  return join(base, "dirigent");
}
