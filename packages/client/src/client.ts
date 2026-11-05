import type { ApiError, BudgetSnapshot, ErrorCode, Health } from "./gateway.js";

/**
 * How long one request to the gateway may take. Above the 10 s the gateway
 * itself gives CurseForge, so a stalled call comes back as the gateway's 504
 * or a stale answer instead of this client's timeout.
 */
export const DEFAULT_TIMEOUT_MS = 15_000;

const CACHE_HEADER = "x-cache";
const POOL_HEADER = "x-budget-pool";
const CACHE_STATUSES = ["hit", "miss", "stale"] as const;

export interface ContentGatewayOptions {
  /** Where the gateway listens, such as `http://127.0.0.1:5005`. */
  baseUrl: string;
  /** This consumer's bearer token. */
  token: string;
  /** The pool calls draw from, sent as `X-Budget-Pool`. Needed when the token has more than one pool. */
  pool?: string;
  /** How long one request may take, in milliseconds. `DEFAULT_TIMEOUT_MS` when unset. */
  timeoutMs?: number;
  /** The `fetch` to send with. Node's own by default, which keeps connections to the gateway open. */
  fetch?: typeof globalThis.fetch;
}

/** Query parameters as an object; `null` and `undefined` values are left out. */
export type Query =
  | URLSearchParams
  | Record<string, string | number | boolean | null | undefined>;

/** A request's method and body: a `GET` carries none, a body makes the request a `POST`. */
export type CurseForgeMethod =
  { method?: "GET"; body?: undefined } | { method?: "POST"; body?: unknown };

export type CurseForgeRequest = CurseForgeMethod & {
  query?: Query;
  /** The pool this call draws from instead of the client's. */
  pool?: string;
  /** Ask CurseForge again instead of answering from the cache (`Cache-Control: no-cache`). */
  fresh?: boolean;
  /**
   * How long to wait out the gateway's refusals, in milliseconds. A `503`
   * whose `Retry-After` fits in what is left is slept out and the call is
   * sent again; one that does not fit is thrown at once. For background
   * callers; a request answering a user should leave it unset.
   */
  waitUpToMs?: number;
  signal?: AbortSignal;
};

/** How the gateway answered a passthrough call, from `X-Cache`. */
export type CacheStatus = (typeof CACHE_STATUSES)[number];

export interface CurseForgeAnswer {
  /** CurseForge's status, as it came. */
  status: number;
  /** Whether `status` is 2xx. */
  ok: boolean;
  cache: CacheStatus;
  /** The body parsed as JSON, the text when it is not JSON, or `null` when there was none. */
  body: unknown;
  headers: Headers;
}

/**
 * An answer the gateway gave itself instead of passing one on: a refused call,
 * a missing token, an unknown pool, CurseForge not answering. `code` is `null`
 * when the answer was not the gateway's JSON error at all, such as a proxy's
 * HTML page, and `message` then carries what came.
 */
export class ContentGatewayError extends Error {
  readonly status: number;
  readonly code: ErrorCode | null;
  readonly pool: string | null;
  /** When the next call can go out, on a refused call. */
  readonly retryAfterMs: number | null;

  constructor(status: number, error: ApiError | null, fallback: string) {
    super(error?.message ?? fallback);
    this.name = "ContentGatewayError";
    this.status = status;
    this.code = error?.code ?? null;
    this.pool = error?.pool ?? null;
    this.retryAfterMs = error?.retryAfterMs ?? null;
  }
}

interface Sent {
  response: Response;
  body: unknown;
}

interface SendOptions {
  method: "GET" | "POST";
  path: string;
  query?: Query | undefined;
  body?: unknown;
  authenticated: boolean;
  pool?: string | undefined;
  fresh?: boolean | undefined;
  waitUpToMs?: number | undefined;
  signal?: AbortSignal | undefined;
}

/** What `budget()` asks for. */
export interface BudgetOptions {
  /** UTC days of daily counts to answer, today included, 1 up; 14 when left out, more than the gateway's `historyDays` answers the days kept. */
  days?: number | undefined;
  signal?: AbortSignal | undefined;
}

/** A consumer's connection to one gateway. */
export class ContentGateway {
  private readonly baseUrl: string;
  private readonly token: string;
  private readonly pool: string | undefined;
  private readonly timeoutMs: number;
  private readonly fetch: typeof globalThis.fetch;

  constructor(options: ContentGatewayOptions) {
    this.baseUrl = options.baseUrl.replace(/\/+$/, "");
    this.token = options.token;
    this.pool = options.pool;
    this.timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    this.fetch = options.fetch ?? globalThis.fetch;
  }

  /** `GET /v1/healthz`, which needs no token. */
  async health(signal?: AbortSignal): Promise<Health> {
    const { body } = await this.send({
      method: "GET",
      path: "/v1/healthz",
      authenticated: false,
      signal,
    });
    return body as Health;
  }

