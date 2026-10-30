// Model provider sign-in for the desktop's Settings › Providers: pi's `/login` and `/logout`
// over the protocol. Pi runs a login as a flow of prompts (`AuthPrompt`: text, a secret, a
// choice, a pasted code) and notices (`AuthEvent`: a url to open, a device code, info,
// progress) that it leaves to the app to show. Each goes to the service as a `login_step`;
// answers come back by prompt number. Credentials land in pi's `auth.json`, which the
// terminal pi shares, and the shared `ModelRuntime` sees them at once: live sessions included.

import { homedir } from "node:os";
import { join } from "node:path";
import type { AuthEvent, AuthPrompt } from "@earendil-works/pi-ai";
import { getAgentDir, type ModelRuntime, SettingsManager } from "@earendil-works/pi-coding-agent";

export type Method = "oauth" | "api_key";

/** A provider as Settings › Providers lists it (`ProviderInfo` on the wire). */
export interface ProviderInfo {
  id: string;
  name: string;
  /** Signing in with an account is offered: what that is ("Anthropic (Claude Pro/Max)"). */
  oauth?: string;
  /** A key (or the provider's other setup: a profile, an account id) can be entered: what it is. */
  api_key?: string;
  /** Where its credentials come from now ("Signed in", "From ANTHROPIC_API_KEY"); absent: none. */
  status?: string;
  /** What `auth.json` holds for it: what `logout` removes. */
  stored?: Method;
}

/** One step of a login (`LoginStep` on the wire). */
export type Step =
  | { type: "auth_url"; url: string; instructions?: string }
  | { type: "device_code"; user_code: string; verification_uri: string }
  | { type: "info"; message: string; links: { url: string; label?: string }[] }
  | { type: "progress"; message: string }
  | {
      type: "prompt";
      n: number;
      kind: AuthPrompt["type"];
      message: string;
      placeholder?: string;
      options: { id: string; label: string; description?: string }[];
    }
  | { type: "prompt_closed"; n: number };

/** A login in progress: what cancels it, and its open prompts by number. */
interface Live {
  abort: AbortController;
  prompts: Map<number, { resolve: (value: string) => void; reject: (e: Error) => void }>;
  next: number;
}

const cancelled = () => new Error("Login cancelled");

export class Logins {
  private live = new Map<number, Live>();

  constructor(
    private models: Promise<ModelRuntime>,
    private send: (id: number, step: Step) => void,
  ) {}

  /** Every provider pi knows, by name, with how it is set up here. */
  async providers(): Promise<{ providers: ProviderInfo[]; auth_path: string }> {
    const rt = await this.models;
    const stored = new Map(
      (await rt.listCredentials({ signal: AbortSignal.timeout(15_000) })).map((c) => [
        c.providerId,
        c.type,
      ]),
    );
    const providers = rt
      .getProviders()
      .map((p): ProviderInfo => {
        const oauth = p.auth.oauth;
        return {
          id: p.id,
          name: p.name,
          oauth: oauth && (oauth.loginLabel ?? oauth.name),
          api_key: p.auth.apiKey?.login ? p.auth.apiKey.name : undefined,
          status: status(rt, p.id, stored.get(p.id)),
          stored: stored.get(p.id),
        };
      })
      .sort((a, b) => a.name.localeCompare(b.name));
    return { providers, auth_path: join(getAgentDir(), "auth.json") };
  }

  /**
   * Run login `id` to its end: the credential saved and the provider's model catalog
   * fetched again (some depend on the account, OpenRouter's). Throws when it fails, and
   * "Login cancelled" when cancelled.
   */
  async login(id: number, provider: string, method: Method): Promise<void> {
    const rt = await this.models;
    const live: Live = { abort: new AbortController(), prompts: new Map(), next: 0 };
    this.live.set(id, live);
    try {
      await rt.login(
        provider,
        method,
        {
          signal: live.abort.signal,
          prompt: (p) => this.prompt(id, live, p),
          notify: (e) => this.send(id, step(e)),
        },
        { getDeviceId },
      );
      // Saved by now: a cancel only cuts this short, the login still succeeds.
      this.send(id, { type: "progress", message: "Fetching the model list…" });
      const signal = AbortSignal.any([live.abort.signal, AbortSignal.timeout(15_000)]);
      await rt.refresh({ providers: [provider], signal }).catch(() => {});
    } catch (e) {
      throw live.abort.signal.aborted ? cancelled() : e;
    } finally {
      this.live.delete(id);
    }
  }

  /** Show prompt `p` and wait for its answer. Pi withdraws a pasted-code prompt when the
   *  browser sign-in finishes first (`p.signal`): the desktop is told to drop it. */
  private prompt(id: number, live: Live, p: AuthPrompt): Promise<string> {
    const n = ++live.next;
    return new Promise((resolve, reject) => {
      const done = () => {
        live.prompts.delete(n);
        p.signal?.removeEventListener("abort", withdrawn);
      };
      const withdrawn = () => {
        done();
        this.send(id, { type: "prompt_closed", n });
        reject(cancelled());
      };
      if (p.signal?.aborted) return reject(cancelled());
      p.signal?.addEventListener("abort", withdrawn, { once: true });
      live.prompts.set(n, {
        resolve: (value) => {
          done();
          resolve(value);
        },
        reject: (e) => {
          done();
          reject(e);
        },
      });
      this.send(id, {
        type: "prompt",
        n,
        kind: p.type,
        message: p.message,
        placeholder: "placeholder" in p ? p.placeholder : undefined,
        options: p.type === "select" ? [...p.options] : [],
      });
    });
  }

  /** The user's answer to prompt `n` of login `id`. */
  answer(id: number, n: number, value: string): void {
    this.live.get(id)?.prompts.get(n)?.resolve(value);
  }

  cancel(id: number): void {
    const live = this.live.get(id);
    if (!live) return;
    live.abort.abort();
    for (const p of live.prompts.values()) p.reject(cancelled());
  }

  /** Remove the provider's stored credential. Environment variables and models.json stay. */
  async logout(provider: string): Promise<void> {
    await (await this.models).logout(provider, { signal: AbortSignal.timeout(15_000) });
  }
}

/** Where the provider's credentials come from, for the list; undefined when it has none.
 *  `stored`: what `auth.json` holds for it. */
function status(rt: ModelRuntime, id: string, stored?: Method): string | undefined {
  const s = rt.getProviderAuthStatus(id);
  if (!s.configured) return undefined;
  switch (s.source) {
    case "stored":
      return stored === "oauth" ? "Signed in" : "API key saved";
    case "environment":
      return s.label ? `From ${s.label}` : "From the environment";
    case "models_json_key":
    case "models_json_command":
      return "From models.json";
    default:
      return "Set up";
  }
}

function step(e: AuthEvent): Step {
  switch (e.type) {
    case "auth_url":
      return { type: "auth_url", url: e.url, instructions: e.instructions };
    case "device_code":
      return { type: "device_code", user_code: e.userCode, verification_uri: e.verificationUri };
    case "info":
      return { type: "info", message: e.message, links: [...(e.links ?? [])] };
    case "progress":
      return { type: "progress", message: e.message };
  }
}

/** This installation's stable id, which some sign-ins send (OpenAI's): pi's own, from its
 *  settings, so dirigent and the terminal pi are one installation. */
function getDeviceId(): string {
  return SettingsManager.create(homedir(), getAgentDir()).getOrCreateDeviceId();
}
