// dirigent runtime: newline-framed JSON on stdin/stdout, one pi session per thread.
// stdout is protocol only; stderr is for failures (the service logs each line as a
// warning). EOF on stdin = shutdown.
//
// Started as `dirigent_runtime pi [args…]` it is pi's own CLI instead (`dirigent pi`): the
// bundled version, on the agent dir the service gives it (`PI_CODING_AGENT_DIR`).

// What pi's own Bun binary sets up first: its OAuth flows (Anthropic, Codex, Copilot sign-ins)
// and Bedrock, which pi loads through variable imports `bun build --compile` can't follow, and
// the QuickJS wasm codemode scripts run in, embedded. It also names the process `pi` (not
// visible in `ps`) and silences `process.emitWarning`. Not exported, hence the path.
import "../node_modules/@earendil-works/pi-coding-agent/dist/bun/runtime-setup.js";
import { main as piMain } from "@earendil-works/pi-coding-agent";
import { Logins, type Method } from "./auth";
import { type Attachment, Sessions, type TitleGen } from "./sessions";
import type { Ask, Notify } from "./tools";

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
  | { type: "list_models"; id: number }
  | { type: "generate_title"; id: number; title: TitleGen; text: string }
  | { type: "list_providers"; id: number }
  | { type: "login"; id: number; provider: string; method: Method }
  | { type: "login_answer"; id: number; n: number; value: string }
  | { type: "login_cancel"; id: number }
  | { type: "logout"; id: number; provider: string };

const send = (msg: unknown) => process.stdout.write(JSON.stringify(msg) + "\n");
/** Answer request `id` with what `work` gives (`reply`), or why it failed (`failed`). The
 *  service waits on these by id: they run side by side and finish in any order. */
function reply(id: number, work: Promise<unknown>): Promise<void> {
  return work.then(
    (value) => void send({ type: "reply", id, value: value ?? null }),
    (e) => void send({ type: "failed", id, message: (e as Error).message ?? String(e) }),
  );
}

async function handle(sessions: Sessions, logins: Logins, msg: Inbound): Promise<void> {
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
      return reply(msg.id, sessions.listModels());
    case "generate_title":
      return reply(msg.id, sessions.generateTitle(msg.title, msg.text));
    case "list_providers":
      return reply(msg.id, logins.providers());
    // Its steps go out as `login_step`s under its id until the reply ends it.
    case "login":
      return reply(msg.id, logins.login(msg.id, msg.provider, msg.method));
    case "login_answer":
      return logins.answer(msg.id, msg.n, msg.value);
    case "login_cancel":
      return logins.cancel(msg.id);
    case "logout":
      return reply(msg.id, logins.logout(msg.provider));
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
  const logins = new Logins(sessions.modelRuntime, (id, step) =>
    send({ type: "login_step", id, step }),
  );
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
      handle(sessions, logins, msg).catch((e) => {
        const threadId = "thread_id" in msg ? msg.thread_id : undefined;
        console.error(`${msg.type} ${threadId ?? ""} failed:`, e);
        send({ type: "error", thread_id: threadId, message: (e as Error).message ?? String(e) });
      });
    }
    if (start < chunk.length) partial.push(chunk.subarray(start));
  }

  await sessions.disposeAll();
  process.exit(0);
}

if (process.argv[2] === "pi") {
  // The rest of what pi's own entry point sets up before `main` (`dist/cli/setup.js`, not
  // exported); `runtime-setup.js` above did the title and warnings.
  process.env.PI_CODING_AGENT = "true";
  process.env.AI_AGENT = "pi";
  await piMain(process.argv.slice(3));
} else {
  // Codemode scripts share this many workers across all threads; pi's default of 2 suits
  // one session. A worker holds a slot while its script runs, mostly waiting on tools, and
  // its VM may grow to 256 MB.
  process.env.PI_CODEMODE_MAX_WORKERS ??= "32";
  await serve();
}
