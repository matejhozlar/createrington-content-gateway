# @createrington/content-gateway-client

The TypeScript client of [content-gateway](../../README.md): the gateway's own routes typed from its OpenAPI document, the CurseForge passthrough with the gateway's refusals handled, and loose zod schemas for the CurseForge answers consumers read.

```bash
pnpm add @createrington/content-gateway-client zod
```

`zod` is a peer dependency: 3.25 or newer, or 4. The schemas are built with `zod/v4`, which both ship.

## Usage

```ts
import {
  ContentGateway,
  ContentGatewayError,
  curseforgeModSchema,
  curseforgePage,
} from "@createrington/content-gateway-client";

const gateway = new ContentGateway({
  baseUrl: "http://127.0.0.1:5005",
  token: process.env.CONTENT_GATEWAY_TOKEN,
  pool: "app",
});

const answer = await gateway.curseforge("mods/search", {
  query: { gameId: 432, searchFilter: "jei", classId: 6 },
});
if (!answer.ok) throw new Error(`CurseForge answered ${answer.status}`);
const page = curseforgePage(curseforgeModSchema).parse(answer.body);
```

`curseforge(path, request)` sends `api.curseforge.com/v1/<path>` through the gateway and answers whatever CurseForge answered: `status`, `ok`, `body` (parsed JSON, the text when it is not JSON, `null` when empty), `headers`, and `cache`, which is `hit`, `miss` or `stale` from the gateway's `X-Cache`. A `body` makes the request a `POST`; `fresh: true` asks CurseForge again instead of the cache; `pool` draws from another of the token's pools; `signal` aborts.

Only the gateway's own answers throw, as `ContentGatewayError` with the gateway's `code` (`BUDGET_SPENT`, `UPSTREAM_HOLD`, `UNAUTHORIZED`, ...), `status`, `pool` and `retryAfterMs`. `code` is `null` when the answer was not the gateway's JSON error at all, such as a proxy's HTML page.

`health()` reads `/v1/healthz` and `budget()` reads `/v1/budget`, both typed from `openapi.json`. The `paths` and `components` types of the document are exported for anything else.

### Waiting out a refusal

A refused call answers `503` with `Retry-After`. A background job can sleep it out:

```ts
const answer = await gateway.curseforge("mods", {
  body: { modIds },
  waitUpToMs: 5 * 60_000,
});
```

The call is sent again after each refusal that fits in what is left of `waitUpToMs`; one that does not fit throws at once. A request answering a user should leave it unset and pass the `retryAfterMs` on.

### Timeouts and connections

Every request is abandoned after `timeoutMs`, 15 s by default. The gateway gives CurseForge 10 s before it answers `504` or a stale answer, so keep the client's timeout above that: a stalled call then comes back as the gateway's answer instead of a `TimeoutError` here. Node's `fetch` keeps connections to the gateway open between calls; pass your own `fetch` to change that or to test.

### Schemas

The `curseforge*Schema` objects describe what consumers read of CurseForge's answers, seeded from the Createrington app: project, file, category, Minecraft version, mod loader, fingerprint matches. They are loose, so a field CurseForge adds passes through and a field nobody reads is simply not listed. `curseforgeData(row)`, `curseforgeList(row)` and `curseforgePage(row)` wrap a row schema in the `{ data }`, `{ data: [] }` and `{ data: [], pagination }` shapes CurseForge answers. The constants (`CURSEFORGE_MINECRAFT_GAME_ID`, `CurseForgeModLoaderType`, `CurseForgeRelationType`, ...) are CurseForge's vocabulary.

A consumer that needs more of a file than its ids extends the file schema for the route it knows: `curseforgeFileSchema.extend({ fileName: z.string() })`.

## Development

`src/generated/openapi.ts` is generated from the gateway's `openapi.json` with [openapi-typescript](https://openapi-ts.dev/) and checked in; `pnpm openapi` at the repository root regenerates both, and CI fails when either is stale. Never edit it by hand.

A change consumers can notice comes with a changeset (`pnpm changeset` at the repository root); the release PR turns the changesets into the version and `CHANGELOG.md`, and the merge into `main` publishes. Never edit the version by hand.

```bash
pnpm --filter @createrington/content-gateway-client test
pnpm --filter @createrington/content-gateway-client typecheck
pnpm --filter @createrington/content-gateway-client build
```
