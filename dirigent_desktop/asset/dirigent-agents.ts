// Bundled privately by Dirigent; no user-installed extension or dependencies are needed.
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { spawn, type ChildProcess } from "node:child_process";
import { randomUUID } from "node:crypto";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const GUIDE = `Execute TypeScript to coordinate persistent child agents in Dirigent. The async function body has an injected agents API. No imports/setup/install needed. Normal JavaScript and erasable TypeScript are supported. Scripts run with normal user permissions, not in a sandbox.

Resolve exact models, supply self-contained briefs, and review actual changes before integration. Do not ask children to delegate further unless their assignment explicitly permits nested delegation.

To execute a workflow, provide both title (short label) and code (async TypeScript function body). Optional mode is "wait" or "background"; optional timeout is the script deadline in seconds. Return results from your code. This guide is now in the conversation; no need to reload it unless its contents are no longer available.

API (all methods async):
  agents.models(): configured authenticated models, each with id (exact provider/model), name, reasoning.
  agents.spawn({ name, model, thinking, workspace: "current" | "new", prompt, allowDirtyBase?: boolean }): { agentId: string, runId: string }
  agents.send(agentId, prompt): { agentId, runId } — new assignment in the SAME child session; rejects busy children.
  agents.wait([runId, ...], { mode?: "all" | "any", timeout?: seconds }): [{ agentId, run: { id, status, result, error } }] — terminal outcomes, NOT only successes. Defaults: all, 3600 seconds. any returns the finished subset without cancelling others.
  agents.inspect(agentId, { transcript?: boolean }): session/workspace paths, runs, model, reasoning, needsInput, and optional bounded recent transcript.
  agents.list(): this manager's children, including previous workflows.
  agents.jobs(): saved workflow statuses/results, including interrupted workflows after restart.
  agents.stop(agentId): cancel work, retain session/files.

The spawn name becomes the child's visible title. Use a concise, human-readable task description: "Add titles to sub-agent rows", not "Sub-agent work entry title".

spawn returns after scheduling, not after completion. Consecutive spawns run concurrently. Use await wait([first.runId]) before spawning a dependent task; use wait(all) or wait(any) for barriers. IDs remain usable in later scripts. send preserves the child's context. Children start with a fresh context and normal project/global Pi instructions, tools, extensions and skills; give each a self-contained brief with the agreed plan, relevant context, boundaries and acceptance criteria. Parent conversation is NOT automatically copied. Children should not delegate further unless their assignment explicitly permits nested delegation. Model/thinking must be explicit; first use models() to resolve shorthand to exact IDs; never silently substitute.

current means the manager's actual checkout, including its worktree. new provisions an isolated Git worktree or JJ workspace at its recorded revision. Dirty Git checkouts reject new unless allowDirtyBase:true explicitly acknowledges exclusion of uncommitted changes. Shared checkouts have no write isolation; avoid overlapping writers, including the manager. Git/JJ integration is NOT automatic: inspect actual diffs, run tests, provide feedback via send, then integrate with ordinary tools only when authorized. A completed run is not approval or proof of correctness. Never discard unrelated changes.

mode:"wait" (default) keeps this tool pending without model token usage, then returns the script result to you. mode:"background" returns a jobId immediately and later sends a completion message to this same session to continue your review. The script should return compact summaries/handles. Full child sessions are available in Dirigent and inspect(). Do not poll from the LLM: use wait in the script. A script finishing does not stop children; explicitly wait if you want their results. Script error/timeout/cancellation stops its active assignments and preserves files. Default script deadline 3600 seconds. Closing/restarting Pi cancels scripts; Dirigent restart marks unfinished work interrupted and NEVER automatically replays side effects. Inspect previous work before recovery.

Review decisions belong to you across tool calls, not a pretend deterministic review() function. Example (replace model IDs and briefs):
const x = await agents.spawn({name:"X", model:"provider/model", thinking:"high", workspace:"new", prompt:"Self-contained X brief"});
const yz = await agents.spawn({name:"Y/Z", model:"provider/model", thinking:"xhigh", workspace:"current", prompt:"Self-contained Y/Z brief"});
return await agents.wait([x.runId, yz.runId], {mode:"all"});`;

type Pending = { resolve(value: any): void; reject(error: Error): void; timer: ReturnType<typeof setTimeout> };
type Job = { id: string; child: ChildProcess; cancel(): Promise<void> };