  /** `GET /v1/budget`: this minute's calls, the 429 hold and the daily counts of the last `days` UTC days. */
  async budget(options: BudgetOptions = {}): Promise<BudgetSnapshot> {
    const { body } = await this.send({
      method: "GET",
      path: "/v1/budget",
      query: options.days === undefined ? undefined : { days: options.days },
      authenticated: true,
      signal: options.signal,
    });
    return body as BudgetSnapshot;
  }

  /**
   * Sends `api.curseforge.com/v1/<path>` through the gateway and answers what
   * CurseForge answered, whatever its status. Only the gateway's own answers
   * throw, as `ContentGatewayError`.
   *
   * @param path - The path under `/v1/`, such as `mods/search` or `mods/238222/files/4567`
   */
  async curseforge(
    path: string,
    request: CurseForgeRequest = {},
  ): Promise<CurseForgeAnswer> {
    const method =
      request.method ?? (request.body === undefined ? "GET" : "POST");
    if (method === "GET" && request.body !== undefined) {
      throw new TypeError(
        `a GET to ${path} cannot carry a body; leave method unset or make it POST`,
      );
    }
    const { response, body } = await this.send({
      method,
      path: `/v1/curseforge/${path.replace(/^\/+/, "")}`,
      query: request.query,
      body: request.body,
      authenticated: true,
      pool: request.pool,
      fresh: request.fresh,
      waitUpToMs: request.waitUpToMs,
      signal: request.signal,
    });
    const cache = response.headers.get(CACHE_HEADER);
    if (!isCacheStatus(cache)) {
      throw new ContentGatewayError(
        response.status,
        null,
        `the answer to ${method} ${path} did not come through the gateway: no X-Cache header`,
      );
    }
    return {
      status: response.status,
      ok: response.ok,
      cache,
      body,
      headers: response.headers,
    };
  }

  private async send(options: SendOptions): Promise<Sent> {
    const url = this.baseUrl + options.path + queryString(options.query);
    const headers = new Headers({ accept: "application/json" });
    if (options.authenticated) {
      headers.set("authorization", `Bearer ${this.token}`);
      const pool = options.pool ?? this.pool;
      if (pool !== undefined) headers.set(POOL_HEADER, pool);
    }
    if (options.fresh) headers.set("cache-control", "no-cache");
    let body: string | undefined;
    if (options.body !== undefined) {
      headers.set("content-type", "application/json");
      body = JSON.stringify(options.body);
    }
    const deadline = Date.now() + (options.waitUpToMs ?? 0);

    for (;;) {
      const response = await this.fetch(url, {
        method: options.method,
        headers,
        body,
        signal: attemptSignal(this.timeoutMs, options.signal),
      });
      const answer = await readBody(response);
      if (response.headers.has(CACHE_HEADER) || response.ok) {
        return { response, body: answer };
      }

      const error = asApiError(answer);
      const wait = retryAfterMs(response, error);
      if (
        response.status === 503 &&
        wait !== null &&
        Date.now() + wait <= deadline
      ) {
        await sleep(wait, options.signal);
        continue;
      }
      throw new ContentGatewayError(
        response.status,
        error,
        `the gateway answered ${response.status} to ${options.method} ${options.path}` +
          (typeof answer === "string" && answer !== "" ? `: ${answer}` : ""),
      );
    }
  }
}

function isCacheStatus(value: string | null): value is CacheStatus {
  return (CACHE_STATUSES as readonly string[]).includes(value ?? "");
}

function queryString(query: Query | undefined): string {
  if (query === undefined) return "";
  let parameters: URLSearchParams;
  if (query instanceof URLSearchParams) {
    parameters = query;
  } else {
    parameters = new URLSearchParams();
    for (const [name, value] of Object.entries(query)) {
      if (value !== undefined && value !== null) {
        parameters.set(name, String(value));
      }
    }
  }
  const encoded = parameters.toString();
  return encoded === "" ? "" : `?${encoded}`;
}

function attemptSignal(
  timeoutMs: number,
  signal: AbortSignal | undefined,
): AbortSignal {
  const timeout = AbortSignal.timeout(timeoutMs);
  return signal === undefined ? timeout : AbortSignal.any([timeout, signal]);
}

async function readBody(response: Response): Promise<unknown> {
  const text = await response.text();
  if (text === "") return null;
  const type = response.headers.get("content-type") ?? "";
  if (!/\bjson\b/i.test(type)) return text;
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return text;
  }
}

function asApiError(body: unknown): ApiError | null {
  if (typeof body !== "object" || body === null) return null;
  const candidate = body as Partial<ApiError>;
  if (typeof candidate.code !== "string") return null;
  if (typeof candidate.message !== "string") return null;
  return candidate as ApiError;
}

function retryAfterMs(
  response: Response,
  error: ApiError | null,
): number | null {
  if (typeof error?.retryAfterMs === "number") return error.retryAfterMs;
  const header = response.headers.get("retry-after");
  if (header !== null && /^\d+$/.test(header.trim())) {
    return Number(header.trim()) * 1000;
  }
  return null;
}

function sleep(ms: number, signal: AbortSignal | undefined): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(signal.reason);
      return;
    }
    const onAbort = (): void => {
      clearTimeout(timer);
      reject(signal?.reason);
    };
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}
