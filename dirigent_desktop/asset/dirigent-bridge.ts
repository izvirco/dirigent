// Loaded explicitly by Dirigent. It exposes session operations that Pi's RPC
// protocol does not yet expose directly, plus the bundled delegation tool.
// It is not installed into the user's Pi configuration.
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import registerAgents from "./dirigent-agents.ts";

const STATUS_KEY = "__dirigent_bridge__";

type BridgeContext = {
  modelRegistry: {
    getProvider(id: string): { baseUrl?: string } | undefined;
    getProviderAuth(id: string): Promise<
      | { auth: { apiKey?: string; baseUrl?: string } }
      | undefined
    >;
  };
  ui: { setStatus(key: string, text: string | undefined): void };
};

type CommandContext = BridgeContext & {
  waitForIdle(): Promise<void>;
  navigateTree(entryId: string, options?: { summarize?: boolean }): Promise<{ cancelled: boolean }>;
  fork(
    entryId: string,
    options?: {
      position?: "before" | "at";
      withSession?: (ctx: CommandContext & { sessionManager: { getSessionFile(): string | undefined } }) => Promise<void>;
    },
  ): Promise<{ cancelled: boolean }>;
  sessionManager: { getSessionFile(): string | undefined };
};

type ExtensionApi = {
  on(
    event: "session_start" | "session_shutdown" | "agent_settled",
    handler: (event: unknown, ctx: BridgeContext) => void | Promise<void>,
  ): void;
  registerCommand(
    name: string,
    options: {
      description: string;
      handler(args: string, ctx: CommandContext): Promise<void>;
    },
  ): void;
};

type CodexUsageWindow = {
  usedPercent: number;
  durationSeconds: number;
  resetsAt?: number;
};

const CODEX_PROVIDER = "openai-codex";
const CODEX_AUTH_CLAIM = "https://api.openai.com/auth";
const CODEX_USAGE_REFRESH_INTERVAL_MS = 60_000;

function report(ctx: BridgeContext, value: Record<string, unknown>) {
  ctx.ui.setStatus(STATUS_KEY, JSON.stringify(value));
}

function accountIdFromToken(token: string): string | undefined {
  try {
    const encoded = token.split(".")[1];
    if (!encoded) return undefined;
    const base64 = encoded.replace(/-/g, "+").replace(/_/g, "/").padEnd(Math.ceil(encoded.length / 4) * 4, "=");
    const payload = JSON.parse(atob(base64)) as Record<string, unknown>;
    const auth = payload[CODEX_AUTH_CLAIM] as Record<string, unknown> | undefined;
    return typeof auth?.chatgpt_account_id === "string" ? auth.chatgpt_account_id : undefined;
  } catch {
    return undefined;
  }
}

function usageUrl(baseUrl: string): string {
  const base = baseUrl.replace(/\/+$/, "");
  return base.includes("/backend-api") ? `${base}/wham/usage` : `${base}/api/codex/usage`;
}

function parseUsageWindow(value: unknown): CodexUsageWindow | undefined {
  if (!value || typeof value !== "object") return undefined;
  const window = value as Record<string, unknown>;
  const usedPercent = window.used_percent;
  const durationSeconds = window.limit_window_seconds;
  if (
    typeof usedPercent !== "number" ||
    !Number.isFinite(usedPercent) ||
    typeof durationSeconds !== "number" ||
    !Number.isFinite(durationSeconds) ||
    durationSeconds < 0
  ) {
    return undefined;
  }
  const resetsAt = window.reset_at;
  return {
    usedPercent,
    durationSeconds,
    ...(typeof resetsAt === "number" && Number.isFinite(resetsAt) ? { resetsAt } : {}),
  };
}

