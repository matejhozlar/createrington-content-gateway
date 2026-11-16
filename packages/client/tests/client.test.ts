import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
  ContentGateway,
  ContentGatewayError,
  DEFAULT_TIMEOUT_MS,
  RESOLVE_TIMEOUT_MS,
  type ApiError,
  type BudgetSnapshot,
  type ContentGatewayOptions,
  type Health,
  type JavaRuntime,
  type KnownErrorCode,
  type LoaderVersions,
  type MinecraftVersionDetails,
  type MinecraftVersions,
  type Resolved,
  type UpstreamBudget,
} from "../src/index.js";
import { FakeGateway, json } from "./gateway.js";

const TOKEN = "app-token-0123456789abcdef0123456789";

let gateway: FakeGateway;

beforeEach(async () => {
  gateway = new FakeGateway();
  await gateway.start();
});

afterEach(async () => {
  await gateway.stop();
});

function client(options: Partial<ContentGatewayOptions> = {}) {
  return new ContentGateway({
    baseUrl: `${gateway.baseUrl}/`,
    token: TOKEN,
    ...options,
  });
}

async function failure(call: Promise<unknown>): Promise<unknown> {
  try {
    await call;
  } catch (error) {
    return error;
  }
  throw new Error("the call did not fail");
}

async function gatewayError(
  call: Promise<unknown>,
): Promise<ContentGatewayError> {
  const error = await failure(call);
  expect(error).toBeInstanceOf(ContentGatewayError);
  return error as ContentGatewayError;
}

async function failed(call: Promise<unknown>): Promise<Error> {
  const error = await failure(call);
  expect(error).toBeInstanceOf(Error);
  return error as Error;
}

function apiError(
  code: KnownErrorCode,
  message: string,
  refusal: Pick<ApiError, "pool" | "retryAfterMs"> = {},
): ApiError {
  return { code, message, ...refusal };
}

const health = {
  status: "ok",
  version: "0.1.0",
  commit: null,
  builtAt: null,
} satisfies Health;

const upstreamBudget = {
  limitPerMinute: 300,
  used: 2,
  withheld: 0,
  resetsAt: "2026-11-03T20:01:00Z",
  heldUntil: null,
  pools: [],
  days: [],
} satisfies UpstreamBudget;

const budget = {
  historyDays: 90,
  source: "redis",
  curseforge: upstreamBudget,
  modrinth: { ...upstreamBudget, used: 0 },
} satisfies BudgetSnapshot;

const resolved = {
  files: [
    {
      fileId: 1,
      source: "curseforge-cdn",
      url: "https://mediafilez.forgecdn.net/files/0/1/a.jar",
      sha1: "abc",
      fileName: "a.jar",
      size: 10,
      retry: false,
    },
  ],
} satisfies Resolved;

const minecraftVersions = {
  latest: { release: "1.21.1", snapshot: "24w33a" },
  versions: [
    { id: "1.21.1", type: "release", releaseTime: "2024-08-08T00:00:00Z" },
  ],
} satisfies MinecraftVersions;

const minecraftVersion = {
  id: "1.21.1",
  type: "release",
  releaseTime: "2024-08-08T00:00:00Z",
  javaMajor: 21,
} satisfies MinecraftVersionDetails;

const loaderVersions = {
  loader: "neoforge",
  minecraft: "1.21.1",
  versions: [
    {
      version: "21.1.72",
      minecraft: "1.21.1",
      stable: true,
      recommended: false,
    },
  ],
} satisfies LoaderVersions;

const javaRuntime = {
  platform: "linux",
  component: "java-runtime-delta",
  major: 21,
  version: "21.0.7",
  released: "2025-05-19T08:30:12+00:00",
  manifest: { url: "https://example.com/manifest.json", sha1: "d", size: 1 },
} satisfies JavaRuntime;

/** An abort signal from another realm or a polyfill: the shape of `AbortSignal` without its prototype. */
class ForeignSignal extends EventTarget {
  aborted = false;
  reason: unknown = undefined;

  abort(reason: unknown): void {
    this.aborted = true;
    this.reason = reason;
    this.dispatchEvent(new Event("abort"));
  }
}

