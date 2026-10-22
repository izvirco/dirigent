// Workflow runner, driven by the service (`workflows.rs`):
//   bun run workflow.ts describe <file>...   prints what each script declares, as JSON
//   bun run workflow.ts plan <file>          runs one against placeholders (`Dirigent.plan`)
//                                            and prints the steps it would take, as JSON
//   bun run workflow.ts run <file>           runs one; DIRIGENT_URL / _RUN / _PROJECT /
//                                            _PARENT / _INPUT say for whom and with what
//
// A workflow is a TypeScript file in `<project>/.dirigent/workflows/` whose default export
// is a `WorkflowDef`: a name, a description, the inputs it takes and `run(d, input)`,
// where `d` is a `Dirigent` (see `client.ts`) acting as the run. The script is plain code
// (spawn agents, wait for them, read their replies, branch, loop); every `d` call is
// reported as a step, which is what the desktop shows as the run's trace.

import { basename, resolve } from "node:path";
import { Client, Dirigent } from "./client";

export type { Dirigent } from "./client";

/** What a workflow script's default export must look like. */
export interface WorkflowDef<I extends Record<string, unknown> = Record<string, unknown>> {
  /** Defaults to the file name. */
  name?: string;
  description?: string;
  /** Inputs by name; the desktop builds a form from them, the service coerces values to `type`. */
  input?: Record<
    string,
    { type: "string" | "number" | "boolean"; description?: string; default?: unknown }
  >;
  run(d: Dirigent, input: I): Promise<unknown>;
}

interface Described {
  name: string;
  path: string;
  description: string;
  input: { name: string; kind: string; description: string; default?: unknown }[];
  error: string | null;
}

async function load(file: string): Promise<WorkflowDef> {
  const mod = await import(resolve(file));
  const def = mod.default as WorkflowDef | undefined;
  if (!def || typeof def !== "object" || typeof def.run !== "function")
    throw new Error("default export must be { name?, description?, input?, run(d, input) }");
  return def;
}

async function describe(files: string[]): Promise<void> {
  const out: Described[] = [];
  for (const file of files) {
    const stem = basename(file).replace(/\.ts$/, "");
    try {
      const def = await load(file);
      out.push({
        name: def.name ?? stem,
        path: file,
        description: def.description ?? "",
        input: Object.entries(def.input ?? {}).map(([name, p]) => ({
          name,
          kind: p.type,
          description: p.description ?? "",
          default: p.default,
        })),
        error: null,
      });
    } catch (e) {
      out.push({ name: stem, path: file, description: "", input: [], error: (e as Error).message });
    }
  }
  process.stdout.write(JSON.stringify(out));
}

/** Apply defaults and coerce the raw input (strings from a form, or whatever the caller sent). */
function coerce(def: WorkflowDef, raw: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = { ...raw };
  for (const [name, p] of Object.entries(def.input ?? {})) {
    let v = raw[name];
    if (v === undefined || v === "") v = p.default;
    if (v === undefined) throw new Error(`input \`${name}\` is required`);
    if (p.type === "number" && typeof v !== "number") {
      const n = Number(v);
      if (Number.isNaN(n)) throw new Error(`input \`${name}\` must be a number`);
      v = n;
    } else if (p.type === "boolean" && typeof v !== "boolean") {
      v = v === "true" || v === "1" || v === 1;
    } else if (p.type === "string" && typeof v !== "string") {
      v = String(v);
    }
    out[name] = v;
  }
  return out;
}

/**
 * The path the script takes with placeholder results (see `Dirigent.plan`): the same
 * steps `run` reports, without touching the host. A script that throws (bad input, a
 * bug, a plan that never ends) fails here, before anything real starts.
 */
async function plan(file: string): Promise<void> {
  const def = await load(file);
  const input = coerce(def, JSON.parse(process.env.DIRIGENT_INPUT || "{}"));
  const steps: Record<string, unknown>[] = [];
  const d = Dirigent.plan(
    { runId: "plan", projectId: process.env.DIRIGENT_PROJECT || undefined },
    async (s) => {
      steps.push(s);
    },
  );
  steps.push({ kind: "start", name: def.name ?? basename(file, ".ts"), input });
  const result = await def.run(d, input);
  steps.push({ kind: "finish", result: result === undefined ? null : result });
  process.stdout.write(JSON.stringify(steps));
}

async function run(file: string): Promise<number> {
  const runId = process.env.DIRIGENT_RUN;
  if (!runId) throw new Error("DIRIGENT_RUN is not set");
  const client = await Client.connect();
  const report = (step: Record<string, unknown>) =>
    client.request({ type: "report_step", run_id: runId, step }).then(() => {});
  const d = new Dirigent(
    client,
    { runId, projectId: process.env.DIRIGENT_PROJECT || undefined },
    report,
  );
  try {
    const def = await load(file);
    const input = coerce(def, JSON.parse(process.env.DIRIGENT_INPUT || "{}"));
    await report({ kind: "start", name: def.name ?? basename(file, ".ts"), input });
    const result = await def.run(d, input);
    await report({ kind: "finish", result: result === undefined ? null : result });
    return 0;
  } catch (e) {
    const message = (e as Error).stack ?? String(e);
    console.error(message);
    await report({ kind: "error", message: (e as Error).message ?? String(e) }).catch(() => {});
    return 1;
  } finally {
    client.close();
  }
}

const [command, ...args] = process.argv.slice(2);
if (command === "describe") {
  await describe(args);
} else if (command === "plan" && args[0]) {
  await plan(args[0]);
} else if (command === "run" && args[0]) {
  process.exit(await run(args[0]));
} else {
  console.error("usage: workflow.ts describe <file>... | plan <file> | run <file>");
  process.exit(2);
}
