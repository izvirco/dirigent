// Uses Pi's existing jiti/typebox dependencies via NODE_PATH; never starts Pi or Dirigent.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import test from "node:test";

const require = createRequire(import.meta.url);
const { createJiti } = require("jiti");
const jiti = createJiti(import.meta.url, { fsCache: false, alias: { typebox: require.resolve("typebox") } });
const { default: registerAgents } = await jiti.import("./dirigent-agents.ts");

function host() {
  const commands = new Map();
  const events = new Map();
  const calls = [];
  const messages = [];
  let tool;
  const branch = [{ id: "launch" }];
  const ctx = {
    mode: "rpc", cwd: process.cwd(),
    sessionManager: { getLeafId: () => "launch", getBranch: () => branch },
    modelRegistry: { getAvailable: () => [{ provider: "test", id: "model" }] },
    ui: {
      setStatus(_key, text) {
        const request = JSON.parse(text);
        calls.push(request);
        let value;
        switch (request.method) {
          case "spawn": value = { agentId: "2", runId: "initial" }; break;
          case "send": value = { agentId: "2", runId: "feedback" }; break;
          case "ping_parent": value = { queued: true }; break;
          case "job_start": case "job_finish": case "cancel_job": value = null; break;
          default: throw new Error(`Unexpected bridge request: ${request.method}`);
        }
        queueMicrotask(() => commands.get("dirigent-agents-response").handler(JSON.stringify({ requestId: request.requestId, value })));
      },
    },
  };
  registerAgents({
    registerCommand: (name, command) => commands.set(name, command),
    registerTool: definition => { tool = definition; },
    on: (name, handler) => events.set(name, handler),
    sendMessage: (message, options) => messages.push({ message, options }),
  });
  return {
    calls, messages, branch,
    execute: (params, signal = new AbortController().signal) => tool.execute("tool-call", params, signal, undefined, ctx),
    event: name => events.get(name)({}, ctx),
    command: (name, args = "") => commands.get(name).handler(args, ctx),
    notify: (jobId, runId = "initial", status = "completed") => commands.get("dirigent-agents-notify").handler(JSON.stringify({
      jobId, agentId: "2", name: "Implement X",
      run: { id: runId, status, result: "Done", parentMessage: "Ready for review" },
    }), ctx),
  };
}

const launch = {
  title: "Implement X", mode: "handoff",
  code: 'return await agents.spawn({name:"Implement X", model:"test/model", thinking:"high", workspace:"current", prompt:"Implement X"});',
};

test("handoff ends after scheduling, then reports behind human chat and supports feedback", { timeout: 5000 }, async () => {
  const h = host();
  const controller = new AbortController();
  const result = await h.execute(launch, controller.signal);
  assert.equal(result.terminate, true);
  assert.equal(result.details.handoff, true);
  assert.deepEqual(h.calls.map(call => call.method), ["job_start", "spawn", "job_finish"]);
  assert.equal(h.calls[0].args.handoff, true);
  assert.equal(h.calls[2].args.status, "completed");
  assert.deepEqual(h.messages, []); // Script completion itself must not wake the parent.

  controller.abort(); // The old tool signal no longer owns the children.
  assert.equal(h.calls.length, 3);
  h.branch.push({ id: "human-chat" });
  await h.notify(result.details.jobId);
  assert.equal(h.messages.length, 1);
  assert.deepEqual(h.messages[0].options, { triggerTurn: true, deliverAs: "followUp" });
  assert.equal(h.messages[0].message.content, "Child Implement X (2), run initial: completed.\nReady for review");
  assert.equal(h.messages[0].message.details.agentId, "2");

  const feedback = await h.execute({ title: "Fix X", mode: "handoff", code: 'return await agents.send("2", "Fix the edge case");' });
  assert.equal(feedback.terminate, true);
  assert.deepEqual(h.calls[4].args, { agentId: "2", prompt: "Fix the edge case" });
  await h.notify(feedback.details.jobId, "feedback", "failed");
  assert.equal(h.messages[1].message.details.run.status, "failed");
});

test("a child can ping its parent and end its own turn without spawning or waiting", { timeout: 5000 }, async () => {
  const h = host();
  const result = await h.execute({ title: "Ask parent", mode: "handoff", code: 'return await agents.pingParent("Which behavior should I preserve?");' });
  assert.equal(result.terminate, true);
  assert.deepEqual(h.calls.map(call => call.method), ["job_start", "ping_parent", "job_finish"]);
  assert.deepEqual(h.calls[1].args, { message: "Which behavior should I preserve?" });
});

test("failed handoffs return control for recovery and disarm child wake-ups", { timeout: 5000 }, async () => {
  const h = host();
  await assert.rejects(h.execute({ ...launch, code: launch.code.replace("return await", "await") + '\nthrow new Error("Launch failed");' }), /Launch failed/);
  assert.deepEqual(h.calls.map(call => call.method), ["job_start", "spawn", "cancel_job", "job_finish"]);
  await h.notify(h.calls[0].jobId);
  assert.deepEqual(h.messages, []);
});

test("stop, navigation, shutdown, and unrelated branches cannot wake stale handoffs", { timeout: 5000 }, async () => {
  for (const action of ["stop", "session_before_tree", "session_shutdown", "other-branch"]) {
    const h = host();
    const result = await h.execute(launch);
    if (action === "stop") await h.command("dirigent-agents-stop");
    else if (action === "other-branch") h.branch.splice(0);
    else await h.event(action);
    await h.notify(result.details.jobId);
    assert.deepEqual(h.messages, [], action);
  }
});

test("ordinary workflows still return to the model", { timeout: 5000 }, async () => {
  const h = host();
  const result = await h.execute({ title: "Get result", code: "return 42;" });
  assert.notEqual(result.terminate, true);
  assert.equal(h.calls[0].args.handoff, false);
  await h.notify(result.details.jobId);
  assert.deepEqual(h.messages, []);
});