const budgetSpent = apiError(
  "BUDGET_SPENT",
  "the gateway has asked CurseForge enough this minute",
  { pool: "jobs", retryAfterMs: 5_000 },
);

describe("the gateway's own routes", () => {
  it("reads the health without a token", async () => {
    gateway.reply(json(200, health));
    expect((await client().health()).version).toBe("0.1.0");
    expect(gateway.seen[0]?.url).toBe("/v1/healthz");
    expect(gateway.seen[0]?.headers.authorization).toBeUndefined();
  });

  it("reads the budget with the token and the pool", async () => {
    gateway.reply(json(200, budget));
    const answer = await client({ pool: "launcher" }).budget({ days: 30 });
    expect(answer.curseforge.used).toBe(2);
    expect(answer.modrinth.used).toBe(0);
    expect(answer.historyDays).toBe(90);
    expect(gateway.seen[0]?.url).toBe("/v1/budget?days=30");
    expect(gateway.seen[0]?.headers.authorization).toBe(`Bearer ${TOKEN}`);
    expect(gateway.seen[0]?.headers["x-budget-pool"]).toBe("launcher");
  });

  it("throws the gateway's error with its code", async () => {
    gateway.reply(
      json(
        401,
        apiError(
          "UNAUTHORIZED",
          "send a consumer token as Authorization: Bearer <token>",
        ),
      ),
    );
    const error = await gatewayError(client().budget());
    expect(error.status).toBe(401);
    expect(error.code).toBe("UNAUTHORIZED");
    expect(error.message).toContain("Bearer");
    expect(error.retryAfterMs).toBeNull();
  });

  it("keeps an error code this client does not know yet", async () => {
    gateway.reply(
      json(400, { code: "SOMETHING_NEWER", message: "a newer gateway" }),
    );
    const error = await gatewayError(client().budget());
    expect(error.code).toBe("SOMETHING_NEWER");
    expect(error.message).toBe("a newer gateway");
  });

  it("throws on a success that is not the gateway's JSON", async () => {
    gateway.reply(
      {
        status: 200,
        headers: { "content-type": "text/html" },
        body: "<html>another service</html>",
      },
      { status: 200, body: "" },
      json(200, [1, 2]),
      json(200, { loader: "fabric", versions: [] }, "warm"),
    );
    for (const call of [
      () => client().health(),
      () => client().budget(),
      () => client().minecraftVersions(),
      () => client().loaderVersions("fabric"),
    ]) {
      const error = await gatewayError(call());
      expect(error.status).toBe(200);
      expect(error.code).toBeNull();
      expect(error.message).toContain("is not the gateway's JSON");
    }
  });

  it("stops when the caller aborts, with the signal in the options", async () => {
    gateway.reply({ ...json(200, health), delayMs: 300 });
    const controller = new AbortController();
    setTimeout(() => controller.abort(new Error("done")), 20);
    const error = await failed(client().health({ signal: controller.signal }));
    expect(error.message).toBe("done");
  });

  it("stops when the caller aborts, with the signal itself", async () => {
    gateway.reply(
      { ...json(200, health), delayMs: 300 },
      { ...json(200, minecraftVersion), delayMs: 300 },
    );
    const controller = new AbortController();
    setTimeout(() => controller.abort(new Error("done")), 20);
    const calls = [
      client().health(controller.signal),
      client().minecraftVersion("1.21.1", controller.signal),
    ];
    for (const call of calls) {
      expect((await failed(call)).message).toBe("done");
    }
  });

  it("follows a signal that is not this realm's AbortSignal", async () => {
    gateway.reply(
      { ...json(200, health), delayMs: 300 },
      { ...json(200, health), delayMs: 300 },
    );
    const foreign = new ForeignSignal();
    setTimeout(() => foreign.abort(new Error("done")), 20);
    const signal = foreign as unknown as AbortSignal;
    expect(signal).not.toBeInstanceOf(AbortSignal);
    const calls = [client().health(signal), client().health({ signal })];
    for (const call of calls) {
      expect((await failed(call)).message).toBe("done");
    }
  });
});