// This mirrors the official Codex client's usage request. Only normalized
// window data crosses the bridge; the endpoint's account fields stay in Pi.
async function fetchCodexUsage(ctx: BridgeContext): Promise<void> {
  try {
    const resolved = await ctx.modelRegistry.getProviderAuth(CODEX_PROVIDER);
    const token = resolved?.auth.apiKey;
    if (!token) {
      report(ctx, { operation: "codex_usage", success: false, error: "OpenAI Codex is not authenticated." });
      return;
    }
    const accountId = accountIdFromToken(token);
    if (!accountId) {
      report(ctx, { operation: "codex_usage", success: false, error: "The Codex account ID is unavailable." });
      return;
    }

    const provider = ctx.modelRegistry.getProvider(CODEX_PROVIDER);
    const baseUrl = resolved.auth.baseUrl ?? provider?.baseUrl ?? "https://chatgpt.com/backend-api";
    const response = await fetch(usageUrl(baseUrl), {
      headers: {
        Authorization: `Bearer ${token}`,
        "ChatGPT-Account-Id": accountId,
      },
      signal: AbortSignal.timeout(10_000),
    });
    if (!response.ok) {
      throw new Error(`Codex usage request failed (${response.status})`);
    }

    const payload = (await response.json()) as Record<string, unknown>;
    const rateLimit = payload.rate_limit as Record<string, unknown> | null | undefined;
    const windows = [rateLimit?.primary_window, rateLimit?.secondary_window]
      .map(parseUsageWindow)
      .filter((window): window is CodexUsageWindow => window !== undefined);
    report(ctx, {
      operation: "codex_usage",
      success: true,
      fetchedAt: Math.floor(Date.now() / 1000),
      windows,
    });
  } catch (error) {
    report(ctx, {
      operation: "codex_usage",
      success: false,
      error: error instanceof Error ? error.message : "Could not fetch Codex usage.",
    });
  }
}

export default function (pi: ExtensionApi & ExtensionAPI) {
  registerAgents(pi);
  let usageRequest: Promise<void> | undefined;
  let usageRefreshTimer: ReturnType<typeof setInterval> | undefined;
  const refreshUsage = (ctx: BridgeContext): Promise<void> => {
    if (usageRequest) return usageRequest;
    usageRequest = fetchCodexUsage(ctx).finally(() => {
      usageRequest = undefined;
    });
    return usageRequest;
  };

  pi.on("session_start", async (_event, ctx) => {
    await refreshUsage(ctx);
    usageRefreshTimer = setInterval(() => void refreshUsage(ctx), CODEX_USAGE_REFRESH_INTERVAL_MS);
  });
  pi.on("session_shutdown", () => {
    if (usageRefreshTimer) {
      clearInterval(usageRefreshTimer);
      usageRefreshTimer = undefined;
    }
  });
  pi.on("agent_settled", (_event, ctx) => refreshUsage(ctx));

  pi.registerCommand("dirigent-navigate", {
    description: "Internal Dirigent session navigation bridge",
    handler: async (args, ctx) => {
      const entryId = args.trim();
      try {
        await ctx.waitForIdle();
        const result = await ctx.navigateTree(entryId, { summarize: false });
        report(ctx, {
          operation: "navigate",
          entryId,
          success: !result.cancelled,
          cancelled: result.cancelled,
        });
      } catch (error) {
        report(ctx, {
          operation: "navigate",
          entryId,
          success: false,
          error: error instanceof Error ? error.message : String(error),
        });
      }
    },
  });

  pi.registerCommand("dirigent-fork", {
    description: "Internal Dirigent session fork bridge",
    handler: async (args, ctx) => {
      const [positionValue, entryId = ""] = args.trim().split(/\s+/, 2);
      const position = positionValue === "at" ? "at" : "before";
      try {
        await ctx.waitForIdle();
        let reported = false;
        const result = await ctx.fork(entryId, {
          position,
          withSession: async (next) => {
            reported = true;
            report(next, {
              operation: "fork",
              entryId,
              position,
              success: true,
              cancelled: false,
              sessionFile: next.sessionManager.getSessionFile(),
            });
          },
        });
        if (result.cancelled && !reported) {
          report(ctx, {
            operation: "fork",
            entryId,
            position,
            success: false,
            cancelled: true,
          });
        }
      } catch (error) {
        report(ctx, {
          operation: "fork",
          entryId,
          position,
          success: false,
          error: error instanceof Error ? error.message : String(error),
        });
      }
    },
  });
}
