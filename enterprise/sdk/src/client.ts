import type { FramePage, HistoryPage, LiveFrame } from "./generated/activity.js";
import { validateLiveFrame } from "./generated/validators.mjs";
import { counter, decodeFramePage, decodeHistoryPage } from "./state.js";

const MAXIMUM_BODY = 1024 * 1024;
export class EnterpriseHttpError extends Error {
  constructor(readonly status: number) { super(`Enterprise request failed (${status})`); }
}
export interface ActivityClientOptions {
  baseUrl: string;
  /** Omit for the same-origin BFF. Never persist bearer tokens in browser storage. */
  accessToken?: () => Promise<string>;
  fetch?: typeof globalThis.fetch;
  /** Optional BFF identity compare-and-set; it restricts a request and grants no authority. */
  browserContext?: () => string | undefined;
}
export interface PageOptions {
  limit?: number;
  signal?: AbortSignal;
}

/** Public activity client. It never accepts a database credential, lease,
 * Worker URL, host path, or a fallback endpoint. */
export class ActivityClient {
  protected readonly base: URL;
  private readonly request: typeof globalThis.fetch;
  constructor(private readonly options: ActivityClientOptions) {
    this.base = new URL(options.baseUrl);
    if (options.accessToken && options.browserContext) throw new Error("Browser context belongs to the BFF");
    if (this.base.protocol !== "https:" || this.base.username || this.base.password || this.base.search || this.base.hash) {
      throw new Error("Enterprise activity requires a credential-free HTTPS API URL");
    }
    if (!this.base.pathname.endsWith("/")) this.base.pathname += "/";
    const path = this.base.pathname;
    if (options.accessToken ? !path.endsWith("/api/v1/") || path.endsWith("/app/api/v1/") : !path.endsWith("/app/api/v1/")) {
      throw new Error("Choose the bearer API or same-origin BFF explicitly");
    }
    if (!options.accessToken && typeof location !== "undefined" && this.base.origin !== location.origin) {
      throw new Error("The browser BFF must share the application's origin");
    }
    this.request = options.fetch ?? globalThis.fetch.bind(globalThis);
  }
  async history(session: string, options: PageOptions & { before?: string; through?: string } = {}): Promise<HistoryPage> {
    const url = this.url(session, "history", options.limit);
    if (options.before !== undefined) url.searchParams.set("before", counter(options.before).toString());
    if (options.through !== undefined) url.searchParams.set("through", counter(options.through).toString());
    const page = decodeHistoryPage(await this.get(url, options.signal));
    if (page.sessionId !== session) throw new Error("Enterprise history session mismatch");
    return page;
  }
  async frames(session: string, after: string, options: PageOptions = {}): Promise<FramePage> {
    const url = this.url(session, "frames", options.limit);
    url.searchParams.set("after", counter(after).toString());
    const page = decodeFramePage(await this.get(url, options.signal));
    if (page.sessionId !== session) throw new Error("Enterprise activity session mismatch");
    let expected = counter(after);
    for (const frame of page.frames) {
      if (counter(frame.sequence) !== ++expected) throw new Error("Enterprise activity has a sequence gap");
    }
    if (counter(page.through) !== expected || (page.more && page.frames.length === 0)) {
      throw new Error("Enterprise activity has an invalid continuation");
    }
    return page;
  }
  async live(session: string, signal?: AbortSignal): Promise<LiveFrame | null> {
    const url = this.url(session, "live"); url.search = "";
    const value = await this.get(url, signal);
    if (value === null) return null;
    if (!validateLiveFrame(value)) throw new Error("Invalid live activity response");
    const frame = value as LiveFrame;
    if (frame.version !== 1 || frame.sessionId !== session || frame.event.kind !== "snapshot") {
      throw new Error("Invalid live activity context");
    }
    counter(frame.sequence); counter(frame.afterCommitted);
    return frame;
  }
  async *watch(session: string, after: string, options: PageOptions & { intervalMs?: number } = {}): AsyncGenerator<FramePage> {
    const interval = options.intervalMs ?? 500;
    if (!Number.isInteger(interval) || interval < 100 || interval > 30000) throw new Error("Invalid activity polling interval");
    let cursor = counter(after).toString();
    while (!options.signal?.aborted) {
      const page = await this.frames(session, cursor, options);
      cursor = page.through;
      if (page.frames.length > 0) yield page;
      if (!page.more) await delay(interval, options.signal);
    }
  }
  private url(session: string, operation: string, limit = 50): URL {
    if (!/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/.test(session) || !Number.isInteger(limit) || limit < 1 || limit > 100) {
      throw new Error("Invalid activity request");
    }
    const url = new URL(`sessions/${encodeURIComponent(session)}/${operation}`, this.base);
    url.searchParams.set("limit", limit.toString());
    return url;
  }
  protected async response(url: URL, signal?: AbortSignal, method = "GET", body?: unknown, accept = "application/json", timeout = 15000): Promise<Response> {
    if (url.origin !== this.base.origin || !url.pathname.startsWith(this.base.pathname) || url.username || url.password || url.hash) {
      throw new Error("Enterprise request escaped its configured API");
    }
    const headers = new Headers({ accept });
    const context = this.options.browserContext?.();
    if (context !== undefined) headers.set("x-zuno-browser-context", context);
    if (this.options.accessToken) headers.set("authorization", `Bearer ${await this.options.accessToken()}`);
    if (body !== undefined) headers.set("content-type", "application/json");
    if (method !== "GET" && !this.options.accessToken) headers.set("x-zuno-csrf", "1");
    const response = await this.request(url, {
      method, body: body === undefined ? undefined : JSON.stringify(body),
      headers, credentials: this.options.accessToken ? "omit" : "same-origin",
      cache: "no-store", redirect: "error", signal: signal ? AbortSignal.any([signal, AbortSignal.timeout(timeout)]) : AbortSignal.timeout(timeout),
    });
    if (!response.ok) {
      await response.body?.cancel();
      throw new EnterpriseHttpError(response.status);
    }
    return response;
  }
  protected async get(url: URL, signal?: AbortSignal, method = "GET", body?: unknown): Promise<unknown> {
    const response = await this.response(url, signal, method, body);
    if (response.status === 204) return null;
    if (!response.headers.get("content-type")?.toLowerCase().startsWith("application/json")) {
      await response.body?.cancel();
      throw new Error("Enterprise activity did not return JSON");
    }
    const reader = response.body?.getReader();
    if (!reader) throw new Error("Enterprise activity response has no body");
    const chunks: Uint8Array[] = [];
    let size = 0;
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        size += value.byteLength;
        if (size > MAXIMUM_BODY) throw new Error("Enterprise activity response exceeds its limit");
        chunks.push(value);
      }
    } catch (error) {
      await reader.cancel().catch(() => undefined);
      throw error;
    } finally {
      reader.releaseLock();
    }
    const bytes = new Uint8Array(size);
    let offset = 0;
    for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
    return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  }
}

function delay(milliseconds: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) { reject(signal.reason); return; }
    const done = () => { signal?.removeEventListener("abort", abort); resolve(); };
    const timer = setTimeout(done, milliseconds);
    const abort = () => { clearTimeout(timer); signal?.removeEventListener("abort", abort); reject(signal?.reason); };
    signal?.addEventListener("abort", abort, { once: true });
  });
}
