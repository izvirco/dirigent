// Dirigent's own tools: `async_bash` runs a command in the background and tells the model
// when it ends; `ask_user` puts questions in front of the user as a form in the desktop.
// Both return at once. The outcome reaches the model later as a custom message through
// pi's steer / follow-up queues (`notify`), which also wakes an idle agent for a new turn.

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

export const TOOL_NAMES = ["async_bash", "ask_user"] as const;

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
  notify: Notify;
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
      'When to tell you about the outcome: "asap" (default) interrupts you after your current tool calls; "when_idle" waits until you have ended your turn.',
  }),
);

const asyncBashParams = Type.Object({
  action: Type.Optional(
    Type.Union([Type.Literal("start"), Type.Literal("status"), Type.Literal("kill")], {
      description:
        '"start" (default) runs `command`; "status" reports on `job_id` (or every job); "kill" stops `job_id`.',
    }),
  ),
  command: Type.Optional(
    Type.String({ description: "Shell command to run in the background (bash)." }),
  ),
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
  notify: notifyParam,
});

/** The two tools, bound to `host`. Both are registered on every session; `tools` picks the active ones. */
export function createTools(host: ToolHost): ToolDefinition[] {
  const asyncBash = defineTool({
    name: "async_bash",
    label: "Background command",
    description:
      "Run a shell command in the background and carry on working. Returns at once with a job id and the path of " +
      "its log file; a command that ends within two seconds is reported right away instead. When a job ends you " +
      "receive a message with its exit code, how long it took, the log path and the last lines of output, so a " +
      "successful run usually needs no further action and a failed one tells you where to read. Use it for " +
      "builds, test suites, long scripts and servers; use `bash` when you need the output before your next step. " +
      "Jobs end with the thread's session.",
    promptSnippet: "Run a command in the background and be told when it finishes",
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
      "Ask the user one or more questions. They see a form: each question with its options (single or multiple " +
      "choice), a text field where you allow a custom answer, or a text field alone when there are no options. " +
      "Returns at once; the answers come to you as a message, so keep working on what does not depend on them, " +
      "or end your turn to wait. The user may dismiss the questions instead of answering.",
    promptSnippet: "Ask the user questions (options or free text); answers arrive as a message",
    parameters: askUserParams,
    async execute(_id, params) {
      const ask: Ask = {
        id: shortId(),
        questions: params.questions,
        notify: params.notify ?? "asap",
      };
      host.onAsk(ask);
      return text(
        `Asked the user (ask ${ask.id}). Their answers arrive as a message; continue with other work or end your turn to wait.`,
      );
    },
  });

  return [asyncBash as ToolDefinition, askUser as ToolDefinition];
}

/** The user's answers (one list per question, picked options and / or typed text) or `null` when dismissed. */
export function deliverAnswers(
  host: ToolHost,
  ask: Ask,
  answers: string[][] | null,
): Promise<void> {
  const lines = answers
    ? ask.questions.map((q, i) => {
        const a = answers[i] ?? [];
        return `- ${q.text}\n  → ${a.length ? a.join(", ") : "(no answer)"}`;
      })
    : [];
  const content = answers
    ? `The user answered ask ${ask.id}:\n${lines.join("\n")}`
    : `The user dismissed ask ${ask.id} without answering.`;
  return deliver(host, `ask-${ask.id}`, {
    customType: "dirigent:answer",
    content,
    details: { ask_id: ask.id, questions: ask.questions, answers },
    notify: ask.notify,
  });
}

/** Pi's `clearQueue` (used by `cancelQueued`) drops queued custom messages too: queue ours again. */
export async function requeueNotices(host: ToolHost): Promise<void> {
  for (const [key, n] of host.notices) await deliver(host, key, n);
}

/** Every job's process group, when the session goes. */
export function killJobs(host: ToolHost): void {
  for (const job of host.jobs.values()) if (job.exit === undefined) killJob(job);
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
  job.exited.then(() => {
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