describe("the version lists", () => {
  it("lists Minecraft versions narrowed to a loader", async () => {
    gateway.reply(json(200, minecraftVersions));
    const versions = await client().minecraftVersions({ loader: "neoforge" });
    expect(versions.versions[0]?.id).toBe("1.21.1");
    expect(gateway.seen[0]?.url).toBe("/v1/minecraft/versions?loader=neoforge");
  });

  it("reads one version, its id encoded in the path", async () => {
    gateway.reply(json(200, minecraftVersion), json(200, minecraftVersion));
    expect((await client().minecraftVersion("1.21.1")).javaMajor).toBe(21);
    expect(gateway.seen[0]?.url).toBe("/v1/minecraft/versions/1.21.1");

    await client().minecraftVersion("1.14 Pre-Release 1/2");
    expect(gateway.seen[1]?.url).toBe(
      "/v1/minecraft/versions/1.14%20Pre-Release%201%2F2",
    );
  });

  it("throws an unknown version as the gateway's error", async () => {
    gateway.reply(json(404, apiError("UNKNOWN_VERSION", "no such version")));
    const error = await gatewayError(client().minecraftVersion("1.99"));
    expect(error.code).toBe("UNKNOWN_VERSION");
  });

  it("lists a loader's builds for one Minecraft version", async () => {
    gateway.reply(json(200, loaderVersions));
    const builds = await client().loaderVersions("neoforge", {
      minecraft: "1.21.1",
    });
    expect(builds.versions[0]?.version).toBe("21.1.72");
    expect(gateway.seen[0]?.url).toBe(
      "/v1/minecraft/loaders/neoforge/versions?minecraft=1.21.1",
    );
  });

  it("asks for a Java runtime by major or by component", async () => {
    gateway.reply(json(200, javaRuntime), json(200, javaRuntime));
    expect(await client().javaRuntime("linux", { major: 21 })).toEqual(
      javaRuntime,
    );
    expect(gateway.seen[0]?.url).toBe(
      "/v1/minecraft/java?platform=linux&major=21",
    );

    await client().javaRuntime("windows-x64", {
      component: "java-runtime-delta",
    });
    expect(gateway.seen[1]?.url).toBe(
      "/v1/minecraft/java?platform=windows-x64&component=java-runtime-delta",
    );

    gateway.reply(json(404, apiError("UNKNOWN_RUNTIME", "no such runtime")));
    const error = await gatewayError(
      client().javaRuntime("linux", { major: 25 }),
    );
    expect(error.code).toBe("UNKNOWN_RUNTIME");
  });
});

describe("resolve", () => {
  it("resolves files with the pool and without the CDN when asked", async () => {
    gateway.reply(json(200, resolved));
    const answer = await client({ pool: "launcher" }).resolve([1, 1], {
      cdn: false,
      pool: "app",
    });
    expect(answer.files[0]?.source).toBe("curseforge-cdn");
    expect(gateway.seen[0]?.url).toBe("/v1/resolve");
    expect(gateway.seen[0]?.body).toBe('{"fileIds":[1,1],"cdn":false}');
    expect(gateway.seen[0]?.headers["x-budget-pool"]).toBe("app");
  });

  it("leaves cdn to the gateway's default when unset", async () => {
    gateway.reply(json(200, resolved));
    await client().resolve([1]);
    expect(gateway.seen[0]?.body).toBe('{"fileIds":[1]}');
  });

  it("throws a refused call with its code", async () => {
    gateway.reply(json(503, budgetSpent));
    const error = await gatewayError(client().resolve([2]));
    expect(error.code).toBe("BUDGET_SPENT");
  });

  it("throws CurseForge's failure that resolve passes on", async () => {
    gateway.reply(
      json(500, { code: "NOT_FOUND", message: "CurseForge says no" }, "miss"),
    );
    const error = await gatewayError(client().resolve([1]));
    expect(error.status).toBe(500);
    expect(error.code).toBeNull();
    expect(error.message).toBe(
      "the 500 answer to POST /v1/resolve is an upstream's, passed on by the gateway (X-Cache: miss)",
    );
  });

  it("gives resolve longer than the gateway's three sources in a row", async () => {
    const gatewayWorstCaseMs = 10_000 + 10_000 + 20_000;
    expect(RESOLVE_TIMEOUT_MS).toBeGreaterThan(gatewayWorstCaseMs);
    expect(DEFAULT_TIMEOUT_MS).toBeGreaterThan(10_000);

    gateway.reply({ ...json(200, { files: [] }), delayMs: 50 });
    const error = await failed(
      client({ timeoutMs: 10 }).resolve([1], { timeoutMs: 10 }),
    );
    expect(error.name).toBe("TimeoutError");
  });
});

