// Dirigent's own tools: `async_bash` runs a command in the background and tells the model
// when it ends; `ask_user` puts questions in front of the user as a form in the desktop;
// `dirigent` is the orchestrator: spawn sub-agent threads, message / read / find / manage
// threads on this host and open one for the user. All of them return at once. The outcome
// reaches the model later as a custom message through pi's steer / follow-up queues, which
// also wakes an idle agent for a new turn. A job picks how it interrupts (`notify`); the
// user's answers are user input and always steer; a sub-agent ending is delivered by the
// service (`deliverNotice`).

import { spawn } from "node:child_process";
import { closeSync, mkdirSync, openSync, readFileSync } from "node:fs";
import { constants as osConstants } from "node:os";
import { join } from "node:path";
import {
  type AgentSession,
  defineTool,
  type ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { Client, Dirigent, describeThread } from "./client";

export const TOOL_NAMES = ["async_bash", "ask_user", "dirigent"] as const;

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
      Type.Literal("send"),
      Type.Literal("read"),
      Type.Literal("status"),
      Type.Literal("search"),
      Type.Literal("archive"),
      Type.Literal("snooze"),
      Type.Literal("abort"),
      Type.Literal("open"),
    ],
    {
      description:
        "spawn: a sub-agent thread called `name` for `task` (you are told when it finishes, with its reply). " +
        "send: message `thread_id` (`text`; a new turn if idle, else a steer or follow_up). " +
        "read: a thread's final reply (`full` for the whole conversation). " +
        "status: your sub-agents, or one `thread_id`. " +
        "search: threads whose messages contain `query`. " +
        "archive / snooze / abort / open (in the user's desktop): act on `thread_id`.",
    },
  ),
  thread_id: Type.Optional(Type.String()),
  task: Type.Optional(Type.String({ description: "spawn: the sub-agent's full prompt." })),
  text: Type.Optional(Type.String({ description: "send: the message." })),
  mode: Type.Optional(
    Type.Union([Type.Literal("steer"), Type.Literal("follow_up")], {
      description:
        "send, when the thread is running: interrupt it (steer, default) or wait for its turn to end.",
    }),
  ),
  model: Type.Optional(
    Type.String({ description: "spawn: `provider/id`; default: your own model." }),
  ),
  effort: Type.Optional(Type.String({ description: "spawn: thinking level; default: your own." })),
  workspace: Type.Optional(
    Type.Boolean({
      description:
        "spawn: give it its own git worktree / jj workspace (from `base`, default HEAD) instead of your directory. Use it when sub-agents edit files in parallel.",
    }),
  ),
  base: Type.Optional(Type.String()),
  query: Type.Optional(Type.String()),
  limit: Type.Optional(Type.Number()),
  full: Type.Optional(Type.Boolean()),
  archived: Type.Optional(Type.Boolean({ description: "archive: false to unarchive." })),
  snoozed: Type.Optional(Type.Boolean({ description: "snooze: false to wake." })),
  name: Type.Optional(
    Type.String({
      description: "spawn (required): the sub-agent's name, a few words on what it is for.",
    }),
  ),
});

/** One connection to the service for the whole runtime, made on first use and remade if it drops. */
let shared: Promise<Client> | undefined;
async function serviceClient(): Promise<Client> {
  if (shared) {
    const c = await shared.catch(() => undefined);
    if (c?.alive) return c;
  }
  shared = Client.connect();
  return shared;
}

