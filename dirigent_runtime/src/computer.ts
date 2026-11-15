// Computer use: the `computer` tool opens a screen for the agent to run and operate GUI apps
// on, when a task needs one. Nothing runs until it is called. A screen is one of two, and
// the agent picks (the user may say which):
//
//   private   an X display of the thread's own (Xvfb), which nobody sees but in the thread:
//             the place for the agent's own work (trying an app, testing a UI). Linux only,
//             and only where there is an `Xvfb`; where it can't be made the desktop stands
//             in, and the agent is told so.
//   desktop   the desktop of the machine the thread runs on, the user's own.
//
// Each is operated through Cua Driver (https://github.com/trycua/cua, MIT), started as an
// MCP server of the session when its screen opens: `private_screen` or `user_desktop`. The model lists windows, reads a window's accessibility tree and
// screenshot, clicks and types, on the desktop in the background where the app allows.
// Installs ship the driver next to the runtime (`build.ts`), the nix package Xvfb too; what
// isn't shipped is taken from PATH (`program`): both in a dev checkout (the nix dev shell
// has them), the driver on macOS, Xvfb in a Linux install (`packaging/README.md`).
//
// The thread shows what the agent does there. The driver's own recorder writes a folder per
// action (`turn-NNNNN/`: the target window before and after, and what was done where),
// which `Recording` reads while a tool of that screen runs and hands on as `screen`
// messages: the window's picture when it changed, and each action with where it clicked.
// The recorder only keeps the calls of the connection that started it, and that connection
// is pi's: `recorded` changes it to start the recording from inside. The pictures the agent
// takes itself are shown as well: the recorder's is taken as the action ends, before an
// app that draws slowly (in software, on a private screen) shows what it did.

