// One bob, as seen from this page. Every HTTP request, the status stream and every panel
// WebSocket go through a BobConnection, never a bare relative URL, so a page can hold
// several (a future desktop app with a tab per bob) and a page served from somewhere else
// (the Vite dev server, that app) can reach any bob. Bobs answer with permissive CORS.

export class ApiError extends Error {
  constructor(message: string, readonly status: number) {
    super(message);
  }
}

export interface WsHandlers {
  onmessage?(m: any): void;
  onopen?(): void;
  onclose?(): void;
}

export interface Ws {
  send(m: unknown): void;
  close(): void;
  readonly ready: boolean;
}

export class BobConnection {
  /** Origin plus optional path prefix, no trailing slash: "http://deskypi.local". */
  readonly baseUrl: string;

  constructor(baseUrl?: string) {
    const base = baseUrl || (typeof location !== "undefined" ? location.origin : "http://localhost");
    this.baseUrl = base.replace(/\/+$/, "");
  }

  /** Same origin as the page: the bob serves this UI itself. */
  get sameOrigin(): boolean {
    return typeof location !== "undefined" && new URL(this.baseUrl).origin === location.origin;
  }

  /** The host name a person on this side would use for the bob (for copy-paste commands). */
  get hostname(): string {
    return new URL(this.baseUrl).hostname;
  }

  /** host[:port] of the bob. */
  get host(): string {
    return new URL(this.baseUrl).host;
  }

  url(path: string): string {
    return this.baseUrl + (path.startsWith("/") ? path : "/" + path);
  }

  /** ws:// or wss:// URL for a path, derived from baseUrl. */
  wsUrl(path: string): string {
    return this.url(path).replace(/^http/, "ws");
  }

  fetch(path: string, init?: RequestInit): Promise<Response> {
    return fetch(this.url(path), init);
  }

  /** JSON request. Resolves the parsed body; throws ApiError on HTTP errors and `ok: false`. */
  async json<T = any>(method: string, path: string, body?: unknown): Promise<T> {
    const r = await this.fetch(path, {
      method,
      headers: body !== undefined ? { "Content-Type": "application/json" } : {},
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
    const j = await r.json().catch(() => ({}));
    if (!r.ok || j?.ok === false) throw new ApiError(j?.message || r.statusText || `HTTP ${r.status}`, r.status);
    return j as T;
  }

  /** Raw-body upload (a File or Blob) or a FormData POST; same error handling as json(). */
  async upload<T = any>(path: string, body: BodyInit): Promise<T> {
    const r = await this.fetch(path, { method: "POST", body });
    const j = await r.json().catch(() => ({}));
    if (!r.ok || j?.ok === false) throw new ApiError(j?.message || r.statusText || `HTTP ${r.status}`, r.status);
    return j as T;
  }

  events(path = "/api/events"): EventSource {
    return new EventSource(this.url(path));
  }

  /**
   * Reconnecting WebSocket. Text messages arrive parsed as JSON when they are JSON; binary
   * arrives as an ArrayBuffer. Objects are sent as JSON, strings and buffers as they are.
   */
  ws(path: string, h: WsHandlers = {}): Ws {
    let sock: WebSocket;
    let closed = false;
    let retry = 500;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const open = () => {
      sock = new WebSocket(this.wsUrl(path));
      sock.binaryType = "arraybuffer";
      sock.onopen = () => {
        retry = 500;
        h.onopen?.();
      };
      sock.onmessage = (e) => {
        let m: any = e.data;
        if (typeof m === "string") {
          try {
            m = JSON.parse(m);
          } catch {}
        }
        h.onmessage?.(m);
      };
      sock.onclose = () => {
        h.onclose?.();
        if (!closed) timer = setTimeout(open, (retry = Math.min(retry * 2, 8000)));
      };
    };
    open();
    return {
      send(m) {
        if (sock.readyState !== 1) return;
        sock.send(typeof m === "string" || m instanceof ArrayBuffer || ArrayBuffer.isView(m) ? (m as any) : JSON.stringify(m));
      },
      close() {
        closed = true;
        clearTimeout(timer);
        sock.close();
      },
      get ready() {
        return sock.readyState === 1;
      },
    };
  }
}

/**
 * The bob this page talks to: `?bob=http://host` in the page URL, else VITE_BOB at build
 * or dev time, else the page's own origin (a bob serving its own UI).
 */
export function defaultBobUrl(search = typeof location !== "undefined" ? location.search : "", env = import.meta.env?.VITE_BOB as string | undefined): string | undefined {
  const q = new URLSearchParams(search).get("bob");
  const raw = q || env || "";
  if (!raw) return undefined;
  return /^https?:\/\//.test(raw) ? raw : "http://" + raw;
}