describe("the CurseForge passthrough", () => {
  it("sends a GET with the query and answers what came, marked by X-Cache", async () => {
    gateway.reply(
      json(200, { data: [{ id: 1, name: "JEI" }], pagination: {} }, "hit"),
    );
    const answer = await client({ pool: "launcher" }).curseforge(
      "/mods/search",
      { query: { gameId: 432, searchFilter: "jei mod", classId: undefined } },
    );
    expect(answer.status).toBe(200);
    expect(answer.ok).toBe(true);
    expect(answer.cache).toBe("hit");
    expect(answer.body).toEqual({
      data: [{ id: 1, name: "JEI" }],
      pagination: {},
    });

    const seen = gateway.seen[0];
    expect(seen?.method).toBe("GET");
    expect(seen?.url).toBe(
      "/v1/curseforge/mods/search?gameId=432&searchFilter=jei+mod",
    );
    expect(seen?.headers.authorization).toBe(`Bearer ${TOKEN}`);
    expect(seen?.headers["x-budget-pool"]).toBe("launcher");
    expect(seen?.headers.accept).toBe("application/json");
    expect(seen?.headers["cache-control"]).toBeUndefined();
  });

  it("sends a URLSearchParams query as it is, repeated names included", async () => {
    gateway.reply(json(200, { data: [] }, "hit"));
    const query = new URLSearchParams([
      ["gameVersion", "1.21.1"],
      ["gameVersion", "1.21"],
    ]);
    await client().curseforge("mods/1/files", { query });
    expect(gateway.seen[0]?.url).toBe(
      "/v1/curseforge/mods/1/files?gameVersion=1.21.1&gameVersion=1.21",
    );
  });

  it("posts a JSON body and lets the call name its own pool", async () => {
    gateway.reply(json(200, { data: [] }, "miss"));
    await client({ pool: "launcher" }).curseforge("mods", {
      body: { modIds: [1, 2] },
      pool: "app",
    });
    const seen = gateway.seen[0];
    expect(seen?.method).toBe("POST");
    expect(seen?.url).toBe("/v1/curseforge/mods");
    expect(seen?.headers["content-type"]).toBe("application/json");
    expect(seen?.body).toBe('{"modIds":[1,2]}');
    expect(seen?.headers["x-budget-pool"]).toBe("app");
  });

  it("refuses a GET with a body before sending anything", async () => {
    const request = { method: "GET", body: { modIds: [1] } } as const;
    const error = await failed(client().curseforge("mods", request as never));
    expect(error).toBeInstanceOf(TypeError);
    expect(error.message).toContain("cannot carry a body");
    expect(gateway.seen).toHaveLength(0);
  });

  it("asks for a fresh answer with Cache-Control: no-cache", async () => {
    gateway.reply(json(200, { data: [] }, "miss"));
    await client().curseforge("mods/1/files", { fresh: true });
    expect(gateway.seen[0]?.headers["cache-control"]).toBe("no-cache");
  });

  it("answers a stale answer the gateway kept", async () => {
    gateway.reply(json(200, { data: { id: 1 } }, "stale"));
    const answer = await client().curseforge("mods/1");
    expect(answer.cache).toBe("stale");
    expect(answer.body).toEqual({ data: { id: 1 } });
  });

  it("passes CurseForge's own errors on as answers", async () => {
    gateway.reply({ status: 404, headers: { "x-cache": "hit" } });
    const answer = await client().curseforge("mods/1");
    expect(answer.status).toBe(404);
    expect(answer.ok).toBe(false);
    expect(answer.body).toBeNull();
    expect(answer.cache).toBe("hit");
  });

  it("keeps a body that is not JSON as text", async () => {
    gateway.reply({
      status: 502,
      headers: { "x-cache": "miss", "content-type": "text/html" },
      body: "<h1>Bad gateway</h1>",
    });
    const answer = await client().curseforge("mods/1");
    expect(answer.body).toBe("<h1>Bad gateway</h1>");
  });
});