import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import {
  type AgentSessionEvent,
  createMcpExtension,
  defineTool,
  type ExtensionAPI,
  type InlineExtension,
  type McpServerConfig,
  type McpTransportFactory,
  type ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
// Not exported, hence the path: what the MCP extension makes a server's connection with
// unless told otherwise.
import { createDefaultTransport } from "../node_modules/@earendil-works/pi-coding-agent/dist/extensions/mcp/runtime.js";
import { dataDir } from "./tools";

/** The tool's name, and its switch in a thread's `tools`. */
export const COMPUTER = "computer";

/** Where a screen is: a display of the thread's own, or the user's desktop. */
type Where = "private" | "desktop";
/**
 * The MCP server each is operated through: its tools are `mcp__private_screen__…` and
 * `mcp__user_desktop__…`. Names a server of the user's own is unlikely to have: one in
 * `mcp.json` by the same name would take ours' place.
 */
const SERVER: Record<Where, string> = { private: "private_screen", desktop: "user_desktop" };

/** The JSON-RPC id of our own call on pi's connection (`recorded`); pi's are numbers. */
const RECORDING_ID = "dirigent-recording";
/** The driver's tools that answer with a picture of what the recorder shows: the screen, a window. */
const LOOKS = new Set(["get_desktop_state", "get_window_state"]);
/** How long the first tool call waits for the recording to start before going ahead without. */
const RECORDING_START_MS = 3000;
/** How often the recording is read while a computer tool runs. */
const POLL_MS = 150;
/** The longest text an action's label quotes. */
const LABEL_CHARS = 48;
/** A private screen's size unless the agent asks for another. */
const WIDTH = 1280;
const HEIGHT = 800;
/** How long Xvfb gets to come up, and the driver to say its version. */
const START_MS = 5000;

/** A step of a thread's computer use, as the service takes it (`RuntimeMessage::Screen`). */
export interface ScreenUpdate {
  /** The window's picture, when it changed. */
  image?: { mime_type: string; data: string };
  /** What the agent just did, in a few words. */
  action?: string;
  /** Where it clicked, as fractions of the picture's width and height. */
  cursor?: [number, number];
  /** It happened on the user's desktop, not on a screen of the thread's own. */
  desktop?: boolean;
}

type Transport = ReturnType<typeof createDefaultTransport>;
type JsonRpc = Parameters<Transport["send"]>[0];

/** The program `name`: the one shipped next to the runtime, else one on PATH. */
function program(name: string): string | undefined {
  const shipped = join(
    dirname(process.execPath),
    process.platform === "win32" ? `${name}.exe` : name,
  );
  return existsSync(shipped) ? shipped : (Bun.which(name) ?? undefined);
}

let found: Promise<string> | undefined;

/**
 * The driver's executable, asked for its version once per runtime. Throws what the agent
 * is told when this machine has none that runs (macOS without the user's install, a Linux
 * without the X libraries it loads).
 */
function driver(): Promise<string> {
  return (found ??= (async () => {
    const command = program("cua-driver");
    if (!command)
      throw new Error(
        "Cua Driver, which operates the screen, isn't installed on this machine (`cua-driver` is not on PATH: https://cua.ai/docs/cua-driver/quickstart)",
      );
    const check = Bun.spawn([command, "--version"], { stdout: "ignore", stderr: "pipe" });
    const timeout = setTimeout(() => check.kill(), START_MS);
    const code = await check.exited;
    clearTimeout(timeout);
    if (code === 0) return command;
    const said = (await new Response(check.stderr).text()).trim().split("\n").pop();
    throw new Error(`Cua Driver doesn't run on this machine (${command}, exit ${code}): ${said}`);
  })());
}

/** A private screen's X server: its display (`:N`) and how to end it. */
interface Display {
  name: string;
  end: () => void;
}

/**
 * Start an X display nobody sees (Xvfb), on a number it picks. A shell holds it and ends it
 * when its stdin closes: by `end`, or with this runtime, however it goes (a runtime is
 * killed, not shut down). Its apps go with it. Throws why there is none.
 */
async function startDisplay(width: number, height: number): Promise<Display> {
  if (process.platform !== "linux") throw new Error("private screens are made on Linux only");
  const xvfb = program("Xvfb");
  if (!xvfb) throw new Error("Xvfb is not installed");
  const hold = '"$@" & xvfb=$!; cat >/dev/null; kill "$xvfb" 2>/dev/null';
  const screen = ["-screen", "0", `${width}x${height}x24`, "-nolisten", "tcp"];
  // `-displayfd 1`: it writes its number to stdout once it takes connections.
  const proc = Bun.spawn(["sh", "-c", hold, "sh", xvfb, "-displayfd", "1", ...screen], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "ignore",
  });
  const end = () => void proc.stdin.end();
  const reader = proc.stdout.getReader();
  const timeout = setTimeout(end, START_MS);
  let said = "";
  while (!said.includes("\n")) {
    const { done, value } = await reader.read();
    if (done) break;
    said += new TextDecoder().decode(value);
  }
  clearTimeout(timeout);
  reader.releaseLock();
  const number = said.trim();
  if (!/^\d+$/.test(number)) {
    end();
    throw new Error("Xvfb did not start");
  }
  return { name: `:${number}`, end };
}

/**
 * A screen's server entry, as one in `mcp.json` would be. Telemetry and the update check
 * are the driver's own and off: an install is updated with dirigent. So is the arrow the
 * driver draws on the screen itself for its agent (`--no-overlay`): the thread shows where
 * the agent clicks, and the user's desktop stays theirs. Its state lives in `home`, except
 * on macOS, where the driver in use is the user's own install and `mcp` talks to its daemon.
 * A private screen's driver, and the apps it launches, get that display and no Wayland one.
 */
function serverConfig(
  command: string,
  where: Where,
  home: string,
  display?: string,
): McpServerConfig {
  const env: Record<string, string> = {
    CUA_TELEMETRY_ENABLED: "0",
    CUA_DRIVER_RS_UPDATE_CHECK: "0",
  };
  if (process.platform !== "darwin") env.CUA_DRIVER_RS_HOME = home;
  if (display)
    Object.assign(env, { DISPLAY: display, WAYLAND_DISPLAY: "", XDG_SESSION_TYPE: "x11" });
  const does =
    "list and launch apps, read a window's accessibility tree and screenshot, click, type, scroll and press keys";
  const description =
    where === "private"
      ? `Operate the apps on this thread's private screen: ${does}.`
      : `Operate the apps on the user's desktop: ${does}, in the background where the app allows.`;
  return { command, args: ["mcp", "--no-overlay"], env, description };
}

/** How a screen's tools are used, for the model that just opened it. */
function how(server: string): string {
  return (
    `Operate it from codemode with the \`mcp__${server}__…\` tools: \`describeNamespace("mcp__${server}")\` has the driver's guide and the tool names. ` +
    "A screenshot is an image block of a tool's result: pass it to `image()` to see it. The user watches what you do there in this thread."
  );
}

