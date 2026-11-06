export interface paths {
  "/v1/budget": {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    /** Each upstream's calls this minute per pool, its 429 hold, and its daily counts of the last UTC days. */
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
    get: operations["curseforge_get"];
    put?: never;
    /**
     * What `POST api.curseforge.com/v1/{path}` answers to this body.
     * @description `POST mods` with only `{"modIds": [...]}`, `POST mods/files` with only `{"fileIds": [...]}`, and
     *     `POST fingerprints/432` or `POST fingerprints` with only `{"fingerprints": [...]}` are answered per id from the
     *     cache, and only the ids not kept are asked for, 100 per call. Every other body goes to CurseForge each time.
     */
    post: operations["curseforge_post"];
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
  "/v1/minecraft/java": {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    /**
     * A Java runtime Mojang ships with its launcher, from Mojang's list of runtimes kept an hour.
     * @description `major` answers the newest runtime of that Java major, `component` the runtime of that name, as `javaComponent`
     *     of `GET /v1/minecraft/versions/{id}` names it. The answer points at Mojang's manifest of the runtime, which lists
     *     every file with its address, SHA-1 and size; the files themselves never pass through the gateway.
     */
    get: operations["java_runtime"];
    put?: never;
    post?: never;
    delete?: never;
    options?: never;
    head?: never;
    patch?: never;
    trace?: never;
  };
  "/v1/minecraft/loaders/{loader}/versions": {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    /**
     * A loader's builds, newest first: NeoForge from its Maven, Forge's promoted builds (the recommended and the
     *     latest per Minecraft version, which is what pickers show), Fabric's loaders from its meta. `minecraft` keeps
     *     only the builds for that version; for Fabric, whose loader is not tied to one, that means all of them when
     *     Fabric supports the version and none otherwise. Each list is kept for an hour.
     */
    get: operations["loader_versions"];
    put?: never;
    post?: never;
    delete?: never;
    options?: never;
    head?: never;
    patch?: never;
    trace?: never;
  };
  "/v1/minecraft/versions": {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    /**
     * The Minecraft versions Mojang lists, releases unless asked otherwise, newest first.
     * @description `loader` keeps only the versions that loader has builds for, which is what a server or pack picker offers.
     *     Mojang's manifest is kept for an hour and the old copy answers when Mojang fails.
     */
    get: operations["minecraft_versions"];
    put?: never;
    post?: never;
    delete?: never;
    options?: never;
    head?: never;
    patch?: never;
    trace?: never;
  };
  "/v1/minecraft/versions/{id}": {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    /** One Minecraft version with the Java it needs, from Mojang's own document of the version, kept a day. */
    get: operations["minecraft_version"];
    put?: never;
    post?: never;
    delete?: never;
    options?: never;
    head?: never;
    patch?: never;
    trace?: never;
  };
  "/v1/modrinth/{path}": {
    parameters: {
      query?: never;
      header?: never;
      path?: never;
      cookie?: never;
    };
    /**
     * What `GET api.modrinth.com/v2/{path}` answers, from the cache while it is fresh.
     * @description The query string goes to Modrinth as it came, and the answer comes back with Modrinth's status, content type and
     *     body, marked `X-Cache`. The routes of the cache table are kept for their fresh time; every other route is asked
     *     each time. `version_file/{sha1}` is kept per hash and shared with `POST version_files`. `Cache-Control: no-cache`
     *     asks Modrinth again. Modrinth needs no key but limits calls per address, so every call counts against the
     *     Modrinth budget of the request's pool, and a 429 holds every pool off.
     */
    get: operations["modrinth_get"];
    put?: never;
    /**
     * What `POST api.modrinth.com/v2/{path}` answers to this body.
     * @description `POST version_files` with `{"hashes": [...], "algorithm": "sha1"}` is answered per hash from the cache, keyed by
     *     lowercase hash with unknown hashes left out as Modrinth does, and only the hashes not kept are asked for, 100 per
     *     call, at most 1000 hashes. Every other body goes to Modrinth each time.
     */
    post: operations["modrinth_post"];
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
    /** @description What `/v1/budget` answers: one budget per upstream, under one history and one source. */
    BudgetSnapshot: {
      /** @description The budget of calls on the CurseForge key. */
      curseforge: components["schemas"]["UpstreamBudget"];
      /**
       * Format: int32
       * @description UTC days of daily counts the gateway keeps, the most `days` can cover.
       */
      historyDays: number;
      /** @description The budget of calls to Modrinth, which rate-limits the gateway's address. */
      modrinth: components["schemas"]["UpstreamBudget"];
      source: components["schemas"]["Source"];
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
      | "INVALID_QUERY"
      | "UNKNOWN_VERSION"
      | "UNKNOWN_LOADER"
      | "UNKNOWN_RUNTIME"
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
    /** @description What `GET /v1/minecraft/java` answers: one Java runtime Mojang ships for a platform. */
    JavaRuntime: {
      /** @description Such as `java-runtime-delta`. */
      component: string;
      /**
       * Format: int32
       * @description Such as 21.
       */
      major: number;
      manifest: components["schemas"]["RuntimeManifest"];
      platform: string;
      /** @description When Mojang released this build of the runtime, ISO 8601, or null when Mojang does not say. */
      released?: string | null;
      /** @description The Java version Mojang lists, such as `21.0.7` or `8u202`. */
      version: string;
    };
    /** @description The newest release and snapshot Mojang names, whatever the filter. */
    Latest: {
      release: string;
      snapshot: string;
    };
    /**
     * @description The mod loaders whose versions the gateway lists.
     * @enum {string}
     */
    Loader: "neoforge" | "forge" | "fabric";
    /** @description One build of a loader. */
    LoaderVersion: {
      /** @description The Minecraft version the build is for; null for Fabric, whose loader works with every version it supports. */
      minecraft?: string | null;
      /** @description True for the build its maintainers recommend for the Minecraft version, where they say. */
      recommended: boolean;
      /** @description False for a beta or pre-release build. */
      stable: boolean;
      /** @description Such as `21.1.57` (NeoForge), `52.0.1` (Forge) or `0.16.5` (Fabric). */
      version: string;
    };
    /** @description What `GET /v1/minecraft/loaders/{loader}/versions` answers. */
    LoaderVersions: {
      loader: components["schemas"]["Loader"];
      /** @description The Minecraft version the list was narrowed to, or null for every build. */
      minecraft?: string | null;
      /** @description Newest first. */
      versions: components["schemas"]["LoaderVersion"][];
    };
    /** @description One Minecraft version as Mojang's manifest lists it. */
    MinecraftVersion: {
      /** @description Such as `1.21.1` or `24w33a`. */
      id: string;
      /** @description ISO 8601. */
      releaseTime: string;
      type: components["schemas"]["VersionType"];
    };
    /** @description What `GET /v1/minecraft/versions/{id}` answers: the version and the Java it needs. */
    MinecraftVersionDetails: {
      id: string;
      /** @description Mojang's runtime for it, such as `java-runtime-delta`, the `component` of `GET /v1/minecraft/java`; null when Mojang does not say. */
      javaComponent?: string | null;
      /**
       * Format: int32
       * @description The major version of the Java the version runs on, such as 21, or null when Mojang does not say.
       */
      javaMajor?: number | null;
      releaseTime: string;
      type: components["schemas"]["VersionType"];
    };
    /** @description What `GET /v1/minecraft/versions` answers. */
    MinecraftVersions: {
      latest: components["schemas"]["Latest"];
      /** @description Newest first, as Mojang lists them. */
      versions: components["schemas"]["MinecraftVersion"][];
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
    /** @description Mojang's manifest of a runtime: every file to download, with its address, SHA-1 and size. */
    RuntimeManifest: {
      /** @description The SHA-1 of the manifest itself. */
      sha1: string;
      /**
       * Format: int64
       * @description The size of the manifest in bytes.
       */
      size: number;
      url: string;
    };
    /**
     * @description Where a readout's counts come from: `redis` when they are the stored ones, `memory` when no Redis is configured,
     *     `unavailable` when Redis did not answer, so the minute is this process's own count and the days hold only what
     *     is not written to Redis yet.
     * @enum {string}
     */
    Source: "redis" | "memory" | "unavailable";
    /** @description One upstream's budget: this minute over all pools and per pool, the 429 hold, and the last UTC days. */
    UpstreamBudget: {
      /** @description The UTC days asked for, today first; a day without a single count is left out. */
      days: components["schemas"]["DaySnapshot"][];
      /**
       * Format: date-time
       * @description Until when the upstream's 429 holds every call off, ISO 8601, or null while no hold is on.
       */
      heldUntil?: string | null;
      /**
       * Format: int32
       * @description Calls the minute allows over all pools.
       */
      limitPerMinute: number;
      /** @description Every pool of every consumer, this minute. */
      pools: components["schemas"]["PoolSnapshot"][];
      /**
       * Format: date-time
       * @description When this minute's counts start over, ISO 8601.
       */
      resetsAt: string;
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
    /**
     * @description Which versions `GET /v1/minecraft/versions` lists.
     * @enum {string}
     */
    VersionFilter: "release" | "snapshot" | "all";
    /**
     * @description Mojang's kinds of versions.
     * @enum {string}
     */
    VersionType: "release" | "snapshot" | "old_beta" | "old_alpha";
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
  curseforge_get: {
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
  curseforge_post: {
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
  java_runtime: {
    parameters: {
      query: {
        /** @description The runtime of this name, such as `java-runtime-delta`, as `javaComponent` of a version names it. */
        component?: string;
        /** @description The newest runtime Mojang ships of this Java major, such as 21. */
        major?: number;
        /** @description Mojang's name for the platform: `linux`, `linux-i386`, `mac-os`, `mac-os-arm64`, `windows-x64`, `windows-x86` or `windows-arm64`. */
        platform: string;
      };
      header?: never;
      path?: never;
      cookie?: never;
    };
    requestBody?: never;
    responses: {
      /** @description The runtime */
      200: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["JavaRuntime"];
        };
      };
      /** @description `INVALID_QUERY`: no `platform`, or not exactly one of `major` and `component` */
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
      /** @description Mojang ships no such runtime for the platform, `UNKNOWN_RUNTIME` */
      404: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description Mojang could not be read: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE` */
      502: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description Mojang did not answer in time, `UPSTREAM_TIMEOUT` */
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
  loader_versions: {
    parameters: {
      query?: {
        /** @description Only the builds for this Minecraft version, such as `1.21.1`. */
        minecraft?: string;
      };
      header?: never;
      path: {
        /**
         * @description `neoforge`, `forge` or `fabric`
         * @example neoforge
         */
        loader: string;
      };
      cookie?: never;
    };
    requestBody?: never;
    responses: {
      /** @description The builds */
      200: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["LoaderVersions"];
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
      /** @description No such loader, `UNKNOWN_LOADER` */
      404: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description The loader's source could not be read: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE` */
      502: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description The source did not answer in time, `UPSTREAM_TIMEOUT` */
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
  minecraft_versions: {
    parameters: {
      query?: {
        /** @description Only the versions this loader has at least one build for. */
        loader?: components["schemas"]["Loader"];
        /** @description `release` (the default), `snapshot` (snapshots only) or `all` (every kind, old betas and alphas included). */
        type?: components["schemas"]["VersionFilter"];
      };
      header?: never;
      path?: never;
      cookie?: never;
    };
    requestBody?: never;
    responses: {
      /** @description The versions */
      200: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["MinecraftVersions"];
        };
      };
      /** @description `INVALID_QUERY`: an unknown `type` or `loader` */
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
      /** @description Mojang or the loader's source could not be read: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE` */
      502: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description The source did not answer in time, `UPSTREAM_TIMEOUT` */
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
  minecraft_version: {
    parameters: {
      query?: never;
      header?: never;
      path: {
        /**
         * @description The version as Mojang names it
         * @example 1.21.1
         */
        id: string;
      };
      cookie?: never;
    };
    requestBody?: never;
    responses: {
      /** @description The version */
      200: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["MinecraftVersionDetails"];
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
      /** @description Mojang lists no such version, `UNKNOWN_VERSION` */
      404: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description Mojang could not be read: `UPSTREAM_UNREACHABLE`, `UPSTREAM_TOO_LARGE` or `UPSTREAM_UNREADABLE` */
      502: {
        headers: {
          [name: string]: unknown;
        };
        content: {
          "application/json": components["schemas"]["ApiError"];
        };
      };
      /** @description Mojang did not answer in time, `UPSTREAM_TIMEOUT` */
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
  modrinth_get: {
    parameters: {
      query?: never;
      header?: {
        /** @description `no-cache` asks Modrinth again instead of answering from the cache */
        "Cache-Control"?: string | null;
        /** @description The pool the call draws from. Required when the token has more than one pool */
        "X-Budget-Pool"?: string | null;
      };
      path: {
        /**
         * @description The path under `api.modrinth.com/v2/`, such as `search`, `project/sodium` or `version_file/{sha1}`: segments of letters, digits, `-` and `_`
         * @example search
         */
        path: string;
      };
      cookie?: never;
    };
    requestBody?: never;
    responses: {
      /** @description Modrinth's answer, as it came */
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
      /** @description `INVALID_PATH`, `INVALID_HASH`, `POOL_REQUIRED` or `UNKNOWN_POOL` */
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
      /** @description The call was refused: `BUDGET_SPENT` this minute or `UPSTREAM_HOLD` after a 429 from Modrinth */
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
  modrinth_post: {
    parameters: {
      query?: never;
      header?: {
        /** @description `no-cache` asks Modrinth again instead of answering from the cache */
        "Cache-Control"?: string | null;
        /** @description The pool the call draws from. Required when the token has more than one pool */
        "X-Budget-Pool"?: string | null;
      };
      path: {
        /**
         * @description The path under `api.modrinth.com/v2/`, such as `version_files` or `version_files/update`: segments of letters, digits, `-` and `_`
         * @example version_files
         */
        path: string;
      };
      cookie?: never;
    };
    /** @description The JSON body Modrinth expects at this path; this shape for `version_files` */
    requestBody: {
      content: {
        "application/json": components["schemas"]["VersionFilesRequest"];
      };
    };
    responses: {
      /** @description Modrinth's answer, as it came, or the versions found keyed by hash for `version_files` */
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
      /** @description `INVALID_PATH`, `INVALID_BODY` (no hashes, more than 1000, or another algorithm), `INVALID_HASH`, `POOL_REQUIRED` or `UNKNOWN_POOL` */
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
      /** @description The hashes need more calls than a whole minute allows, `REQUEST_TOO_LARGE`; split them */
      413: {
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
      /** @description The call was refused: `BUDGET_SPENT` this minute or `UPSTREAM_HOLD` after a 429 from Modrinth */
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
