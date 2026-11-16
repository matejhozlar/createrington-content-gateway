import type {
  ApiError,
  BudgetSnapshot,
  ErrorCode,
  Health,
  JavaRuntime,
  Loader,
  LoaderVersions,
  MinecraftVersionDetails,
  MinecraftVersions,
  ResolveRequest,
  Resolved,
  VersionFilesRequest,
  paths,
} from "./gateway.js";

/** How long one request to the gateway may take, above the 10 s the gateway gives an upstream. */
export const DEFAULT_TIMEOUT_MS = 15_000;

/** How long `resolve()` may take unless its options say otherwise, above the gateway's 40 s worst case. */
export const RESOLVE_TIMEOUT_MS = 60_000;

const CACHE_HEADER = "x-cache";
const POOL_HEADER = "x-budget-pool";
const CACHE_STATUSES = ["hit", "miss", "stale"] as const;
const UNAUTHENTICATED: keyof paths = "/v1/healthz";

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

/** What every call takes. */
export interface RequestOptions {
  /** Aborts the call, the wait for a refusal to end included. */
  signal?: AbortSignal;
}

/** What a call that draws from the budget takes. */
export interface BudgetedOptions extends RequestOptions {
  /** The pool this call draws from instead of the client's. */
  pool?: string;
  /** Ask the upstream again instead of answering from the cache (`Cache-Control: no-cache`). */
  fresh?: boolean;
  /** How long to wait out the gateway's refusals, in milliseconds, for background callers. */
  waitUpToMs?: number;
}

/** What a passthrough call takes besides its method and body. */
export interface PassthroughOptions extends BudgetedOptions {
  query?: Query;
}

/** A request's method and body: a `GET` carries none, a body makes the request a `POST`. */
export type CurseForgeMethod =
  { method?: "GET"; body?: undefined } | { method?: "POST"; body?: unknown };

export type CurseForgeRequest = CurseForgeMethod & PassthroughOptions;

/** A Modrinth call's options, as `CurseForgeRequest`: the calls draw from the pool's Modrinth budget. */
export type ModrinthRequest = CurseForgeRequest;

type Method = "get" | "post";

type Operation<P extends keyof paths, M extends Method> = Extract<
  paths[P][M],
  { responses: unknown }
>;

type RouteMethod<P extends keyof paths> = {
  [M in Method]: [Operation<P, M>] extends [never] ? never : M;
}[Method];

type RouteQuery<P extends keyof paths, M extends RouteMethod<P>> =
  Operation<P, M> extends { parameters: { query?: infer Q } }
    ? NonNullable<Q>
    : never;

type RouteAnswer<O> = O extends {
  responses: { 200: { content: { "application/json": infer A } } };
}
  ? A
  : never;

type RouteCall<O> = (O extends {
  parameters: { path: infer T extends Record<string, string> };
}
  ? { params: T }
  : { params?: never }) &
  (O extends { parameters: { query: infer Q extends Query } }
    ? { query: Q }
    : {
        query?: O extends { parameters: { query?: infer Q extends Query } }
          ? Q
          : never;
      }) &
  (O extends { requestBody: { content: { "application/json": infer B } } }
    ? { body: B }
    : { body?: never }) &
  BudgetedOptions & { timeoutMs?: number };

interface Call extends BudgetedOptions {
  params?: Record<string, string>;
  query?: Query;
  body?: unknown;
  timeoutMs?: number;
}

/** What `resolve()` asks for. */
export interface ResolveOptions
  extends BudgetedOptions, Pick<ResolveRequest, "cdn"> {
  /** How long the call may take, in milliseconds. `RESOLVE_TIMEOUT_MS` when unset, longer than the client's own timeout. */
  timeoutMs?: number;
}

/** What `minecraftVersions()` asks for. */
export type MinecraftVersionsOptions = RouteQuery<
  "/v1/minecraft/versions",
  "get"
> &
  RequestOptions;

