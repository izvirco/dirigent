// Runs generated TypeScript in a separate Node process using Pi's own Node executable.
// This is fault isolation, NOT a sandbox: scripts have the user's normal permissions.
import { stripTypeScriptTypes } from "node:module";

const pending = new Map();
let sequence = 0;
function request(method, args = {}) {
  return new Promise((resolve, reject) => {
    const id = String(++sequence);
    pending.set(id, { resolve, reject });
    process.send({ type: "request", id, method, args });
  });
}

export function createAgents(call, sleep = ms => new Promise(resolve => setTimeout(resolve, ms))) {
  return Object.freeze({
    models: () => call("models"),
    spawn: options => call("spawn", options),
    send: (agentId, prompt) => call("send", { agentId, prompt }),
    inspect: (agentId, options = {}) => call("inspect", { agentId, ...options }),
    list: () => call("list"),
    jobs: () => call("jobs"),
    async pingParent(message) {
      if (typeof message !== "string" || !message.trim() || Buffer.byteLength(message, "utf8") > 8000) {
        throw new Error("pingParent expects a nonempty message up to 8000 bytes.");
      }
      return await call("ping_parent", { message });
    },
    async stop(agentId) {
      await call("stop", { agentId });
      for (let attempt = 0; attempt < 60; attempt++) {
        if (!(await call("inspect", { agentId })).cancelling) return;
        await sleep(500);
      }
      throw new Error("Child cancellation has not settled; inspect it before sending more work.");
    },
    async wait(runIds, { mode = "all", timeout = 3600 } = {}) {
      if (!Array.isArray(runIds) || !runIds.length || runIds.length > 64 || !runIds.every(id => typeof id === "string")) {
        throw new Error("wait expects 1–64 runId strings, not agent IDs or handles.");
      }
      if (!["any", "all"].includes(mode)) throw new Error("wait mode must be any or all.");
      if (!Number.isFinite(timeout) || timeout <= 0) throw new Error("wait timeout must be positive seconds.");
      const deadline = Date.now() + timeout * 1000;
      for (;;) {
        const results = await call("runs", { runIds });
        const finished = results.filter(result => result.run.status !== "running");
        if (mode === "any" ? finished.length > 0 : finished.length === results.length) return finished;
        if (Date.now() >= deadline) throw new Error("Timed out waiting for assignments.");
        await sleep(750);
      }
    },
  });
}

export function compileWorkflow(code) {
  // Wrapping before stripping permits return/await and ordinary erasable TypeScript syntax.
  const source = stripTypeScriptTypes(`async function workflow(agents) {\n${code}\n}`, { mode: "strip" });
  return new Function(`${source}\nreturn workflow;`)();
}

if (process.send) {
  process.on("disconnect", () => process.exit(1));
  let started = false;
  process.on("message", async message => {
    if (message.type === "response") {
      const item = pending.get(message.id);
      if (!item) return;
      pending.delete(message.id);
      if (message.error) item.reject(new Error(message.error));
      else item.resolve(message.value);
    } else if (message.type === "start" && !started) {
      started = true;
      try {
        const value = await compileWorkflow(message.code)(createAgents(request));
        const result = JSON.stringify(value ?? null);
        process.send({ type: "done", result: result.slice(0, 24000), truncated: result.length > 24000 });
      } catch (error) {
        process.send({ type: "failed", error: String(error?.stack ?? error).slice(0, 8000) });
      }
    }
  });
}
