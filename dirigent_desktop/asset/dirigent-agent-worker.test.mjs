import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { compileWorkflow, createAgents } from "./dirigent-agent-worker.mjs";

const outcome = (id, status) => ({ agentId: id, run: { id, status, result: id } });

test("TypeScript can start concurrent assignments, wait for any, and then join all", async () => {
  const calls = [];
  const snapshots = [
    [outcome("x", "running"), outcome("yz", "running")],
    [outcome("x", "completed"), outcome("yz", "running")],
    [outcome("x", "completed"), outcome("yz", "failed")],
  ];
  const api = createAgents(async (method, args) => {
    calls.push(method);
    if (method === "spawn") return { agentId: args.name, runId: args.name };
    if (method === "runs") return snapshots.shift();
    throw new Error(method);
  }, async () => {});
  const result = await compileWorkflow(`
    const x: {runId: string} = await agents.spawn({name: "x"});
    const yz = await agents.spawn({name: "yz"});
    const first = await agents.wait([x.runId, yz.runId], {mode: "any"});
    const all = await agents.wait([x.runId, yz.runId]);
    return {first, all};
  `)(api);
  assert.deepEqual(calls, ["spawn", "spawn", "runs", "runs", "runs"]);
  assert.equal(result.first.length, 1);
  assert.equal(result.all[1].run.status, "failed"); // failures resolve, not hang or disappear
});

test("sequential feedback keeps the session but waits on the new run ID", async () => {
  const calls = [];
  const api = createAgents(async (method, args) => {
    calls.push([method, args]);
    if (method === "spawn") return { agentId: "child", runId: "initial" };
    if (method === "send") return { agentId: args.agentId, runId: "feedback" };
    return args.runIds.map(id => outcome(id, "completed"));
  });
  await compileWorkflow(`
    const child = await agents.spawn({name: "x"});
    await agents.wait([child.runId]);
    const fix = await agents.send(child.agentId, "Fix the edge case");
    return await agents.wait([fix.runId]);
  `)(api);
  assert.deepEqual(calls.map(([method]) => method), ["spawn", "runs", "send", "runs"]);
  assert.equal(calls[2][1].agentId, "child");
  assert.deepEqual(calls[3][1].runIds, ["feedback"]);
});

test("invalid waits reject before issuing requests; stop waits for cancellation acknowledgement", async () => {
  let called = 0;
  const api = createAgents(async method => {
    called++;
    return method === "inspect" ? { cancelling: called < 3 } : null;
  }, async () => {});
  await assert.rejects(api.wait([]), /runId/);
  await assert.rejects(api.wait(["run"], { mode: "either" }), /mode/);
  assert.equal(called, 0);
  await api.stop("child");
  assert.equal(called, 3);
});

test("real worker executes TypeScript over IPC and exits when its host disconnects", async () => {
  const child = spawn(process.execPath, [fileURLToPath(new URL("./dirigent-agent-worker.mjs", import.meta.url))], {
    stdio: ["ignore", "ignore", "ignore", "ipc"],
  });
  try {
    child.send({ type: "start", code: "const models: unknown[] = await agents.models(); return models;" });
    const [request] = await once(child, "message");
    assert.equal(request.method, "models");
    child.send({ type: "response", id: request.id, value: ["test/model"] });
    const [result] = await once(child, "message");
    assert.equal(result.type, "done");
    assert.deepEqual(JSON.parse(result.result), ["test/model"]);
    const exited = once(child, "exit");
    child.disconnect();
    await exited;
  } finally {
    child.kill("SIGKILL");
  }
});