const parameters = Type.Object({
  screen: Type.Union([Type.Literal("private"), Type.Literal("desktop")], {
    description:
      "`private`: an invisible screen of this thread's own, for your own work (trying an app or a page, testing a UI). `desktop`: the user's real desktop, when they ask for it or the task is about what is on their screen.",
  }),
  action: Type.Optional(
    Type.Union([Type.Literal("open"), Type.Literal("close")], {
      description: "`open` (default) the screen, or `close` it and what runs on it.",
    }),
  ),
  width: Type.Optional(
    Type.Number({ description: `A private screen's width in pixels (default ${WIDTH}).` }),
  ),
  height: Type.Optional(
    Type.Number({ description: `A private screen's height in pixels (default ${HEIGHT}).` }),
  ),
});

/** The picture a thread last sent, whichever of its screens it was of: the same again isn't sent. */
interface Shown {
  hash?: number | bigint;
}

/** A screen the thread has open: its recording, and its display if it is a private one. */
interface Open {
  recording: Recording;
  display?: Display;
}

/** One thread's computer use: the `computer` tool, the screens it opened, and what they show. */
export class Computer {
  private pi?: ExtensionAPI;
  private open = new Map<Where, Open>();
  private root: string;
  private shown: Shown = {};
  /** Opens and closes take turns (`inTurn`): pi runs a batch of tool calls side by side. */
  private turn: Promise<unknown> = Promise.resolve();

  /** `update` takes the steps of what the agent does on its screens. */
  constructor(
    threadId: string,
    private update: (update: ScreenUpdate) => void,
  ) {
    this.root = join(dataDir(), "screens", threadId);
  }

  /**
   * The session's extensions: ours, only to have pi's handle (a screen's server is
   * registered when the screen opens), and pi's MCP extension, connecting those servers
   * through `recorded`.
   */
  extensions(): InlineExtension[] {
    const handle = {
      name: COMPUTER,
      hidden: true,
      factory: (pi: ExtensionAPI) => {
        this.pi = pi;
      },
    };
    const createTransport: McpTransportFactory = (entry, cwd, auth) => {
      const transport = createDefaultTransport(entry, cwd, auth);
      // A screen of ours that is open; a server of the user's by that name otherwise.
      const where = (Object.keys(SERVER) as Where[]).find((w) => SERVER[w] === entry.name);
      const open = where && this.open.get(where);
      return open ? recorded(transport, open.recording) : transport;
    };
    const mcp = {
      name: "mcp",
      factory: createMcpExtension({ createTransport }),
      replaceable: true,
      builtin: true,
    };
    return [handle, mcp];
  }

  /** The `computer` tool: opens a screen, or closes one. */
  tool(): ToolDefinition {
    return defineTool({
      name: COMPUTER,
      label: "Computer",
      description:
        "Open a screen to run and operate GUI apps on, when a task needs one: a `private` screen of this thread's own, or the user's `desktop`. " +
        "Prefer the private one for your own work; use the desktop when the user asks for it or the task is about what is on their screen. " +
        "Opening one gives you the tools to see and operate it.",
      promptSnippet:
        "Open a private screen, or the user's desktop, to run and operate GUI apps (see windows, click, type)",
      parameters,
      execute: async (_id, p) => {
        const said = await this.inTurn(() => {
          if (p.action !== "close")
            return this.openScreen(p.screen, p.width ?? WIDTH, p.height ?? HEIGHT);
          return this.close(p.screen)
            ? `Closed the ${p.screen} screen.`
            : `The ${p.screen} screen wasn't open.`;
        });
        return { content: [{ type: "text" as const, text: said }], details: undefined };
      },
    }) as ToolDefinition;
  }

  /** Run `work` once what was asked before it is done, whether that worked or not. */
  private inTurn<T>(work: () => T | Promise<T>): Promise<T> {
    const done = this.turn.then(work, work);
    this.turn = done.catch(() => {});
    return done;
  }