export default function registerAgents(pi: ExtensionAPI) {
  const pending = new Map<string, Pending>();
  const jobs = new Map<string, Job>();
  let closing = false;

  function request(ctx: ExtensionContext, jobId: string, method: string, args: unknown = {}): Promise<any> {
    return new Promise((resolve, reject) => {
      const requestId = randomUUID();
      const timer = setTimeout(() => {
        pending.delete(requestId);
        reject(new Error(`Dirigent bridge timed out (${method}); inspect existing jobs/children before retrying side effects.`));
      }, 60_000);
      pending.set(requestId, { resolve, reject, timer });
      try {
        ctx.ui.setStatus("__dirigent_bridge__", JSON.stringify({ operation: "agents_request", requestId, jobId, method, args }));
      } catch (error) {
        clearTimeout(timer);
        pending.delete(requestId);
        reject(error);
      }
    });
  }

  pi.registerCommand("dirigent-agents-response", {
    description: "Internal Dirigent delegation response",
    handler: async args => {
      const response = JSON.parse(args);
      const item = pending.get(response.requestId);
      if (!item) return;
      pending.delete(response.requestId);
      clearTimeout(item.timer);
      if (response.error) item.reject(new Error(response.error));
      else item.resolve(response.value);
    },
  });

  pi.registerCommand("dirigent-agents-stop", {
    description: "Stop this session's running delegation scripts",
    handler: async () => { await Promise.all([...jobs.values()].map(job => job.cancel())); },
  });

  pi.on("session_start", () => { closing = false; });
  // Navigation changes the manager's instructions. Do not wake an unrelated branch later.
  pi.on("session_before_tree", async () => {
    await Promise.all([...jobs.values()].map(job => job.cancel()));
  });
  pi.on("session_shutdown", async () => {
    closing = true;
    // Report best-effort, but never wait for RPC commands while Pi is tearing down.
    for (const job of jobs.values()) { void job.cancel(); }
    for (const item of pending.values()) {
      clearTimeout(item.timer);
      item.reject(new Error("Pi session shut down; workflow interrupted."));
    }
    pending.clear();
  });

  pi.registerTool({
    name: "dirigent_agents",
    label: "Delegate work",
    description: "Spawn and manage child agents. Only use when asked. Call with {} to read the API before first use.",
    promptSnippet: "Spawn and manage child agents. Only use when asked; call with {} to read the API first.",
    parameters: Type.Object({
      title: Type.Optional(Type.String({ description: "Workflow label; required with code", minLength: 1, maxLength: 160 })),
      code: Type.Optional(Type.String({ description: "Async TypeScript body; required with title. Read the API first by calling with {}.", minLength: 1, maxLength: 64000 })),
      mode: Type.Optional(Type.String({ enum: ["wait", "background"] })),
      timeout: Type.Optional(Type.Number({ minimum: 1, maximum: 86400, description: "Script deadline in seconds (default 3600)" })),
    }),
    async execute(toolCallId, params, signal, onUpdate, ctx) {
      // Reading documentation must not allocate jobs, start workers, or contact the host.
      if (Object.keys(params).length === 0) {
        return { content: [{ type: "text", text: GUIDE }], details: {} };
      }
      if (!params.title?.trim() || !params.code?.trim()) {
        throw new Error("Provide both title and code to execute a workflow, or call with {} to read the API.");
      }
      if (ctx.mode !== "rpc") throw new Error("This bundled tool requires Dirigent's RPC host.");
      if (closing || signal?.aborted) throw new Error("Cancelled.");
      if (jobs.size >= 8) throw new Error("At most eight running scripts per session.");
      const jobId = randomUUID();
      const originEntry = ctx.sessionManager.getLeafId();
      await request(ctx, jobId, "job_start", { title: params.title, toolCallId });
      if (closing || signal?.aborted) {
        await request(ctx, jobId, "cancel_job");
        throw new Error("Cancelled.");
      }
      const child = spawn(process.execPath, ["--disable-warning=ExperimentalWarning", join(dirname(fileURLToPath(import.meta.url)), "dirigent-agent-worker.mjs")], {
        cwd: ctx.cwd, stdio: ["ignore", "pipe", "pipe", "ipc"], windowsHide: true,
      });
      let log = "";
      let finished = false;
      let resolveDone!: (value: { status: string; result: string }) => void;
      const done = new Promise<{ status: string; result: string }>(resolve => { resolveDone = resolve; });
      const deadline = setTimeout(() => { void finish("failed", "Workflow exceeded its deadline."); }, (params.timeout ?? 3600) * 1000);
      const abort = () => { void finish("cancelled", "Workflow cancelled."); };
      const finish = async (status: string, result: string) => {
        if (finished) return;
        finished = true;
        clearTimeout(deadline);
        signal?.removeEventListener("abort", abort);
        child.kill();
        // Generated code may ignore SIGTERM. Esc/stop must still reclaim its process.
        const killTimer = setTimeout(() => child.kill("SIGKILL"), 1000);
        killTimer.unref();
        child.once("exit", () => clearTimeout(killTimer));
        jobs.delete(jobId);
        const output = `${result}${log ? `\n\nScript output:\n${log}` : ""}`.slice(0, 32000);
        if (closing) {
          void request(ctx, jobId, "cancel_job").catch(() => {});
          resolveDone({ status, result: output });
          return;
        }
        try {
          if (status !== "completed") await request(ctx, jobId, "cancel_job", { status });
          await request(ctx, jobId, "job_finish", { status, result: output });
        } catch { /* The saved running record becomes interrupted after host restart. */ }
        resolveDone({ status, result: output });
        if (params.mode === "background" && !closing && status !== "cancelled") {
          try {
            const onBranch = !originEntry || ctx.sessionManager.getBranch().some(entry => entry.id === originEntry);
            if (onBranch) pi.sendMessage({
              customType: "dirigent-workflow", display: true,
              content: `Workflow ${params.title} (${jobId}) ${status}.\n${output}\nInspect child changes and continue the requested review/feedback workflow.`,
              details: { jobId, status },
            }, { triggerTurn: true, deliverAs: "followUp" });
          } catch { /* A replaced session must not receive this completion. */ }
        }
      };
      const record = (chunk: Buffer) => {
        if (log.length < 8000) log = (log + chunk.toString()).slice(0, 8000);
      };
      child.stdout?.on("data", record);
      child.stderr?.on("data", record);
      child.on("error", error => { void finish("failed", error.message); });
      child.on("exit", (code, signal) => { void finish("failed", `Script exited without a result (${code ?? signal}).`); });
      child.on("message", async (message: any) => {
        if (finished) return;
        if (message.type === "done") {
          void finish("completed", message.result + (message.truncated ? "\n[Result truncated]" : ""));
        } else if (message.type === "failed") {
          void finish("failed", message.error);
        } else if (message.type === "request") {
          try {
            let value: any;
            if (message.method === "models") {
              value = ctx.modelRegistry.getAvailable().map(model => ({ id: `${model.provider}/${model.id}`, name: model.name, reasoning: model.reasoning }));
            } else {
              if (message.method === "spawn") {
                const modelId = message.args?.model;
                if (!ctx.modelRegistry.getAvailable().some(model => `${model.provider}/${model.id}` === modelId)) {
                  throw new Error(`Unavailable model ${modelId}; use agents.models() and an exact provider/model ID.`);
                }
              }
              if (!["spawn", "send", "inspect", "list", "jobs", "stop", "runs"].includes(message.method)) throw new Error("Unknown agents operation.");
              value = await request(ctx, jobId, message.method, message.args);
            }
            if (!finished && child.connected) child.send({ type: "response", id: message.id, value }, error => {
              if (error) void finish("failed", error.message);
            });
          } catch (error) {
            if (!finished && child.connected) child.send({ type: "response", id: message.id, error: String(error) }, () => {});
          }
        }
      });
      jobs.set(jobId, { id: jobId, child, cancel: () => finish("cancelled", "Workflow cancelled.") });
      signal?.addEventListener("abort", abort, { once: true });
      if (signal?.aborted) abort();
      else child.send({ type: "start", code: params.code }, error => {
        if (error) void finish("failed", error.message);
      });
      if (params.mode === "background") {
        // A later manager turn's cancellation should not accidentally cancel an earlier detached job.
        signal?.removeEventListener("abort", abort);
        return { content: [{ type: "text", text: `Started background workflow ${params.title}. Job ID: ${jobId}. Completion will return here automatically; do not poll.` }], details: { jobId } };
      }
      onUpdate?.({ content: [{ type: "text", text: `${params.title} · waiting for workflow ${jobId}` }], details: { jobId } });
      const result = await done;
      if (result.status !== "completed") throw new Error(`${result.status}: ${result.result}`);
      return { content: [{ type: "text", text: result.result }], details: { jobId, status: result.status } };
    },
  });
}