describe("the Modrinth passthrough", () => {
  it("sends Modrinth calls through with the pool", async () => {
    gateway.reply(json(200, { hits: [], total_hits: 0 }, "hit"));
    const search = await client({ pool: "launcher" }).modrinth("/search", {
      query: { query: "sodium", limit: 20 },
    });
    expect(search.cache).toBe("hit");
    expect(gateway.seen[0]?.method).toBe("GET");
    expect(gateway.seen[0]?.url).toBe(
      "/v1/modrinth/search?query=sodium&limit=20",
    );
    expect(gateway.seen[0]?.headers["x-budget-pool"]).toBe("launcher");
  });

  it("looks one file up by its SHA-1", async () => {
    gateway.reply(json(200, { id: "v", files: [] }, "miss"));
    const one = await client({ pool: "launcher" }).modrinthVersionFile(
      "3E1DD9FB5EA2F0DD4FFCC2B1A7D0BB1A1A9D6A8E",
      { fresh: true, pool: "app" },
    );
    expect(one.cache).toBe("miss");
    expect(one.body).toEqual({ id: "v", files: [] });
    expect(gateway.seen[0]?.method).toBe("GET");
    expect(gateway.seen[0]?.url).toBe(
      "/v1/modrinth/version_file/3E1DD9FB5EA2F0DD4FFCC2B1A7D0BB1A1A9D6A8E",
    );
    expect(gateway.seen[0]?.headers["cache-control"]).toBe("no-cache");
    expect(gateway.seen[0]?.headers["x-budget-pool"]).toBe("app");

    gateway.reply(json(404, "", "hit"));
    const none = await client().modrinthVersionFile("abc");
    expect(none.status).toBe(404);
    expect(none.ok).toBe(false);
  });

  it("looks files up by their SHA-1s in one POST", async () => {
    gateway.reply(json(200, {}, "hit"));
    await client().modrinthVersionFiles(["abc", "def"]);
    expect(gateway.seen[0]?.method).toBe("POST");
    expect(gateway.seen[0]?.url).toBe("/v1/modrinth/version_files");
    expect(gateway.seen[0]?.body).toBe(
      '{"hashes":["abc","def"],"algorithm":"sha1"}',
    );
  });
});

