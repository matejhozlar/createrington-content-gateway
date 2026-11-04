export interface paths {
  "/v1/budget": {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    /** This minute's calls per pool, the 429 hold, and the daily counts of the last 14 UTC days. */
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
    /** @description What `/v1/budget` answers: this minute over all pools and per pool, the 429 hold, and the last 14 UTC days. */
    BudgetSnapshot: {
      /** @description The last 14 UTC days, today first. */
      days: components["schemas"]["DaySnapshot"][];
      /**
       * Format: date-time
       * @description Until when CurseForge's 429 holds every call off, ISO 8601, or null while no hold is on.
       */
      heldUntil?: string | null;
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
      | "UPSTREAM_TIMEOUT"
      | "UPSTREAM_UNREACHABLE"
      | "UPSTREAM_TOO_LARGE"
      | "UPSTREAM_UNREADABLE"
      | "BUDGET_SPENT"
      | "UPSTREAM_HOLD"
      | "REQUEST_TOO_LARGE";
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
    /**
     * @description Where a readout's counts come from: `redis` when they are the stored ones, `memory` when no Redis is configured,
     *     `unavailable` when Redis did not answer, so the minute is this process's own count and the days hold only what
     *     is not written to Redis yet.
     * @enum {string}
     */
    Source: "redis" | "memory" | "unavailable";
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
      query?: never;
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
}