/** What `loaderVersions()` asks for. */
export type LoaderVersionsOptions = RouteQuery<
  "/v1/minecraft/loaders/{loader}/versions",
  "get"
> &
  RequestOptions;

/** What `budget()` asks for. */
export type BudgetOptions = RouteQuery<"/v1/budget", "get"> & RequestOptions;

type JavaQuery = RouteQuery<"/v1/minecraft/java", "get">;

/** What `javaRuntime()` asks for: a Java major or a runtime component, not both. */
export type JavaRuntimeOptions = (
  | {
      /** The newest runtime Mojang ships of this Java major, such as 21. */
      major: NonNullable<JavaQuery["major"]>;
      component?: never;
    }
  | {
      /** The runtime of this name, such as `java-runtime-delta`, as `javaComponent` of `minecraftVersion()` names it. */
      component: NonNullable<JavaQuery["component"]>;
      major?: never;
    }
) &
  RequestOptions;

/** How the gateway answered a passthrough call, from `X-Cache`. */
export type CacheStatus = (typeof CACHE_STATUSES)[number];

/** What an upstream (CurseForge or Modrinth) answered through the gateway. */
export interface PassthroughAnswer {
  /** The upstream's status, as it came. */
  status: number;
  /** Whether `status` is 2xx. */
  ok: boolean;
  cache: CacheStatus;
  /** The body parsed as JSON, the text when it is not JSON, or `null` when there was none. */
  body: unknown;
  headers: Headers;
}

export type CurseForgeAnswer = PassthroughAnswer;

/**
 * An answer that is not the one asked for: the gateway's own error, or, from the gateway's own routes, anything
 * but its JSON (an upstream's failure passed on by `resolve()`, a page from something else at the base URL).
 * `code` is `null` unless it was the gateway's JSON error.
 */
export class ContentGatewayError extends Error {
  readonly status: number;
  readonly code: ErrorCode | null;
  readonly pool: string | null;
  /** When the next call can go out, on a refused call: the body's `retryAfterMs`, else `Retry-After`. */
  readonly retryAfterMs: number | null;

  constructor(
    status: number,
    error: ApiError | null,
    fallback: string,
    retryAfterMs: number | null = null,
  ) {
    super(error?.message ?? fallback);
    this.name = "ContentGatewayError";
    this.status = status;
    this.code = error?.code ?? null;
    this.pool = error?.pool ?? null;
    this.retryAfterMs = error?.retryAfterMs ?? retryAfterMs;
  }
}

interface Sent {
  response: Response;
  body: unknown;
}

interface SendOptions extends Omit<Call, "params"> {
  method: "GET" | "POST";
  path: string;
  authenticated: boolean;
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
  async health(options?: RequestOptions): Promise<Health>;
  /** @deprecated Pass `{ signal }`. */
  async health(signal: AbortSignal): Promise<Health>;
  async health(options: RequestOptions | AbortSignal = {}): Promise<Health> {
    return this.call("get", "/v1/healthz", { signal: signalOf(options) });
  }

  /** `GET /v1/budget`: this minute's calls, the 429 hold and the daily counts of the last `days` UTC days. */
  async budget(options: BudgetOptions = {}): Promise<BudgetSnapshot> {
    const { signal, ...query } = options;
    return this.call("get", "/v1/budget", { query, signal });
  }

  /** Sends `api.curseforge.com/v1/<path>` (such as `mods/search`) through the gateway; CurseForge's answer comes back whatever its status, and only the gateway's own answers throw `ContentGatewayError`. */
  async curseforge(
    path: string,
    request: CurseForgeRequest = {},
  ): Promise<CurseForgeAnswer> {
    return this.passthrough("curseforge", path, request);
  }

  /** Sends `api.modrinth.com/v2/<path>` (such as `project/sodium`) through the gateway; Modrinth's answer comes back whatever its status, and only the gateway's own answers throw `ContentGatewayError`. */
  async modrinth(
    path: string,
    request: ModrinthRequest = {},
  ): Promise<PassthroughAnswer> {
    return this.passthrough("modrinth", path, request);
  }

