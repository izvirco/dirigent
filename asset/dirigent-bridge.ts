// Loaded explicitly by Dirigent. It exposes session operations that Pi's RPC
// protocol does not yet expose directly. It is not installed into the user's
// Pi configuration and does not register any LLM tools.
const STATUS_KEY = "__dirigent_bridge__";

type CommandContext = {
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
  ui: { setStatus(key: string, text: string | undefined): void };
};

type ExtensionApi = {
  registerCommand(
    name: string,
    options: {
      description: string;
      handler(args: string, ctx: CommandContext): Promise<void>;
    },
  ): void;
};

function report(ctx: CommandContext, value: Record<string, unknown>) {
  ctx.ui.setStatus(STATUS_KEY, JSON.stringify(value));
}

export default function (pi: ExtensionApi) {
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