describe("refusals", () => {
  it("throws a refused call with its pool and when to come back", async () => {
    gateway.reply({
      ...json(503, { ...budgetSpent, retryAfterMs: 12_300 }),
      headers: { "content-type": "application/json", "retry-after": "13" },
    });
    const error = await gatewayError(client().curseforge("mods/1"));
    expect(error.status).toBe(503);
    expect(error.code).toBe("BUDGET_SPENT");
    expect(error.pool).toBe("jobs");
    expect(error.retryAfterMs).toBe(12_300);
    expect(gateway.seen).toHaveLength(1);
  });

  it("sleeps out a refusal that fits in waitUpToMs and asks again", async () => {
    gateway.reply(
      json(
        503,
        apiError(
          "UPSTREAM_HOLD",
          "CurseForge answered 429, every call waits until the hold ends",
          { pool: "jobs", retryAfterMs: 40 },
        ),
      ),
      json(200, { data: { id: 1 } }, "miss"),
    );
    const started = Date.now();
    const answer = await client().curseforge("mods/1", { waitUpToMs: 1_000 });
    expect(answer.status).toBe(200);
    expect(gateway.seen).toHaveLength(2);
    expect(Date.now() - started).toBeGreaterThanOrEqual(35);
  });

  it("throws at once when the refusal does not fit in waitUpToMs", async () => {
    gateway.reply(json(503, budgetSpent));
    const started = Date.now();
    const error = await gatewayError(
      client().curseforge("mods/1", { waitUpToMs: 1_000 }),
    );
    expect(error.code).toBe("BUDGET_SPENT");
    expect(Date.now() - started).toBeLessThan(1_000);
    expect(gateway.seen).toHaveLength(1);
  });

  it("stops sleeping out a refusal when the caller aborts", async () => {
    gateway.reply(json(503, budgetSpent));
    const controller = new AbortController();
    setTimeout(() => controller.abort(new Error("done")), 50);
    const started = Date.now();
    const error = await failed(
      client().curseforge("mods/1", {
        waitUpToMs: 60_000,
        signal: controller.signal,
      }),
    );
    expect(error.message).toBe("done");
    expect(Date.now() - started).toBeLessThan(5_000);
    expect(gateway.seen).toHaveLength(1);
  });

  it("takes Retry-After in seconds when the body says nothing", async () => {
    gateway.reply(
      { status: 503, headers: { "retry-after": "1" }, body: "" },
      json(200, {}, "miss"),
    );
    const answer = await client().curseforge("mods/1", { waitUpToMs: 2_000 });
    expect(answer.status).toBe(200);
    expect(gateway.seen).toHaveLength(2);
  });

  it("tells when to come back from Retry-After when the body says nothing", async () => {
    gateway.reply({ status: 503, headers: { "retry-after": "7" }, body: "" });
    const error = await gatewayError(client().curseforge("mods/1"));
    expect(error.code).toBeNull();
    expect(error.retryAfterMs).toBe(7_000);
  });
});

describe("the connection", () => {
  it("sends with the fetch it was given", async () => {
    gateway.reply(json(200, health));
    const sent: string[] = [];
    const gatewayClient = client({
      fetch: (input, init) => {
        sent.push(`${init?.method} ${String(input)}`);
        return globalThis.fetch(input, init);
      },
    });
    await gatewayClient.health();
    expect(sent).toEqual([`GET ${gateway.baseUrl}/v1/healthz`]);
  });

  it("throws fetch's own error when nothing listens at the base URL", async () => {
    const closed = client();
    await gateway.stop();
    const error = await failed(closed.curseforge("mods/1"));
    expect(error).toBeInstanceOf(TypeError);
    expect(error.cause).toMatchObject({ code: "ECONNREFUSED" });
  });

  it("gives up on an answer that takes longer than the timeout", async () => {
    gateway.reply({ ...json(200, {}, "miss"), delayMs: 300 });
    const error = await failed(client({ timeoutMs: 50 }).curseforge("mods/1"));
    expect(error.name).toBe("TimeoutError");
  });

  it("stops when the caller aborts", async () => {
    gateway.reply({ ...json(200, {}, "miss"), delayMs: 300 });
    const controller = new AbortController();
    setTimeout(() => controller.abort(new Error("done")), 20);
    const error = await failed(
      client().curseforge("mods/1", { signal: controller.signal }),
    );
    expect(error.message).toBe("done");
  });

  it("names an answer that is not the gateway's JSON error", async () => {
    gateway.reply({
      status: 502,
      headers: { "content-type": "text/html" },
      body: "<h1>502 Bad Gateway</h1>",
    });
    const error = await gatewayError(client().curseforge("mods/1"));
    expect(error.status).toBe(502);
    expect(error.code).toBeNull();
    expect(error.message).toContain("502");
    expect(error.message).toContain("<h1>502 Bad Gateway</h1>");
  });

  it("refuses an answer the gateway did not mark", async () => {
    gateway.reply(json(200, { data: [] }));
    const error = await gatewayError(client().curseforge("mods/1"));
    expect(error.code).toBeNull();
    expect(error.message).toContain("no X-Cache header");
  });

  it("names an X-Cache value the gateway does not send", async () => {
    gateway.reply(json(200, { data: [] }, "warm"));
    const error = await gatewayError(client().curseforge("mods/1"));
    expect(error.message).toContain(
      'X-Cache "warm" is not one the gateway sends',
    );
  });
});