  /** Open the screen `where`, or the desktop in a private one's place where none can be made. */
  private async openScreen(where: Where, width: number, height: number): Promise<string> {
    if (!this.pi) throw new Error("the session is still loading");
    const command = await driver();
    let stoodIn = "";
    let display: Display | undefined;
    if (where === "private" && !this.open.has("private")) {
      try {
        display = await startDisplay(width, height);
      } catch (e) {
        where = "desktop";
        stoodIn = `No private screen can be made on this machine (${(e as Error).message}), so this is the user's desktop in its place: tell them so. `;
      }
    }
    const server = SERVER[where];
    const opened = this.open.get(where);
    if (opened) {
      const at = opened.display ? ` (\`DISPLAY=${opened.display.name}\`)` : "";
      return `${stoodIn}The ${where} screen is open already${at}. ${how(server)}`;
    }
    const sessionless =
      where === "desktop" &&
      process.platform === "linux" &&
      !process.env.DISPLAY &&
      !process.env.WAYLAND_DISPLAY;
    if (sessionless) throw new Error(`${stoodIn}This machine has no desktop session.`);
    const dir = join(this.root, where);
    const recording = new Recording(join(dir, "recording"), this.shown, (update) =>
      this.update(where === "desktop" ? { ...update, desktop: true } : update),
    );
    // Before the server: its connection is changed only for a screen that is open.
    this.open.set(where, { recording, display });
    try {
      const config = serverConfig(command, where, join(dir, "driver"), display?.name);
      this.pi.registerMcpServer(server, config);
    } catch (e) {
      this.close(where);
      throw e;
    }
    if (!display)
      return `${stoodIn}The user's desktop is open to you. ${how(server)} It is the screen they are working on: act only on what they asked for, and leave their other windows as they are.`;
    return (
      `Your private screen is open: an X display of this thread's own (\`DISPLAY=${display.name}\`, ${width}x${height}), which nobody sees but in this thread. ${how(server)}\n` +
      `Start apps on it with \`mcp__${server}__launch_app\`, or from bash, in the background, with \`env -u WAYLAND_DISPLAY DISPLAY=${display.name} <command>\`: anything started without that opens on the user's desktop. ` +
      'It has no window manager: windows have no title bars and stay where they open, and input aimed at a window (`pid`, `window_id`) doesn\'t reach every app. Aim at the screen instead: `get_desktop_state({})` for its picture, then `click` with `target: {kind: "desktop", display_id: "primary"}` and the `x`, `y` you read off that picture. `type_text`, `press_key`, `hotkey`, `scroll` and `drag` take the same `target`; keys go to the window under the pointer, so click into it first. Apps draw in software here and can take a moment to show what an action did: a picture taken in the same script as the action may show the screen as it was, so take it in your next call. ' +
      "The screen closes when the thread has been idle for a while; open it again then."
    );
  }

  /** Close the screen `where`: its server, its recording, its display and the apps on it. */
  private close(where: Where): boolean {
    const open = this.open.get(where);
    if (!open) return false;
    this.open.delete(where);
    try {
      this.pi?.unregisterMcpServer(SERVER[where]);
    } catch {}
    open.recording.stop();
    open.display?.end();
    return true;
  }

  /**
   * The tool's switch went off, or the session is going: every screen closes, one still
   * opening once it has.
   */
  closeAll(): void {
    void this.inTurn(() => {
      for (const where of this.open.keys()) this.close(where);
    });
  }

  /**
   * The session's events: a screen's recording is read while one of its tools runs, and no
   * longer once the run is over, whatever became of its calls (a stopped run may not end them).
   */
  observe(event: AgentSessionEvent): void {
    for (const [where, { recording }] of this.open) {
      const ours = "toolName" in event && event.toolName.startsWith(`mcp__${SERVER[where]}__`);
      if (event.type === "tool_execution_start" && ours) recording.calls(1);
      else if (event.type === "tool_execution_end" && ours) recording.calls(-1);
      else if (event.type === "agent_settled") recording.calls(0);
    }
  }
}

/**
 * Forget what a runtime before this one left of its screens (it is killed, not shut down,
 * so its sessions never close them): pictures of the user's windows don't stay on disk.
 */
export function forgetScreens(): void {
  rmSync(join(dataDir(), "screens"), { recursive: true, force: true });
}

/**
 * `transport` (pi's connection to a screen's driver), made to start the driver's recorder
 * ahead of the first tool call: our own `start_recording` call, whose answer pi never sees.
 * The call waits for it, so its own action is recorded; a recorder that won't start costs
 * only the picture. The pictures the agent asks for (`LOOKS`) are handed to `recording` on
 * their way to pi. Changed in place, not wrapped: pi tells a stdio transport by its class
 * (a failed connection's error then has the server's stderr).
 */
