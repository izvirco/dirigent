// dirigent runtime: newline-framed JSON on stdin/stdout, one pi session per thread.
// stdout is protocol only; logs go to stderr. EOF on stdin = shutdown.

import { Sessions } from "./sessions";

type Inbound =
  | {
      type: "load";
      thread_id: string;
      cwd: string;
      entries: any[];
      model?: string;
      thinking_level?: string;
    }
  | { type: "unload"; thread_id: string }
  | {
      type: "prompt";
      thread_id: string;
      text: string;
      mode: "prompt" | "steer" | "follow_up";
      model?: string;
      thinking_level?: string;
      images?: { mime_type: string; data: string }[];
    }
  | { type: "abort"; thread_id: string }
  | { type: "cancel_queued"; thread_id: string; text: string }
  | { type: "list_models" };

const send = (msg: unknown) => process.stdout.write(JSON.stringify(msg) + "\n");
const sessions = new Sessions(send);

async function handle(msg: Inbound): Promise<void> {
  switch (msg.type) {
    case "load":
      return sessions.load(msg.thread_id, msg.cwd, msg.entries, msg.model, msg.thinking_level);
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
      );
    case "abort":
      return sessions.abort(msg.thread_id);
    case "cancel_queued":
      return sessions.cancelQueued(msg.thread_id, msg.text);
    case "list_models":
      return sessions.listModels();
    default:
      throw new Error(`unknown message type: ${(msg as { type: string }).type}`);
  }
}

// Decode stdin into lines, dispatching each message without awaiting it so a
// long `prompt` never blocks a concurrent `abort`.
let buf = "";
for await (const chunk of Bun.stdin.stream()) {
  buf += new TextDecoder().decode(chunk);
  let nl: number;
  while ((nl = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, nl).trim();
    buf = buf.slice(nl + 1);
    if (!line) continue;
    let msg: Inbound;
    try {
      msg = JSON.parse(line);
    } catch {
      send({ type: "error", message: `bad json: ${line.slice(0, 80)}` });
      continue;
    }
    handle(msg).catch((e) => {
      const threadId = "thread_id" in msg ? msg.thread_id : undefined;
      console.error(`[runtime] ${msg.type} ${threadId ?? ""} failed:`, e);
      send({ type: "error", thread_id: threadId, message: (e as Error).message ?? String(e) });
    });
  }
}

sessions.disposeAll();
process.exit(0);
