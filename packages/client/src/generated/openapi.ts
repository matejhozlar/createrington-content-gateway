export interface paths {
  "/v1/budget": {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    /** This minute's calls per pool, the 429 hold, and the daily counts of the last UTC days. */
    get: operations["readout"];
    put?: never;
    post?: never;
    delete?: never;
    options?: never;
    head?: never;
    patch?: never;
    trace?: never;
  };
  "/v1/curseforge/{path}": {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    /**
     * What `GET api.curseforge.com/v1/{path}` answers, from the cache while it is fresh.
     * @description The query string goes to CurseForge as it came, and the answer comes back with CurseForge's status, content type
     *     and body, marked `X-Cache`. The routes of the cache table are kept for their fresh time; every other route is
     *     asked each time. `Cache-Control: no-cache` asks CurseForge again and keeps the new answer. A kept answer that is
     *     no longer fresh is still given, marked `stale`, when the budget refuses the call or CurseForge fails.
     */
    get: operations["passthrough_get"];
    put?: never;
    /**
     * What `POST api.curseforge.com/v1/{path}` answers to this body.
     * @description `POST mods` with only `{"modIds": [...]}`, `POST mods/files` with only `{"fileIds": [...]}`, and
     *     `POST fingerprints/432` or `POST fingerprints` with only `{"fingerprints": [...]}` are answered per id from the
     *     cache, and only the ids not kept are asked for, 100 per call. Every other body goes to CurseForge each time.
     */
    post: operations["passthrough_post"];
    delete?: never;
    options?: never;
    head?: never;
    patch?: never;
    trace?: never;
  };
  "/v1/healthz": {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    /** Whether the gateway is up, and which build it is. */
    get: operations["healthz"];
    put?: never;
    post?: never;
    delete?: never;
    options?: never;
    head?: never;
    patch?: never;
    trace?: never;
  };
  "/v1/modrinth/version_file/{hash}": {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    /**
     * The Modrinth version of the file with this SHA-1, as `GET api.modrinth.com/v2/version_file/{hash}` answers it.
     * @description Kept for an hour once found and for 10 minutes when Modrinth does not know the hash. `Cache-Control: no-cache`
     *     asks Modrinth again. Modrinth calls do not count against the CurseForge budget.
     */
    get: operations["version_file"];
    put?: never;
    post?: never;
    delete?: never;
    options?: never;
    head?: never;
    patch?: never;
    trace?: never;
  };
  "/v1/modrinth/version_files": {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    get?: never;
    put?: never;
    /**
     * The Modrinth versions of the files with these SHA-1s, keyed by hash, as `POST api.modrinth.com/v2/version_files`
     *     answers them: a hash Modrinth does not know is left out.
     * @description Answered per hash from the cache; only the hashes not kept are asked for, 100 per call, at most 1000 hashes.
     */
    post: operations["version_files"];
    delete?: never;
    options?: never;
    head?: never;
    patch?: never;
    trace?: never;
  };
  "/v1/openapi.json": {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    /** This document. */
    get: operations["document"];
    put?: never;
    post?: never;
    delete?: never;
    options?: never;
    head?: never;
    patch?: never;
    trace?: never;
  };
  "/v1/resolve": {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    get?: never;
    put?: never;
    /**
     * A download address for each CurseForge file: CurseForge's own link, else Modrinth's copy of the file found by
     *     its SHA-1, else the file's built address on CurseForge's CDN when a HEAD answers 200 with the file's size.
     * @description The files are read from the cache like `POST /v1/curseforge/mods/files`, and the CurseForge calls for the ones
     *     not kept count against the budget. Modrinth answers and CDN verdicts are kept for an hour, misses for 10
     *     minutes. No bytes pass through the gateway: the consumer downloads from the address given. A null `source` with
     *     `retry` false means no source has the file; `retry` true means a source could not be asked this time. At worst
     *     a call takes 40 s: 10 s for CurseForge, 10 s for Modrinth and 20 s for the CDN checks.
     */
    post: operations["resolve"];
    delete?: never;
    options?: never;
    head?: never;
    patch?: never;
    trace?: never;
  };
}
export type webhooks = Record<string, never>;
export interface components {
  schemas: {
    /** @description The JSON error every route answers with: `{ code, message }`, plus `pool` and `retryAfterMs` on a refused call. */
    ApiError: {
      code: components["schemas"]["ErrorCode"];
      /** @description What happened, in one sentence for a log line. */
      message: string;
      /** @description The pool the refused call would have drawn from, on a refused call and an unknown pool. */
      pool?: string | null;
      /**
       * Format: int64
       * @description When the next call can go out, on a refused call. `Retry-After` carries the same in whole seconds.
       */
      retryAfterMs?: number | null;
    };
    /** @description What `/v1/budget` answers: this minute over all pools and per pool, the 429 hold, and the last UTC days. */
    BudgetSnapshot: {
      /** @description The UTC days asked for, today first; a day without a single count is left out. */
      days: components["schemas"]["DaySnapshot"][];
      /**
       * Format: date-time
       * @description Until when CurseForge's 429 holds every call off, ISO 8601, or null while no hold is on.
       */
      heldUntil?: string | null;
      /**
       * Format: int32
       * @description UTC days of daily counts the gateway keeps, the most `days` can cover.
       */
      historyDays: number;
      /**
       * Format: int32
       * @description Calls to CurseForge the minute allows over all pools.
       */
      limitPerMinute: number;
      /** @description Every pool of every consumer, this minute. */
      pools: components["schemas"]["PoolSnapshot"][];
      /**
       * Format: date-time
       * @description When this minute's counts start over, ISO 8601.
       */
      resetsAt: string;
      source: components["schemas"]["Source"];
      /**
       * Format: int32
       * @description Calls admitted this minute.
       */
      used: number;
      /**
       * Format: int32
       * @description Calls refused this minute.
       */
      withheld: number;
    };
    /** @description What one pool did on one UTC day. */
    DayCounts: {
      /**
       * Format: int64
       * @description Calls that reached CurseForge.
       */
      calls: number;
      /**
       * Format: int64
       * @description Calls CurseForge answered 403.
       */
      forbidden: number;
      /**
       * Format: int64
       * @description Requests answered without a call: from the cache, or by joining a call already in flight.
       */
      hits: number;
      /**
       * Format: int64
       * @description Calls CurseForge answered 429.
       */
      throttled: number;
      /**
       * Format: int64
       * @description Calls the budget refused.
       */
      withheld: number;
    };
    /** @description One pool's counts on one UTC day. */
    DayPoolSnapshot: components["schemas"]["DayCounts"] & {
      consumer: string;
      pool: string;
    };
    /** @description One UTC day's counts for every pool. */
    DaySnapshot: {
      /**
       * Format: date
       * @description The UTC day, `YYYY-MM-DD`.
       */
      date: string;
      pools: components["schemas"]["DayPoolSnapshot"][];
    };
    /**
     * @description What went wrong, for a consumer to branch on.
     * @enum {string}
     */
    ErrorCode:
      | "UNAUTHORIZED"
      | "POOL_REQUIRED"
      | "UNKNOWN_POOL"
      | "INVALID_PATH"
      | "INVALID_DAYS"
      | "INVALID_HASH"
      | "INVALID_BODY"
      | "UPSTREAM_TIMEOUT"
      | "UPSTREAM_UNREACHABLE"
      | "UPSTREAM_TOO_LARGE"
      | "UPSTREAM_UNREADABLE"
      | "BUDGET_SPENT"
      | "UPSTREAM_HOLD"
      | "REQUEST_TOO_LARGE";
    /**
     * @description Where a file's download address comes from.
     * @enum {string}
     */
    FileSource: "curseforge" | "modrinth" | "curseforge-cdn";
    /** @description What `/v1/healthz` answers: the running gateway's version and build. */
    Health: {
      /** @description When the binary was built, ISO 8601 in UTC, when the build said. */
      builtAt?: string | null;
      /** @description The commit the binary was built from, when the build said. */
      commit?: string | null;
      /**
       * @description Always `ok` when the gateway answers at all.
       * @example ok
       */
      status: string;
      /**
       * @description The gateway's version, as released.
       * @example 0.1.0
       */
      version: string;
    };
    /** @description One pool's calls this minute. */
    PoolSnapshot: {
      consumer: string;
      pool: string;
      /**
       * Format: int32
       * @description Calls admitted this minute.
       */
      used: number;
      /**
       * Format: int32
       * @description Calls refused this minute.
       */
      withheld: number;
    };
    /** @description What `POST /v1/resolve` answers. */
    Resolved: {
      /** @description One entry per file id asked for, in the order asked, each id once. */
      files: components["schemas"]["ResolvedFile"][];
    };
    /** @description What `POST /v1/resolve` answers for each file asked for. */
    ResolvedFile: {
      /** Format: int64 */
      fileId: number;
      /** @description The file's name as CurseForge lists it, null when CurseForge does not know the file. */
      fileName?: string | null;
      /**
       * @description True when a source could not be asked for the file, so that a null `source` is worth asking again later
       *     rather than remembering.
       */
      retry: boolean;
      /** @description The file's SHA-1 as CurseForge lists it, null when CurseForge does not know the file. */
      sha1?: string | null;
      /**
       * Format: int64
       * @description The file's size in bytes as CurseForge lists it, null when CurseForge does not know the file.
       */
      size?: number | null;
      source?: components["schemas"]["FileSource"] | null;
      /** @description The address to download the file from, or null when no source has it. */
      url?: string | null;
    };
    /** @description The body of `POST /v1/resolve`. */
    ResolveRequest: {
      /**
       * @description Whether a file's built address on CurseForge's CDN may be checked and given when neither CurseForge nor
       *     Modrinth has a link. On unless set to false.
       * @default true
       */
      cdn: boolean;
      /**
       * @description CurseForge file ids, up to 1000.
       * @example [
       *       4567890,
       *       4567891
       *     ]
       */
      fileIds: number[];
    };
    /**
     * @description Where a readout's counts come from: `redis` when they are the stored ones, `memory` when no Redis is configured,
     *     `unavailable` when Redis did not answer, so the minute is this process's own count and the days hold only what
     *     is not written to Redis yet.
     * @enum {string}
     */
    Source: "redis" | "memory" | "unavailable";
    /** @description The body of `POST /v1/modrinth/version_files`. */
    VersionFilesRequest: {
      /**
       * @description Only `sha1` is supported.
       * @example sha1
       */
      algorithm?: string;
      /**
       * @description SHA-1 hashes of the wanted files, 40 hex digits each, up to 1000.
       * @example [
       *       "3e1dd9fb5ea2f0dd4ffcc2b1a7d0bb1a1a9d6a8e"
       *     ]
       */
      hashes: string[];
    };
  };
  responses: never;
  parameters: never;
  requestBodies: never;
  headers: never;
  pathItems: never;
}
export type $defs = Record<string, never>;
export interface operations {
  readout: {
    parameters: {
      query?: {
        /** @description UTC days to answer, today included, 1 up; 14 when left out, and more than the days kept answers the days kept. */
        days?: number;
      };
      header?: never;
      path?: never;
      cookie?: never;
    };
    requestBody?: never;
    responses: {
      /** @description The budget as it stands */
      200: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["BudgetSnapshot"];
        };
      };
      /** @description `days` is not a whole number from 1 up */
      400: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description No valid consumer token */
      401: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
    };
  };
  passthrough_get: {
    parameters: {
      query?: never;
      header?: {
        /** @description `no-cache` asks CurseForge again instead of answering from the cache */
        "Cache-Control"?: string | null;
        /** @description The pool the call draws from. Required when the token has more than one pool */
        "X-Budget-Pool"?: string | null;
      };
      path: {
        /**
         * @description The path under `api.curseforge.com/v1/`, such as `mods/search` or `mods/238222/files`: segments of letters, digits, `-` and `_`
         * @example mods/search
         */
        path: string;
      };
      cookie?: never;
    };
    requestBody?: never;
    responses: {
      /** @description CurseForge's answer, as it came */
      200: {
        headers: {
          /** @description `hit` from the cache, `miss` from CurseForge, `stale` an old answer given because CurseForge could not be asked */
          "X-Cache"?: string;
          [name: string]: unknown;
        };
        content: {
          "application/json": unknown;
        };
      };
      /** @description `INVALID_PATH`, `POOL_REQUIRED` or `UNKNOWN_POOL` */
      400: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description No valid consumer token */
      401: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description CurseForge could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE` */
      502: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description The call was refused: `BUDGET_SPENT` this minute or `UPSTREAM_HOLD` after a 429 from CurseForge */
      503: {
        headers: {
          /** @description Seconds until the next call can go out */
          "Retry-After"?: number;
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description CurseForge did not answer in time, `UPSTREAM_TIMEOUT` */
      504: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description Any other status CurseForge answered, with its body. Only the gateway's own answers lack `X-Cache` */
      default: {
        headers: {
          [name: string]: unknown;
        };
        content?: never;
      };
    };
  };
  passthrough_post: {
    parameters: {
      query?: never;
      header?: {
        /** @description `no-cache` asks CurseForge again instead of answering from the cache */
        "Cache-Control"?: string | null;
        /** @description The pool the call draws from. Required when the token has more than one pool */
        "X-Budget-Pool"?: string | null;
      };
      path: {
        /**
         * @description The path under `api.curseforge.com/v1/`, such as `mods` or `fingerprints/432`: segments of letters, digits, `-` and `_`
         * @example mods
         */
        path: string;
      };
      cookie?: never;
    };
    /** @description The JSON body CurseForge expects at this path */
    requestBody: {
      content: {
        "application/json": unknown;
      };
    };
    responses: {
      /** @description CurseForge's answer, as it came, or one built from the kept rows of a batch */
      200: {
        headers: {
          /** @description `hit` when every id was kept, `miss` when CurseForge was asked, `stale` an old answer given because CurseForge could not be asked */
          "X-Cache"?: string;
          [name: string]: unknown;
        };
        content: {
          "application/json": unknown;
        };
      };
      /** @description `INVALID_PATH`, `POOL_REQUIRED` or `UNKNOWN_POOL` */
      400: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description No valid consumer token */
      401: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description The batch needs more calls than a whole minute allows, `REQUEST_TOO_LARGE`; split it */
      413: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description CurseForge could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE` */
      502: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description The call was refused: `BUDGET_SPENT` this minute or `UPSTREAM_HOLD` after a 429 from CurseForge */
      503: {
        headers: {
          /** @description Seconds until the next call can go out */
          "Retry-After"?: number;
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description CurseForge did not answer in time, `UPSTREAM_TIMEOUT` */
      504: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description Any other status CurseForge answered, with its body. Only the gateway's own answers lack `X-Cache` */
      default: {
        headers: {
          [name: string]: unknown;
        };
        content?: never;
      };
    };
  };
  healthz: {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    requestBody?: never;
    responses: {
      /** @description The gateway is up */
      200: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["Health"];
        };
      };
    };
  };
  version_file: {
    parameters: {
      query?: never;
      header?: {
        /** @description `no-cache` asks Modrinth again instead of answering from the cache */
        "Cache-Control"?: string | null;
      };
      path: {
        /**
         * @description The file's SHA-1, 40 hex digits
         * @example 3e1dd9fb5ea2f0dd4ffcc2b1a7d0bb1a1a9d6a8e
         */
        hash: string;
      };
      cookie?: never;
    };
    requestBody?: never;
    responses: {
      /** @description The version Modrinth lists the file under, as Modrinth sent it */
      200: {
        headers: {
          /** @description `hit` from the cache, `miss` from Modrinth, `stale` an old answer given because Modrinth could not be asked */
          "X-Cache"?: string;
          [name: string]: unknown;
        };
        content: {
          "application/json": unknown;
        };
      };
      /** @description `INVALID_HASH` */
      400: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description No valid consumer token */
      401: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description Modrinth does not know the hash */
      404: {
        headers: {
          /** @description As on 200 */
          "X-Cache"?: string;
          [name: string]: unknown;
        };
        content?: never;
      };
      /** @description Modrinth could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE` */
      502: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description Modrinth did not answer in time, `UPSTREAM_TIMEOUT` */
      504: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description Any other status Modrinth answered, with its body. Only the gateway's own answers lack `X-Cache` */
      default: {
        headers: {
          [name: string]: unknown;
        };
        content?: never;
      };
    };
  };
  version_files: {
    parameters: {
      query?: never;
      header?: {
        /** @description `no-cache` asks Modrinth again instead of answering from the cache */
        "Cache-Control"?: string | null;
      };
      path?: never;
      cookie?: never;
    };
    requestBody: {
      content: {
        "application/json": components["schemas"]["VersionFilesRequest"];
      };
    };
    responses: {
      /** @description The versions found, keyed by lowercase hash, each as Modrinth sent it */
      200: {
        headers: {
          /** @description `hit` when every hash was kept, `miss` when Modrinth was asked, `stale` an old answer given because Modrinth could not be asked */
          "X-Cache"?: string;
          [name: string]: unknown;
        };
        content: {
          "application/json": unknown;
        };
      };
      /** @description `INVALID_BODY` (no hashes, more than 1000, or another algorithm) or `INVALID_HASH` */
      400: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description No valid consumer token */
      401: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description Modrinth could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE` */
      502: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description Modrinth did not answer in time, `UPSTREAM_TIMEOUT` */
      504: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description Any other status Modrinth answered, with its body. Only the gateway's own answers lack `X-Cache` */
      default: {
        headers: {
          [name: string]: unknown;
        };
        content?: never;
      };
    };
  };
  document: {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    requestBody?: never;
    responses: {
      /** @description The OpenAPI document of every route */
      200: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": unknown;
        };
      };
    };
  };
  resolve: {
    parameters: {
      query?: never;
      header?: {
        /** @description `no-cache` asks every source again instead of answering from the cache */
        "Cache-Control"?: string | null;
        /** @description The pool the CurseForge calls draw from. Required when the token has more than one pool */
        "X-Budget-Pool"?: string | null;
      };
      path?: never;
      cookie?: never;
    };
    requestBody: {
      content: {
        "application/json": components["schemas"]["ResolveRequest"];
      };
    };
    responses: {
      /** @description An address per file, in the order asked */
      200: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["Resolved"];
        };
      };
      /** @description `INVALID_BODY`, `POOL_REQUIRED` or `UNKNOWN_POOL` */
      400: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description No valid consumer token */
      401: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description The files need more CurseForge calls than a whole minute allows, `REQUEST_TOO_LARGE`; split them */
      413: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description CurseForge could not be reached or answered something the gateway cannot keep: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE` */
      502: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description The CurseForge call was refused: `BUDGET_SPENT` this minute or `UPSTREAM_HOLD` after a 429 */
      503: {
        headers: {
          /** @description Seconds until the next call can go out */
          "Retry-After"?: number;
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description CurseForge did not answer in time, `UPSTREAM_TIMEOUT` */
      504: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
    };
  };
}
