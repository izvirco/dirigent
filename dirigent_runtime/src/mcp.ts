// MCP servers for the desktop's Settings › MCP servers: what `pi mcp add / remove / list /
// login / logout` do in a terminal, over the protocol, on the user-level `<agent dir>/mcp.json`
// every session reads (a project's `.pi/mcp.json` is the project's own, edited with it).
// Sessions read the file when they start, so the service unloads its idle ones after a
// change. Sign-ins land in pi's `mcp-auth.json`; a session waiting for one picks it up on its
// next turn.

import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { getAgentDir, type ModelRuntime } from "@earendil-works/pi-coding-agent";
// Pi's MCP config and client, which its package doesn't export (hence the paths): the modules
// its sessions and `pi mcp` use, so a server works here as it does there.
import { validateMcpServerConfig } from "../node_modules/@earendil-works/pi-coding-agent/dist/core/mcp-servers.js";
import {
  addMcpServerConfig,
  loadMcpConfig,
  type McpServerEntry,
  removeMcpServerConfig,
} from "../node_modules/@earendil-works/pi-coding-agent/dist/extensions/mcp/config.js";
import { loadMcpRuntime } from "../node_modules/@earendil-works/pi-coding-agent/dist/extensions/mcp/runtime.lazy.js";
import type { Interaction } from "./auth";

/** How long a check waits for a server: a command fetched first (`npx`) can take a while. */
const CHECK_TIMEOUT_MS = 45_000;

/** A server as Settings › MCP servers lists it (`McpServer` on the wire). */
interface McpServerInfo {
  name: string;
  /** Its entry as the file has it. */
  config: unknown;
  /** Why pi skips it. */
  error?: string;
  /** OAuth credentials are stored for it. */
  signed_in: boolean;
}

/** How connecting went (`McpServerStatus` on the wire). */
interface McpStatus {
  state: "connected" | "needs_auth" | "failed";
  tools: { name: string; description?: string }[];
  error?: string;
}

const configPath = () => join(getAgentDir(), "mcp.json");

/** Servers whose names differ only in `-` and `_` share their tools' names: pi refuses the second. */
const namespace = (name: string) => name.replace(/-/g, "_");

const isRecord = (v: unknown): v is Record<string, unknown> =>
  typeof v === "object" && v !== null && !Array.isArray(v);

/** The file's `mcpServers` as written, invalid entries included (none without a file). */
function entries(): [string, unknown][] {
  const path = configPath();
  if (!existsSync(path)) return [];
  const parsed: unknown = JSON.parse(readFileSync(path, "utf8").replace(/^\uFEFF/, ""));
  const servers = isRecord(parsed) ? (parsed.mcpServers ?? {}) : undefined;
  if (!isRecord(servers))
    throw new Error(`${path}: expected an object with an "mcpServers" object`);
  return Object.entries(servers);
}

/** Every server in the file, in its order, with what pi thinks of it. Connects to none. */
export async function list(): Promise<{ servers: McpServerInfo[]; path: string }> {
  const credentials = new (await loadMcpRuntime()).McpOAuthCredentialStore();
  // The names of the servers pi takes, as it reads them: an entry it skips claims none.
  const taken = new Map<string, string>();
  const servers = entries().map(([name, raw]): McpServerInfo => {
    const config = validateMcpServerConfig(name, raw);
    if (typeof config === "string") return { name, config: raw, error: config, signed_in: false };
    const clash = taken.get(namespace(name));
    if (clash) return { name, config: raw, error: `conflicts with "${clash}"`, signed_in: false };
    taken.set(namespace(name), name);
    const signed_in = "url" in config && credentials.tokens(name, config.url) !== undefined;
    return { name, config: raw, signed_in };
  });
  return { servers, path: configPath() };
}

/** Add server `name` with `config`, or replace its entry. Refused as pi would skip it. */
export async function save(name: string, config: unknown): Promise<void> {
  const valid = validateMcpServerConfig(name, config);
  if (typeof valid === "string") throw new Error(valid);
  // A new name only: an entry already in the file keeps its place, whatever clashes with it.
  const all = entries();
  const clash =
    !all.some(([other]) => other === name) &&
    all.find(([other]) => namespace(other) === namespace(name));
  if (clash)
    throw new Error(
      `"${name}" is too close to "${clash[0]}": pick a name that differs in more than - and _`,
    );
  addMcpServerConfig(configPath(), name, valid);
}

export async function remove(name: string): Promise<void> {
  if (!removeMcpServerConfig(configPath(), name)) throw new Error(`No MCP server named "${name}"`);
}

/** The server's entry as sessions load it. */
function entry(name: string): McpServerEntry {
  const loaded = loadMcpConfig({ agentDir: getAgentDir(), cwd: homedir(), projectTrusted: false });
  const found = loaded.servers.find((s) => s.name === name);
  if (!found) throw new Error(`No usable MCP server named "${name}"`);
  return found;
}

