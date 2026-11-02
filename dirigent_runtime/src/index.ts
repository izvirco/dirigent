// dirigent runtime: newline-framed JSON on stdin/stdout, one pi session per thread.
// stdout is protocol only; stderr is for failures (the service logs each line as a
// warning). EOF on stdin = shutdown.
//
// Started as `dirigent_runtime pi [args…]` it is pi's own CLI instead (`dirigent pi`): the
// bundled version, on the agent dir the service gives it (`PI_CODING_AGENT_DIR`).

import { registerBunOAuthFlows } from "@earendil-works/pi-ai/bun-oauth";
import { main as piMain } from "@earendil-works/pi-coding-agent";
import { type Attachment, Sessions, type TitleGen } from "./sessions";
import type { Ask, Notify } from "./tools";

// pi loads its OAuth flows (Anthropic, Codex, Copilot sign-ins) through a variable import
// that `bun build --compile` can't follow, so the compiled runtime would lack them. Register
// them statically, as pi's own binary does (`pi-coding-agent/dist/bun/runtime-setup.js`).
registerBunOAuthFlows();

type Inbound =
  | {
      type: "load";
      thread_id: string;
      cwd: string;
      entries: any[];
      model?: string;
      thinking_level?: string;
      tools?: string[];
    }
  | { type: "unload"; thread_id: string }
  | {
      type: "prompt";
      thread_id: string;
      text: string;
      mode: "prompt" | "steer" | "follow_up";
      model?: string;
      thinking_level?: string;
      images?: Attachment[];
      title?: TitleGen;
      tools?: string[];
      edit?: string;
    }
  | { type: "answer"; thread_id: string; ask: Ask; answers: string[][] | null }
  | {
      type: "notify";
      thread_id: string;
      custom_type: string;
      content: string;
      details: Record<string, unknown>;
      notify: Notify;
    }
  | { type: "abort"; thread_id: string }
  | { type: "cancel_queued"; thread_id: string; text: string }
  | { type: "list_models" }
  | { type: "generate_title"; id: number; title: TitleGen; text: string };

const send = (msg: unknown) => process.stdout.write(JSON.stringify(msg) + "\n");

async function handle(sessions: Sessions, msg: Inbound): Promise<void> {
  switch (msg.type) {
    case "load":
      return sessions.load(
        msg.thread_id,
        msg.cwd,
        msg.entries,
        msg.model,
        msg.thinking_level,
        msg.tools,
      );
    case "unload":
      return sessions.unload(msg.thread_id);
    case "prompt":
      return sessions.prompt(
        msg.thread_id,
        msg.text,
        msg.mode,
        msg.model,
        msg.thinking_level,
        msg.images,
        msg.title,
        msg.tools,
        msg.edit,
      );
    case "answer":
      return sessions.answer(msg.thread_id, msg.ask, msg.answers);
    case "notify":
      return sessions.notify(msg.thread_id, msg.custom_type, msg.content, msg.details, msg.notify);
    case "abort":
      return sessions.abort(msg.thread_id);
    case "cancel_queued":
      return sessions.cancelQueued(msg.thread_id, msg.text);
    case "list_models":
      return sessions.listModels();
    // Answered by `id`: titles are generated side by side and finish in any order.
    case "generate_title":
      return sessions.generateTitle(msg.title, msg.text).then(
        (r) => {
          send({ type: "title", id: msg.id, ...r });
        },
        (e) => {
          send({ type: "title_failed", id: msg.id, message: (e as Error).message ?? String(e) });
        },
      );
    default:
      throw new Error(`unknown message type: ${(msg as { type: string }).type}`);
  }
}

// Split stdin into lines, dispatching each message without awaiting it so a
// long `prompt` never blocks a concurrent `abort`. A `load` is one line of the
// whole thread, images inline (hundreds of MB): keep the bytes of an unfinished
// line and search only each new chunk for "\n", so reading stays linear. A line
// is decoded only once complete, so no UTF-8 character is split across chunks.
async function serve(): Promise<void> {
  const sessions = new Sessions(send);
  let partial: Uint8Array[] = [];
  const decoder = new TextDecoder();
  for await (const chunk of Bun.stdin.stream()) {
    let start = 0;
    let nl: number;
    while ((nl = chunk.indexOf(10, start)) >= 0) {
      partial.push(chunk.subarray(start, nl));
      const line = decoder.decode(Buffer.concat(partial)).trim();
      partial = [];
      start = nl + 1;
      if (!line) continue;
      let msg: Inbound;
      try {
        msg = JSON.parse(line);
      } catch {
        send({ type: "error", message: `bad json: ${line.slice(0, 80)}` });
        continue;
      }
      handle(sessions, msg).catch((e) => {
        const threadId = "thread_id" in msg ? msg.thread_id : undefined;
        console.error(`${msg.type} ${threadId ?? ""} failed:`, e);
        send({ type: "error", thread_id: threadId, message: (e as Error).message ?? String(e) });
      });
    }
    if (start < chunk.length) partial.push(chunk.subarray(start));
  }

  sessions.disposeAll();
  process.exit(0);
}

if (process.argv[2] === "pi") {
  // What pi's own entry point sets up before `main` (`dist/cli/setup.js`, not exported).
  process.title = "pi";
  process.env.PI_CODING_AGENT = "true";
  process.env.AI_AGENT = "pi";
  process.emitWarning = () => {};
  await piMain(process.argv.slice(3));
} else {
  await serve();
}
