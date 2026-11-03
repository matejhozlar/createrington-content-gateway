import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { ContentGateway, ContentGatewayError } from "../src/index.js";
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

function client(options: { pool?: string; timeoutMs?: number } = {}) {
  return new ContentGateway({
    baseUrl: `${gateway.baseUrl}/`,
    token: TOKEN,
    ...options,
  });
}

describe("the gateway's own routes", () => {
  it("reads the health without a token", async () => {
    gateway.reply(
      json(200, {
        status: "ok",
        version: "0.1.0",
        commit: null,
        builtAt: null,
      }),
    );
    const health = await client().health();
    expect(health.version).toBe("0.1.0");
    expect(gateway.seen[0]?.url).toBe("/v1/healthz");
    expect(gateway.seen[0]?.headers.authorization).toBeUndefined();
  });

  it("reads the budget with the token and the pool", async () => {
    gateway.reply(
      json(200, {
        limitPerMinute: 300,
        used: 2,
        withheld: 0,
        resetsAt: "2026-11-03T20:01:00Z",
        heldUntil: null,
        source: "redis",
        pools: [],
        days: [],
      }),
    );
    const budget = await client({ pool: "launcher" }).budget();
    expect(budget.used).toBe(2);
    expect(gateway.seen[0]?.url).toBe("/v1/budget");
    expect(gateway.seen[0]?.headers.authorization).toBe(`Bearer ${TOKEN}`);
    expect(gateway.seen[0]?.headers["x-budget-pool"]).toBe("launcher");
  });

  it("throws the gateway's error with its code", async () => {
    gateway.reply(
      json(401, {
        code: "UNAUTHORIZED",
        message: "send a consumer token as Authorization: Bearer <token>",
      }),
    );
    const error = await client()
      .budget()
      .catch((error: unknown) => error);
    expect(error).toBeInstanceOf(ContentGatewayError);
    const thrown = error as ContentGatewayError;
    expect(thrown.status).toBe(401);
    expect(thrown.code).toBe("UNAUTHORIZED");
    expect(thrown.message).toContain("Bearer");
    expect(thrown.retryAfterMs).toBeNull();
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
    const error = (await client()
      .curseforge("mods", request as never)
      .catch((error: unknown) => error)) as Error;
    expect(error).toBeInstanceOf(TypeError);
    expect(error.message).toContain("cannot carry a body");
    expect(gateway.seen).toHaveLength(0);
  });

  it("asks for a fresh answer with Cache-Control: no-cache", async () => {
    gateway.reply(json(200, { data: [] }, "miss"));
    await client().curseforge("mods/1/files", { fresh: true });
    expect(gateway.seen[0]?.headers["cache-control"]).toBe("no-cache");
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

  it("throws a refused call with its pool and when to come back", async () => {
    gateway.reply({
      status: 503,
      headers: { "content-type": "application/json", "retry-after": "13" },
      body: JSON.stringify({
        code: "BUDGET_SPENT",
        message: "the gateway has asked CurseForge enough this minute",
        pool: "launcher",
        retryAfterMs: 12_300,
      }),
    });
    const error = (await client()
      .curseforge("mods/1")
      .catch((error: unknown) => error)) as ContentGatewayError;
    expect(error).toBeInstanceOf(ContentGatewayError);
    expect(error.status).toBe(503);
    expect(error.code).toBe("BUDGET_SPENT");
    expect(error.pool).toBe("launcher");
    expect(error.retryAfterMs).toBe(12_300);
    expect(gateway.seen).toHaveLength(1);
  });

  it("sleeps out a refusal that fits in waitUpToMs and asks again", async () => {
    gateway.reply(
      json(503, {
        code: "UPSTREAM_HOLD",
        message:
          "CurseForge answered 429, every call waits until the hold ends",
        pool: "jobs",
        retryAfterMs: 40,
      }),
      json(200, { data: { id: 1 } }, "miss"),
    );
    const started = Date.now();
    const answer = await client().curseforge("mods/1", { waitUpToMs: 1_000 });
    expect(answer.status).toBe(200);
    expect(gateway.seen).toHaveLength(2);
    expect(Date.now() - started).toBeGreaterThanOrEqual(35);
  });

  it("throws at once when the refusal does not fit in waitUpToMs", async () => {
    gateway.reply(
      json(503, {
        code: "BUDGET_SPENT",
        message: "the gateway has asked CurseForge enough this minute",
        pool: "jobs",
        retryAfterMs: 5_000,
      }),
    );
    const started = Date.now();
    const error = (await client()
      .curseforge("mods/1", { waitUpToMs: 1_000 })
      .catch((error: unknown) => error)) as ContentGatewayError;
    expect(error.code).toBe("BUDGET_SPENT");
    expect(Date.now() - started).toBeLessThan(1_000);
    expect(gateway.seen).toHaveLength(1);
  });

  it("takes Retry-After in seconds when the body says nothing", async () => {
    gateway.reply(
      {
        status: 503,
        headers: { "retry-after": "1" },
        body: "",
      },
      json(200, {}, "miss"),
    );
    const answer = await client().curseforge("mods/1", { waitUpToMs: 2_000 });
    expect(answer.status).toBe(200);
    expect(gateway.seen).toHaveLength(2);
  });

  it("gives up on an answer that takes longer than the timeout", async () => {
    gateway.reply({ ...json(200, {}, "miss"), delayMs: 300 });
    const error = (await client({ timeoutMs: 50 })
      .curseforge("mods/1")
      .catch((error: unknown) => error)) as Error;
    expect(error.name).toBe("TimeoutError");
  });

  it("stops when the caller aborts", async () => {
    gateway.reply({ ...json(200, {}, "miss"), delayMs: 300 });
    const controller = new AbortController();
    setTimeout(() => controller.abort(new Error("done")), 20);
    const error = (await client()
      .curseforge("mods/1", { signal: controller.signal })
      .catch((error: unknown) => error)) as Error;
    expect(error.message).toBe("done");
  });

  it("names an answer that is not the gateway's JSON error", async () => {
    gateway.reply({
      status: 502,
      headers: { "content-type": "text/html" },
      body: "<h1>502 Bad Gateway</h1>",
    });
    const error = (await client()
      .curseforge("mods/1")
      .catch((error: unknown) => error)) as ContentGatewayError;
    expect(error).toBeInstanceOf(ContentGatewayError);
    expect(error.status).toBe(502);
    expect(error.code).toBeNull();
    expect(error.message).toContain("502");
    expect(error.message).toContain("<h1>502 Bad Gateway</h1>");
  });

  it("refuses an answer the gateway did not mark", async () => {
    gateway.reply(json(200, { data: [] }));
    const error = (await client()
      .curseforge("mods/1")
      .catch((error: unknown) => error)) as ContentGatewayError;
    expect(error).toBeInstanceOf(ContentGatewayError);
    expect(error.code).toBeNull();
    expect(error.message).toContain("X-Cache");
  });
});