/** A connection to server `name` as a session makes it, from the home directory. */
async function connect(name: string, models: Promise<ModelRuntime>) {
  const rt = await loadMcpRuntime();
  const credentials = new rt.McpOAuthCredentialStore();
  const connection = new rt.McpServerConnection({
    entry: entry(name),
    cwd: homedir(),
    createTransport: rt.createDefaultTransport,
    credentials,
    // For servers that send a provider's token (`auth.provider`).
    providerToken: async (provider) =>
      (await (await models).getAuth(provider).catch(() => undefined))?.auth.apiKey,
    log: new rt.McpServerLog(join(getAgentDir(), "mcp.log")),
    onTools: () => {},
  });
  return { rt, credentials, connection };
}

/** Connect to server `name`, say how that went and what it offers, and disconnect. */
export async function check(name: string, models: Promise<ModelRuntime>): Promise<McpStatus> {
  const { connection } = await connect(name, models);
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<"timeout">((resolve) => {
    timer = setTimeout(() => resolve("timeout"), CHECK_TIMEOUT_MS);
  });
  try {
    // A failure is in the connection's state and error.
    const outcome = await Promise.race([connection.getClient().catch(() => undefined), timeout]);
    if (outcome === "timeout")
      return { state: "failed", tools: [], error: `No answer within ${CHECK_TIMEOUT_MS / 1000} s` };
    const { state, error } = connection;
    if (state === "connected") {
      const tools = connection.tools.map((t) => ({
        name: t.name,
        description: t.description?.split("\n", 1)[0]?.trim() || undefined,
      }));
      return { state, tools };
    }
    if (state === "needs-auth") return { state: "needs_auth", tools: [], error };
    return { state: "failed", tools: [], error: error ?? `The connection ended up ${state}` };
  } finally {
    clearTimeout(timer);
    await connection.close();
  }
}

/** pi-mcp's `OAuthRegistrationError` (not ours to import) for a server that forbade registering. */
const isRegistrationRefusal = (e: unknown): e is Error =>
  e instanceof Error && e.name === "OAuthRegistrationError" && "status" in e && e.status === 403;

/** Sign in to OAuth server `name`, as `pi mcp login` does, through a login's `interaction`. */
export async function signIn(
  name: string,
  models: Promise<ModelRuntime>,
  interaction: Interaction,
): Promise<void> {
  const { rt, credentials, connection } = await connect(name, models);
  // A cancel also stops connecting, before and after the sign-in (`close` aborts a connect).
  const stop = () => void connection.close();
  interaction.signal.addEventListener("abort", stop, { once: true });
  try {
    const { config } = connection.entry;
    if ("url" in config && config.auth?.provider)
      throw new Error(
        `${name} sends the credentials of provider "${config.auth.provider}": sign in to that under Providers`,
      );
    const url = connection.oauthUrl;
    if (!url)
      throw new Error(
        `${name} doesn't sign in with OAuth: only an HTTP server without an Authorization header does`,
      );
    // Connecting first says whether a sign-in is needed, and records the server's challenge.
    try {
      await connection.getClient();
      return;
    } catch {
      if (connection.state !== "needs-auth")
        throw new Error(connection.error ?? "It didn't connect");
    }
    const settings = connection.oauthSettings();
    const signingIn = rt.signInMcpServer({
      serverUrl: url,
      store: credentials.forServer(name, url),
      settings,
      challenge: connection.challenge,
      prompt: {
        showAuthorizationUrl: (u) => interaction.notify({ type: "auth_url", url: u.href }),
        // The way in when the browser can't reach this host's callback (a remote host). Withdrawn
        // once the callback came; rejected by a cancel, which ends the sign-in.
        promptForRedirectUrl: (signal) =>
          interaction
            .prompt({
              type: "manual_code",
              message: "If the browser can't reach this host, paste the address it ended up on:",
              placeholder: "http://127.0.0.1:…/callback?code=…",
              signal,
            })
            .catch(() => undefined),
      },
      signal: interaction.signal,
    });
    await signingIn.catch((e: unknown) => {
      // Some servers only register the clients they know, by name (Figma's refuses pi's). What
      // to do comes first: pi's message ends with the server's answer, however long.
      if (isRegistrationRefusal(e) && !settings.clientName)
        throw new Error(
          `The server may only let clients it knows sign in: edit ${name} and give it a client name the server accepts, such as Claude Code (${e.message})`,
        );
      throw e;
    });
    // The sign-in answered the challenge: connecting proves it works.
    connection.challenge = undefined;
    interaction.notify({ type: "progress", message: "Connecting…" });
    await connection.reconnect();
  } finally {
    interaction.signal.removeEventListener("abort", stop);
    await connection.close();
  }
}

/** Delete the OAuth credentials stored for server `name`. */
export async function signOut(name: string): Promise<void> {
  const { config } = entry(name);
  if (!("url" in config)) throw new Error(`${name} doesn't sign in`);
  new (await loadMcpRuntime()).McpOAuthCredentialStore().remove(name, config.url);
}