  /** `GET /v1/modrinth/version_file/<sha1>`: the version Modrinth lists the file under, or a 404. */
  async modrinthVersionFile(
    sha1: string,
    options: PassthroughOptions = {},
  ): Promise<PassthroughAnswer> {
    return this.passthrough(
      "modrinth",
      `version_file/${encodeURIComponent(sha1)}`,
      { ...options, method: "GET" },
    );
  }

  /** `POST /v1/modrinth/version_files`: the versions Modrinth lists these SHA-1s under, keyed by lowercase hash. */
  async modrinthVersionFiles(
    hashes: readonly string[],
    options: PassthroughOptions = {},
  ): Promise<PassthroughAnswer> {
    const body: VersionFilesRequest = {
      hashes: [...hashes],
      algorithm: "sha1",
    };
    return this.passthrough("modrinth", "version_files", {
      ...options,
      method: "POST",
      body,
    });
  }

  /** `POST /v1/resolve`: a download address per CurseForge file id. CurseForge's own failure, which the gateway passes on, throws as a `ContentGatewayError` with its status. */
  async resolve(
    fileIds: readonly number[],
    options: ResolveOptions = {},
  ): Promise<Resolved> {
    const { cdn, timeoutMs, ...rest } = options;
    return this.call("post", "/v1/resolve", {
      ...rest,
      body: { fileIds: [...fileIds], cdn },
      timeoutMs: timeoutMs ?? RESOLVE_TIMEOUT_MS,
    });
  }

  /** `GET /v1/minecraft/versions`: the Minecraft versions Mojang lists, newest first. */
  async minecraftVersions(
    options: MinecraftVersionsOptions = {},
  ): Promise<MinecraftVersions> {
    const { signal, ...query } = options;
    return this.call("get", "/v1/minecraft/versions", { query, signal });
  }

  /** `GET /v1/minecraft/versions/<id>`: one version with the Java major it needs. Throws `UNKNOWN_VERSION` as a `ContentGatewayError`. */
  async minecraftVersion(
    id: string,
    options?: RequestOptions,
  ): Promise<MinecraftVersionDetails>;
  /** @deprecated Pass `{ signal }`. */
  async minecraftVersion(
    id: string,
    signal: AbortSignal,
  ): Promise<MinecraftVersionDetails>;
  async minecraftVersion(
    id: string,
    options: RequestOptions | AbortSignal = {},
  ): Promise<MinecraftVersionDetails> {
    return this.call("get", "/v1/minecraft/versions/{id}", {
      params: { id },
      signal: signalOf(options),
    });
  }

  /** `GET /v1/minecraft/loaders/<loader>/versions`: a loader's builds, newest first. */
  async loaderVersions(
    loader: Loader,
    options: LoaderVersionsOptions = {},
  ): Promise<LoaderVersions> {
    const { signal, ...query } = options;
    return this.call("get", "/v1/minecraft/loaders/{loader}/versions", {
      params: { loader },
      query,
      signal,
    });
  }

  /** `GET /v1/minecraft/java`: a Java runtime Mojang ships for `platform` (Mojang's name, such as `linux` or `windows-x64`), with the address of its manifest of files. Throws `UNKNOWN_RUNTIME` as a `ContentGatewayError`. */
  async javaRuntime(
    platform: string,
    options: JavaRuntimeOptions,
  ): Promise<JavaRuntime> {
    const { signal, ...runtime } = options;
    return this.call("get", "/v1/minecraft/java", {
      query: { platform, ...runtime },
      signal,
    });
  }