/** The three tools, bound to `host`. All are registered on every session; `tools` picks the active ones. */
export function createTools(host: ToolHost): ToolDefinition[] {
  const asyncBash = defineTool({
    name: "async_bash",
    label: "Background command",
    description:
      "Run a command in the background. Returns at once with a job id and log path; when the job ends you get a " +
      "message with its exit code and the last lines of output. For builds, tests and servers; use `bash` when " +
      "you need the output before your next step.",
    promptSnippet: "Run a command in the background; a message tells you when it ends",
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
      "Ask the user one or more questions, shown as a form. Returns at once; the answers arrive as a message, so " +
      "keep working on what does not depend on them, or end your turn to wait.",
    promptSnippet: "Ask the user questions; the answers arrive as a message",
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
      "Orchestrate threads on this host. Spawn sub-agents (each a thread of its own, started in your " +
      "directory with your model unless told otherwise) and carry on; when one finishes you get a message " +
      "with its reply, so end your turn to wait or keep working meanwhile. Message, read, find, archive, " +
      "snooze or abort any thread, and open one for the user.",
    promptSnippet: "Spawn and manage sub-agent threads, find threads",
    parameters: dirigentParams,
    async execute(_id, p) {
      const d = new Dirigent(await serviceClient(), { threadId: host.threadId });
      const need = (v: string | undefined, what: string) => {
        if (!v) throw new Error(`\`${what}\` is required for ${p.action}`);
        return v;
      };
      switch (p.action) {
        case "spawn": {
          const id = await d.spawn({
            name: need(p.name, "name"),
            task: need(p.task, "task"),
            model: p.model,
            effort: p.effort,
            workspace: p.workspace,
            base: p.base,
          });
          return text(
            `Spawned sub-agent thread ${id}. You will get a message with its reply when it finishes; ` +
              `use status / read to look in on it, send to steer it.`,
          );
        }
        case "send": {
          await d.send(need(p.thread_id, "thread_id"), need(p.text, "text"), p.mode);
          return text(`Sent to ${p.thread_id}.`);
        }
        case "read": {
          const r = await d.read(need(p.thread_id, "thread_id"), p.full);
          const body = p.full ? (r.transcript ?? "(empty)") : (r.reply ?? "(no final reply yet)");
          return text(`${describeThread(r.thread)}\n\n${body}`);
        }
        case "status": {
          const threads = await d.threads();
          if (p.thread_id) {
            const t = threads.find((t) => t.id === p.thread_id);
            if (!t) throw new Error(`no thread ${p.thread_id}`);
            return text(describeThread(t));
          }
          // In the desktop panel's order: by when each was last given a task.
          const kids = threads
            .filter((t) => t.parent_id === host.threadId)
            .sort(
              (a, b) =>
                (a.prompted_at ?? a.created_at) - (b.prompted_at ?? b.created_at) ||
                a.created_at - b.created_at ||
                a.id.localeCompare(b.id),
            );
          return text(
            `You are thread ${host.threadId}.\n` +
              (kids.length
                ? `Sub-agents:\n${kids.map((t) => describeThread(t)).join("\n")}`
                : "No sub-agents."),
          );
        }
        case "search": {
          const hits = await d.search(need(p.query, "query"), p.limit ?? 10);
          if (hits.length === 0) return text("No thread matches.");
          return text(
            hits
              .map(
                (h) =>
                  `${h.thread ? describeThread(h.thread) : `- ${h.thread_id}`}\n    \u2026${h.snippet}\u2026`,
              )
              .join("\n"),
          );
        }
        case "archive":
          await d.archive(need(p.thread_id, "thread_id"), p.archived ?? true);
          return text(`${p.archived === false ? "Unarchived" : "Archived"} ${p.thread_id}.`);
        case "snooze":
          await d.snooze(need(p.thread_id, "thread_id"), p.snoozed ?? true);
          return text(`${p.snoozed === false ? "Woke" : "Snoozed"} ${p.thread_id}.`);
        case "abort":
          await d.abort(need(p.thread_id, "thread_id"));
          return text(`Aborted ${p.thread_id}.`);
        case "open":
          await d.open(need(p.thread_id, "thread_id"));
          return text(`Opened ${p.thread_id} for the user.`);
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
  const id = shortId();
  const dir = join(dataDir(), "jobs", host.threadId);
  mkdirSync(dir, { recursive: true });
  const log = join(dir, `${id}.log`);
  // One fd for both streams so their lines interleave in order; the child keeps its own copy.
  const fd = openSync(log, "w");
  const child = spawn("bash", ["-c", command], {
    cwd: host.cwd,
    env: host.env ? { ...process.env, ...host.env } : process.env,
    detached: true,
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
  if (error) console.error(`[runtime] job ${job.id} failed to spawn: ${error}`);
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
  const lines = text.trimEnd().split("\n").slice(-TAIL_LINES).join("\n");
  return lines.length > TAIL_CHARS ? `…${lines.slice(-TAIL_CHARS)}` : lines;
}

function getJob(host: ToolHost, id: string | undefined): Job {
  if (!id) throw new Error("`job_id` is required");
  const job = host.jobs.get(id);
  if (!job) throw new Error(`no job ${id} in this thread`);
  return job;
}

/** SIGTERM to the job's process group (`detached` made it one), SIGKILL if it lingers. */
function killJob(job: Job): void {
  const pid = job.pid;
  if (!pid) return;
  job.killed = true;
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

/** `$XDG_DATA_HOME/dirigent` (else `~/.local/share/dirigent`), as the service computes it. */
function dataDir(): string {
  const base = process.env.XDG_DATA_HOME || join(process.env.HOME ?? "/tmp", ".local/share");
  return join(base, "dirigent");
}