function recorded(transport: Transport, recording: Recording): Transport {
  const send = transport.send.bind(transport);
  const onMessage = transport.onMessage.bind(transport);
  let started: Promise<void> | undefined;
  /** The ids of the calls in flight that answer with a picture. */
  const looks = new Set<unknown>();
  const start = () =>
    new Promise<void>((resolve) => {
      let dir: string;
      try {
        dir = recording.begin();
      } catch (e) {
        console.error("computer: no directory to record into:", e);
        return resolve();
      }
      const timer = setTimeout(resolve, RECORDING_START_MS);
      const off = onMessage((message) => {
        if (!("id" in message) || message.id !== RECORDING_ID) return;
        off();
        clearTimeout(timer);
        const result = "result" in message ? (message.result as { isError?: boolean }) : undefined;
        const failed = "error" in message || result?.isError;
        if (failed) console.error(`computer: start_recording failed: ${JSON.stringify(message)}`);
        resolve();
      });
      const call: JsonRpc = {
        jsonrpc: "2.0",
        id: RECORDING_ID,
        method: "tools/call",
        // No accessibility walk around every action: the pictures are what we show.
        params: {
          name: "start_recording",
          arguments: { output_dir: dir, include_accessibility_tree: false },
        },
      };
      send(call).catch(() => resolve());
    });
  transport.send = async (message, options) => {
    if ("method" in message && message.method === "tools/call") {
      await (started ??= start());
      // Cancelled while it waited (the user stopped the run): the call never goes out.
      if (options?.signal?.aborted) return;
      const name = (message.params as { name?: string } | undefined)?.name;
      if ("id" in message && name && LOOKS.has(name)) looks.add(message.id);
    }
    return send(message, options);
  };
  transport.onMessage = (listener) =>
    onMessage((message) => {
      if ("id" in message && message.id === RECORDING_ID) return;
      listener(message);
      if ("id" in message && looks.delete(message.id) && "result" in message)
        recording.looked(message.result);
    });
  return transport;
}

/** The recorder's `action.json`, as far as we read it. */
interface Action {
  tool?: string;
  arguments?: Record<string, unknown>;
  result_error?: boolean;
  /** Where a click was aimed, in pixels of the picture taken before it. */
  click_point?: { x: number; y: number };
  /** That picture, when it isn't `before.png`. */
  click_point_image?: string;
}

/**
 * A recording's reader. The recorder (`start_recording`) makes a `turn-NNNNN/` folder per
 * action in `begin`'s directory: `before.png` (the target window) as the action starts,
 * then `action.json`, `after.png` and, last and written whole, `evidence.json` once it is
 * done. A turn is read when it starts (its picture, if the window has changed since the
 * last one shown) and when it is done (the action, and the picture after it), then
 * deleted. Turns may finish out of order.
 */
class Recording {
  private dir?: string;
  private recordings = 0;
  /** Turns whose `before.png` has been read. */
  private begun = new Set<string>();
  /** Turns read to their end: one whose folder won't go isn't read again. */
  private done = new Set<string>();
  private running = 0;
  private timer?: ReturnType<typeof setInterval>;

  constructor(
    private root: string,
    private shown: Shown,
    private update: (update: ScreenUpdate) => void,
  ) {}

  /** A new connection to the driver records into a fresh directory: turns count from 1 again. */
  begin(): string {
    this.dir = join(this.root, String(++this.recordings));
    this.begun.clear();
    this.done.clear();
    rmSync(this.dir, { recursive: true, force: true });
    mkdirSync(this.dir, { recursive: true });
    return this.dir;
  }

  /**
   * A computer tool call started (1) or ended (-1), or none runs any more (0): read while
   * any runs, and once after.
   */
  calls(change: 1 | -1 | 0): void {
    this.running = change === 0 ? 0 : Math.max(0, this.running + change);
    if (this.running > 0) {
      this.timer ??= setInterval(() => this.read(), POLL_MS);
      return;
    }
    clearInterval(this.timer);
    this.timer = undefined;
    this.read();
  }

  /** The agent took a picture (the `result` of one of `LOOKS`): it is shown. */
  looked(result: unknown): void {
    const content = (result as { content?: unknown } | null)?.content;
    if (!Array.isArray(content)) return;
    const data = content.find((b) => b?.type === "image" && b.mimeType === "image/png")?.data;
    const image = typeof data === "string" && this.changed(Buffer.from(data, "base64"));
    if (image) this.update({ image });
  }