  private async call<P extends keyof paths, M extends RouteMethod<P>>(
    method: M,
    path: P,
    request: RouteCall<Operation<P, M>>,
  ): Promise<RouteAnswer<Operation<P, M>>>;
  private async call(
    method: Method,
    route: keyof paths,
    request: Call,
  ): Promise<unknown> {
    const { params, ...rest } = request;
    const options: SendOptions = {
      ...rest,
      method: method === "get" ? "GET" : "POST",
      path: fillPath(route, params),
      authenticated: route !== UNAUTHENTICATED,
    };
    const { response, body } = await this.send(options);
    const cache = response.headers.get(CACHE_HEADER);
    if (response.ok && cache === null && isObject(body)) return body;
    const answered = `${response.status} answer to ${options.method} ${options.path}`;
    throw new ContentGatewayError(
      response.status,
      null,
      isCacheStatus(cache)
        ? `the ${answered} is an upstream's, passed on by the gateway (X-Cache: ${cache})`
        : `the ${answered} is not the gateway's JSON`,
    );
  }

  private async passthrough(
    upstream: "curseforge" | "modrinth",
    path: string,
    request: CurseForgeRequest,
  ): Promise<PassthroughAnswer> {
    const { method: asked, ...rest } = request;
    const method = asked ?? (request.body === undefined ? "GET" : "POST");
    if (method === "GET" && request.body !== undefined) {
      throw new TypeError(
        `a GET to ${path} cannot carry a body; leave method unset or make it POST`,
      );
    }
    const options: SendOptions = {
      ...rest,
      method,
      path: `/v1/${upstream}/${path.replace(/^\/+/, "")}`,
      authenticated: true,
    };
    const { response, body } = await this.send(options);
    const cache = response.headers.get(CACHE_HEADER);
    if (!isCacheStatus(cache)) {
      throw new ContentGatewayError(
        response.status,
        null,
        `the answer to ${options.method} ${options.path} did not come through the gateway: ` +
          (cache === null
            ? "no X-Cache header"
            : `X-Cache ${JSON.stringify(cache)} is not one the gateway sends`),
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
    const caller = followSignal(options.signal);

    try {
      for (;;) {
        const response = await this.fetch(url, {
          method: options.method,
          headers,
          body,
          signal: attemptSignal(
            options.timeoutMs ?? this.timeoutMs,
            caller.signal,
          ),
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
          await sleep(wait, caller.signal);
          continue;
        }
        throw new ContentGatewayError(
          response.status,
          error,
          `the gateway answered ${response.status} to ${options.method} ${options.path}` +
            (typeof answer === "string" && answer !== "" ? `: ${answer}` : ""),
          wait,
        );
      }
    } finally {
      caller.stop();
    }
  }
}

function signalOf(
  options: RequestOptions | AbortSignal,
): AbortSignal | undefined {
  return "aborted" in options ? options : options.signal;
}

function followSignal(signal: AbortSignal | undefined): {
  signal: AbortSignal | undefined;
  stop: () => void;
} {
  if (signal === undefined || signal instanceof AbortSignal) {
    return { signal, stop: () => {} };
  }
  const foreign: EventTarget & Pick<AbortSignal, "aborted" | "reason"> = signal;
  const controller = new AbortController();
  const abort = (): void => controller.abort(foreign.reason);
  if (foreign.aborted) abort();
  else foreign.addEventListener("abort", abort, { once: true });
  return {
    signal: controller.signal,
    stop: () => foreign.removeEventListener("abort", abort),
  };
}

function fillPath(
  route: string,
  params: Record<string, string> | undefined,
): string {
  return route.replace(/\{(\w+)\}/g, (_, name: string) => {
    const value = params?.[name];
    if (value === undefined) {
      throw new TypeError(`no value for {${name}} in ${route}`);
    }
    return encodeURIComponent(value);
  });
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
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
  if (!isObject(body)) return null;
  const { code, message } = body;
  if (typeof code !== "string" || typeof message !== "string") return null;
  return {
    code,
    message,
    pool: typeof body.pool === "string" ? body.pool : null,
    retryAfterMs:
      typeof body.retryAfterMs === "number" ? body.retryAfterMs : null,
  };
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