  /** Nothing is read any more, and what was recorded goes. */
  stop(): void {
    clearInterval(this.timer);
    this.timer = undefined;
    this.dir = undefined;
    rmSync(this.root, { recursive: true, force: true });
  }

  private read(): void {
    if (!this.dir) return;
    let turns: string[];
    try {
      turns = readdirSync(this.dir).filter((name) => name.startsWith("turn-"));
    } catch (e) {
      return console.error("computer: reading the recording failed:", e);
    }
    for (const name of turns.sort()) {
      const turn = join(this.dir, name);
      // Each turn on its own: one that can't be read doesn't hold up those after it.
      try {
        if (this.done.has(name) || existsSync(join(turn, "evidence.json"))) {
          if (!this.done.has(name)) {
            this.done.add(name);
            this.begun.delete(name);
            this.finish(turn);
          }
          // A folder that won't go (a file of it still open, on Windows) is tried again.
          try {
            rmSync(turn, { recursive: true, force: true });
            this.done.delete(name);
          } catch {}
        } else if (!this.begun.has(name)) {
          const before = picture(join(turn, "before.png"));
          if (!before) continue;
          this.begun.add(name);
          const image = this.changed(before);
          if (image) this.update({ image });
        }
      } catch (e) {
        console.error(`computer: reading ${name} of the recording failed:`, e);
      }
    }
  }

  /** A finished turn: its action, where it clicked, and the window after it. */
  private finish(turn: string): void {
    const action = JSON.parse(readFileSync(join(turn, "action.json"), "utf8")) as Action;
    const update: ScreenUpdate = { action: label(action) };
    const aimed =
      action.click_point && picture(join(turn, action.click_point_image ?? "before.png"));
    const size = aimed && dimensions(aimed);
    // Not where a click was refused: a cursor there would say it happened.
    if (action.click_point && size && !action.result_error)
      update.cursor = [action.click_point.x / size[0], action.click_point.y / size[1]];
    const after = picture(join(turn, "after.png"));
    if (after) update.image = this.changed(after);
    this.update(update);
  }

  /** `png` as an image to send, unless it is the picture last sent. */
  private changed(png: Buffer): ScreenUpdate["image"] {
    const hash = Bun.hash(png);
    if (hash === this.shown.hash) return undefined;
    this.shown.hash = hash;
    return { mime_type: "image/png", data: png.toString("base64") };
  }
}

/** The last bytes of every PNG: its `IEND` chunk. */
const PNG_END = Buffer.from([0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82]);

/** The PNG at `path` once it is all there: the recorder writes `before.png` in place. */
function picture(path: string): Buffer | undefined {
  if (!existsSync(path)) return undefined;
  const png = readFileSync(path);
  return png.length > 24 && png.subarray(-PNG_END.length).equals(PNG_END) ? png : undefined;
}

/** A PNG's width and height, from its header. */
function dimensions(png: Buffer): [number, number] | undefined {
  const [width, height] = [png.readUInt32BE(16), png.readUInt32BE(20)];
  return width > 0 && height > 0 ? [width, height] : undefined;
}

/** What an action was, for the screen's caption: `click`, `type "hello"`, `key Return`. */
function label(action: Action): string {
  const args = action.arguments ?? {};
  const tool = (action.tool ?? "action").replaceAll("_", " ");
  const text = (value: unknown) => {
    const s = String(value).replace(/\s+/g, " ");
    return s.length > LABEL_CHARS ? `${s.slice(0, LABEL_CHARS)}…` : s;
  };
  let what: string;
  if (action.tool === "type_text" && args.text !== undefined) what = `type "${text(args.text)}"`;
  else if (action.tool === "press_key" && args.key !== undefined) what = `key ${text(args.key)}`;
  else if (action.tool === "hotkey" && Array.isArray(args.keys))
    what = `key ${text(args.keys.join("+"))}`;
  else if (action.tool === "scroll" && args.direction !== undefined)
    what = `scroll ${text(args.direction)}`;
  else if (action.tool === "set_value" && args.value !== undefined)
    what = `set "${text(args.value)}"`;
  else what = tool;
  return action.result_error ? `${what} (failed)` : what;
}
