# content-gateway

A small HTTP service that sits between your own services and the [CurseForge](https://docs.curseforge.com/rest-api/) and [Modrinth](https://docs.modrinth.com/api/) APIs. It holds the CurseForge API key so your services do not have to, shares one cache between all of them, counts every call made on the key, and holds every caller off when CurseForge answers 429.

> **Status:** early. Consumers, tokens, the call budget, the CurseForge passthrough with its cache, the OpenAPI document and the TypeScript client exist. The rest is being built in the open; the plan lives in [Createrington/app#1692](https://gitea.matejhoz.com/Createrington/app/issues/1692).

## What it will do

- **Pass CurseForge through.** `/v1/curseforge/*` mirrors `api.curseforge.com/v1/*`: same paths, same JSON, with the gateway's key.
- **Cache once for everyone.** Answers are cached per route, list requests (`/mods`, `/mods/files`, fingerprints) are cached per id, and identical requests in flight share one upstream call.
- **Count and cap calls.** Every consumer has its own token and named pools; calls are counted per pool and per day, with a ceiling against runaway loops.
- **Back off together.** One 429 from CurseForge holds every consumer off for as long as its `Retry-After` asks, and cached answers keep being served meanwhile.
- **Pass Modrinth through.** `/v1/modrinth/*` mirrors `api.modrinth.com/v2/*` the same way, with its own cache table, its own budget against Modrinth's rate limit, and one User-Agent for everyone.
- **Resolve downloads.** `POST /v1/resolve` finds a download address for CurseForge files: CurseForge's own link, else Modrinth's copy by SHA-1, else the file's built address on CurseForge's CDN when it answers with the right size.

## Running it

Requires the Rust toolchain pinned in `rust-toolchain.toml` (installed automatically by `rustup`).

```bash
CURSEFORGE_API_KEY="<your key>" CONTENT_GATEWAY_TOKEN="$(openssl rand -base64 32)" cargo run
curl http://127.0.0.1:5005/v1/healthz
```

### Configuration

All configuration comes from environment variables.

| Variable                              | Default     | Meaning                                                                                                                                                                     |
| ------------------------------------- | ----------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `CURSEFORGE_API_KEY`                  |             | Required. The [CurseForge API key](https://console.curseforge.com/) every call to CurseForge is made with                                                                   |
| `CONTENT_GATEWAY_HOST`                | `127.0.0.1` | IP address to listen on                                                                                                                                                     |
| `CONTENT_GATEWAY_PORT`                | `5005`      | Port to listen on                                                                                                                                                           |
| `CONTENT_GATEWAY_LOG`                 | `info`      | Log filter, in [`tracing-subscriber` `EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) syntax                         |
| `CONTENT_GATEWAY_TOKEN`               |             | The token of a single consumer named `default` with one pool named `default`                                                                                                |
| `CONTENT_GATEWAY_CONSUMERS`           |             | Several consumers as JSON, see below. Set this or `CONTENT_GATEWAY_TOKEN`, not both                                                                                         |
| `CONTENT_GATEWAY_BUDGET_PER_MINUTE`   | `300`       | Calls to CurseForge per minute over all consumers and pools                                                                                                                 |
| `CONTENT_GATEWAY_MODRINTH_PER_MINUTE` | `300`       | Calls to Modrinth per minute over all consumers and pools. Modrinth allows 300 per address                                                                                  |
| `CONTENT_GATEWAY_BUDGET_HISTORY_DAYS` | `90`        | UTC days of daily counts `/v1/budget` keeps, 1 to 3660. A day takes a few hundred bytes per pool                                                                            |
| `MODRINTH_USER_AGENT`                 |             | What the gateway calls itself to Modrinth, which asks for a name and a way to reach you, such as `my-app/1.0 (https://example.com)`. `content-gateway/<version>` when unset |
| `REDIS_URL`                           |             | Redis to keep the budget and the cache in across restarts, such as `redis://:password@127.0.0.1:6379/0`. Without it everything stays in memory                              |

### Consumers and pools

Every process that calls the gateway is a consumer with its own bearer token (at least 32 characters, no two alike). A consumer draws from one or more named pools, so its calls are counted per purpose:

```json
[
  { "name": "app", "token": "…", "pools": ["launcher", "app"] },
  { "name": "sandbox", "token": "…", "pools": ["jobs"] }
]
```

Names and pools are lowercase letters, digits and `-`. A request sends `Authorization: Bearer <token>`, and `X-Budget-Pool: <pool>` when its token has more than one pool. One ceiling per minute covers all pools; every pool is counted on its own.

An empty value counts as unset. An invalid value stops the gateway at startup with a message naming the variable. `.env.example` lists every variable. The gateway does not read a `.env` file by itself: export the variables, or pass the file with `docker run --env-file .env`.

Two more variables are read at build time, not at runtime. `/v1/healthz` reports them, and they are `null` when unset:

| Variable                   | Meaning                                    |
| -------------------------- | ------------------------------------------ |
| `CONTENT_GATEWAY_COMMIT`   | Commit the binary was built from           |
| `CONTENT_GATEWAY_BUILT_AT` | When the binary was built, ISO 8601 in UTC |

### Redis

Redis is optional. With it, the minute's count, CurseForge's 429 hold, the daily counts and the cached answers survive a restart of the gateway. While Redis cannot be reached, the gateway keeps counting and caching in memory and writes the daily counts once Redis answers again; a request never waits more than a second on Redis. Without Redis everything stays in memory, which is correct for a single gateway process.

Cached answers are read from memory first. Redis is asked only when memory has no entry (one `MGET` for all the ids of a batch), and new answers are written to it in the background, each until its stale time. They can take a few hundred MB, so give the gateway a Redis of its own with a memory limit and `volatile-lru`, for example `redis-server --maxmemory 1gb --maxmemory-policy volatile-lru`. That policy only evicts keys with an expiry: cached answers have one, so a full Redis drops the least used of them instead of refusing writes, while the daily counts have none and are never evicted (the gateway deletes days older than the `CONTENT_GATEWAY_BUDGET_HISTORY_DAYS` it keeps). The minute's count and the 429 hold expire too, but every call touches them, so they are the last to go. Do not use `allkeys-lru`, which can evict the daily counts, and do not share a `noeviction` Redis with other services: once it is full of answers, it refuses their writes too.

Every key starts with `content-gateway:`. TLS (`rediss://`) is not supported, keep Redis on the same machine or a private network.

### Docker

```bash
docker build -t content-gateway \
  --build-arg CONTENT_GATEWAY_COMMIT="$(git rev-parse HEAD)" \
  --build-arg CONTENT_GATEWAY_BUILT_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)" .
cp .env.example .env   # then set CURSEFORGE_API_KEY and CONTENT_GATEWAY_TOKEN (or CONTENT_GATEWAY_CONSUMERS)
docker run --rm --env-file .env -p 127.0.0.1:5005:5005 content-gateway
```

The image is a static binary on a distroless base. Inside the container the gateway listens on `0.0.0.0`; publish the port on `127.0.0.1` as above. The build keeps downloaded crates and compiled dependencies in BuildKit cache mounts, so a rebuild after an update only compiles what changed.

### As a host process

The binary has no runtime dependencies, so it can run straight from a directory under systemd. `docker build --target binary --output type=local,dest=build .` writes the static binary to `build/content-gateway` without making an image (a native `cargo build --release` does the same on a machine with the toolchain). A unit for a directory that holds the binary and a `.env` with the variables above, `/opt/content-gateway` here as an example:

```ini
[Service]
WorkingDirectory=/opt/content-gateway
ExecStart=/opt/content-gateway/content-gateway
EnvironmentFile=/opt/content-gateway/.env
Restart=always
RestartSec=5
User=content-gateway
```

This is how the gateway runs for Createrington: `.gitea/workflows/deploy.yml` builds the binary when a release merges into `main`, writes `.env` from Infisical, keeps the previous binary and `.env` as `content-gateway.previous` and `.env.previous`, touches `.restart-trigger` for a systemd path unit that restarts the service, and waits until `/v1/healthz` reports the deployed commit and build time; when it does not, the previous build is put back and restarted. The directory and the Infisical project come from the `DEPLOY_DIR` and `INFISICAL_PROJECT_ID` repository variables, the Infisical credentials from secrets.

## Endpoints

| Route                                           | Auth  | What it answers                                                                                                                                                                                                                                                                                                                                                    |
| ----------------------------------------------- | ----- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `GET /v1/healthz`                               | none  | `{ status, version, commit, builtAt }`                                                                                                                                                                                                                                                                                                                             |
| `GET /v1/openapi.json`                          | none  | The OpenAPI document of these routes, the same as `openapi.json` in this repository                                                                                                                                                                                                                                                                                |
| `GET /v1/budget?days=14`                        | token | One budget for CurseForge and one for Modrinth, under `curseforge` and `modrinth`: this minute's calls sent and withheld per pool, the 429 hold, and daily counts for the last `days` UTC days (14 when left out, more than the `historyDays` kept answers the days kept): calls made, answers given without one (`hits`), calls withheld, and 403 and 429 answers |
| `GET /v1/curseforge/*`, `POST /v1/curseforge/*` | token | What `api.curseforge.com/v1/*` answers, see below                                                                                                                                                                                                                                                                                                                  |
| `GET /v1/modrinth/*`, `POST /v1/modrinth/*`     | token | What `api.modrinth.com/v2/*` answers, see below                                                                                                                                                                                                                                                                                                                    |
| `POST /v1/resolve`                              | token | A download address per CurseForge file, see below                                                                                                                                                                                                                                                                                                                  |

`source` in the budget says where its counts come from: `redis` for the stored ones, `memory` when no Redis is configured, and `unavailable` when Redis did not answer, in which case the minute is the gateway's own count and the days show only what is not written to Redis yet.

The gateway's own answers are described by [`openapi.json`](openapi.json), which the server derives from its types (`content-gateway --openapi` prints it). Its `info.version` is the version of the API, the `1` of `/v1`; the gateway's own version is in `/v1/healthz`. The CurseForge answers are CurseForge's and are not described there. TypeScript consumers install [`@createrington/content-gateway-client`](packages/client/README.md), whose types come from the document; anyone else can generate a client from it.

### CurseForge

`/v1/curseforge/<path>` sends the request to `https://api.curseforge.com/v1/<path>` with the same query string and body and the gateway's key, and answers with CurseForge's status, content type and body as they came. Every call that reaches CurseForge counts against the budget of the request's pool. A path is segments of letters, digits, `-` and `_`; anything else answers `400 INVALID_PATH`.

These answers are kept and given again while they are fresh, marked `X-Cache: hit` (`miss` when CurseForge was asked). The order of query parameters does not matter. Requests for an answer that is being fetched wait for it and share it. A call that has started always finishes, is counted and is kept, also when the request that started it goes away.

| Route (under `/v1/curseforge`)                                          | Fresh  | Not found (404) |
| ----------------------------------------------------------------------- | ------ | --------------- |
| `GET /mods/search`                                                      | 5 min  |                 |
| `GET /categories`, `GET /minecraft/version`, `GET /minecraft/modloader` | 24 h   |                 |
| `GET /mods/{id}`, `POST /mods` (per id)                                 | 1 h    | 5 min           |
| `GET /mods/{id}/files`                                                  | 10 min |                 |
| `GET /mods/{id}/files/{fileId}`, `POST /mods/files` (per id)            | 10 min | 10 min          |
| `GET /mods/{id}/files/{fileId}/download-url`                            | 1 h    | 10 min          |
| `GET /mods/{id}/files/{fileId}/changelog`, `GET /mods/{id}/description` | 24 h   |                 |
| `POST /fingerprints/432`, `POST /fingerprints` (per fingerprint)        | 1 h    | 10 min          |

`POST /mods` with a body of only `{"modIds": [...]}` and `POST /mods/files` with only `{"fileIds": [...]}` are kept per id, and each id is shared with `GET /mods/{id}` or `GET /mods/{id}/files/{fileId}` (a file only under its own project, since CurseForge answers 404 for a file under another one). Only the ids that are not kept are asked for, 100 per call, and the budget admits all of those calls or none. The answer is `{"data": [...]}` with one row per known id in the order asked, without repeats, or a 404 without a body when CurseForge knows none of them, as it answers itself.

`POST /fingerprints/432` and `POST /fingerprints` with a body of only `{"fingerprints": [...]}` are kept per fingerprint, each route on its own: the files that matched it, as CurseForge sent them. Only fingerprints that are not kept are asked for, 100 per call. The answer is rebuilt in CurseForge's shape: `exactMatches` in the order asked, `exactFingerprints` for the ones that matched, `installedFingerprints` as asked (repeats included), empty `partialMatches` and `partialMatchFingerprints`, and `unmatchedFingerprints: null`, which is what CurseForge answers for a list of mod files. An answer from CurseForge with partial matches is not split and answers `502 UPSTREAM_UNREADABLE`.

Every other route, and every other `POST`, goes to CurseForge each time. Other statuses are never kept. `Cache-Control: no-cache` asks CurseForge again and keeps the new answer. A kept answer stays for 24 h after its fresh time and is given, marked `X-Cache: stale`, when the budget refuses the call, CurseForge holds the key off, answers 5xx or does not answer.

Without a kept answer, a 429 from CurseForge answers `503 UPSTREAM_HOLD` like any held call, a CurseForge that does not answer within 10 s `504 UPSTREAM_TIMEOUT`, and one that cannot be reached `502 UPSTREAM_UNREACHABLE`.

Errors answer `{ "code", "message" }`. A call the budget refuses answers `503` with `Retry-After`, plus `pool` and `retryAfterMs` in the body; `code` is `BUDGET_SPENT` when the minute's ceiling is reached and `UPSTREAM_HOLD` while CurseForge's 429 hold lasts. A request that needs more calls than a whole minute allows answers `413 REQUEST_TOO_LARGE` without `Retry-After`, since waiting would not help.

### Modrinth

`/v1/modrinth/<path>` sends the request to `https://api.modrinth.com/v2/<path>` with the same query string and body, and answers with Modrinth's status, content type and body as they came, exactly like the CurseForge passthrough: the same `X-Cache`, the same joining of requests in flight, the same stale answers when Modrinth fails. Modrinth needs no key, but it limits calls per address (300 a minute), so every call counts against the Modrinth budget of the request's pool (`CONTENT_GATEWAY_MODRINTH_PER_MINUTE`), and a 429 from Modrinth holds every pool off for its `X-Ratelimit-Reset` or `Retry-After`. `MODRINTH_USER_AGENT` is what Modrinth sees. Modrinth gets 10 s to answer, like CurseForge.

| Route (under `/v1/modrinth`)                                                | Fresh  | Not found (404) |
| --------------------------------------------------------------------------- | ------ | --------------- |
| `GET /search`                                                               | 5 min  |                 |
| `GET /tag/category`, `GET /tag/loader`, `GET /tag/game_version`, other tags | 24 h   |                 |
| `GET /project/{id}`, `GET /project/{id}/check`                              | 1 h    | 5 min           |
| `GET /projects`, `GET /versions` (by ids), `GET /project/{id}/version`      | 10 min |                 |
| `GET /project/{id}/dependencies`, `GET /user/{id}/projects`                 | 10 min |                 |
| `GET /project/{id}/members`, `GET /team/{id}/members`, `GET /user/{id}`     | 1 h    | 5 min           |
| `GET /version/{id}`                                                         | 10 min | 10 min          |
| `GET /version_file/{sha1}`, `POST /version_files` (per hash)                | 1 h    | 10 min          |

`GET /version_file/{sha1}` and `POST /version_files` with `{"hashes": [...], "algorithm": "sha1"}` (up to 1000 hashes) are kept per hash and share their entries: only the hashes not kept are asked for, 100 per call, and the batch answer is the versions found keyed by lowercase hash, leaving unknown hashes out as Modrinth does. A hash that is not 40 hex digits answers `400 INVALID_HASH`; a body asking for another algorithm goes to Modrinth as it came. Every other route, such as `POST /version_files/update`, is asked each time and still counted. A Modrinth path may hold the characters its slugs allow (`.`, `+`, `!`, `(`, `)`, `~`, `'`, `,`, `@`, `$`) besides letters, digits, `-` and `_`.

### Resolve

`POST /v1/resolve` with `{"fileIds": [...]}` (up to 1000) answers one entry per file id, in the order asked: `source` and `url` where to download it from, `sha1`, `fileName` and `size` as CurseForge lists them (null when CurseForge does not know the file), and `retry`. The sources are tried in order:

1. `curseforge`: the `downloadUrl` CurseForge's API gives. The files are read like `POST /v1/curseforge/mods/files`, from the cache while fresh, so these are the only calls that count against the pool of the request.
2. `modrinth`: the copy of the file Modrinth hosts, found by SHA-1 through the lookup above. Only the file whose own hash matches counts, and only an address on `cdn.modrinth.com`.
3. `curseforge-cdn`: the file's address on CurseForge's CDN built from its id and name, the way CurseForge's own links are. The address is unofficial, so it is given only when a HEAD answers 200 with the file's size; the bytes are not read, the consumer verifies the SHA-1 when it downloads. Verdicts are kept an hour, misses 10 minutes, and at most 8 addresses are checked at once within 20 seconds. `"cdn": false` in the body skips this step.

A null `source` with `retry` false means no source has the file and the consumer is left to its own fallbacks; `retry` true means a source could not be asked this time, so the miss is not worth remembering. `Cache-Control: no-cache` asks every source again. No bytes pass through the gateway. The sources are tried in a row, so a call takes 40 s at worst (10 s for CurseForge, 10 s for Modrinth, 20 s for the CDN checks): give the call at least that long.

## Keep it private

Run the gateway on loopback or a private network only, with your own CurseForge API key. Anyone who can reach it can spend your key. It is meant to sit next to the services that call it, not on the internet.

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm install        # Prettier, the pre-commit hook, the Infisical CLI and the client's dependencies
pnpm dev            # cargo run with the env injected from Infisical (maintainers)
pnpm openapi        # rewrite openapi.json and the client's generated types after a route or type changed
pnpm test           # the client's tests; pnpm typecheck and pnpm build likewise
pnpm format:check
```

`openapi.json` and `packages/client/src/generated/openapi.ts` are generated and checked in: `cargo test` fails when the document no longer matches the server, and the client's `generate:check` fails when the types no longer match the document.

### Releasing the client

The client package is versioned with [changesets](https://github.com/changesets/changesets), apart from the server. A change to `packages/client` comes with a changeset written on its branch (`pnpm changeset`); CI refuses a changed client without one. The release PR runs `pnpm version-packages`, which turns the changesets into the version bump and the package's `CHANGELOG.md` entry, and the merge into `main` publishes that version to the npm registry named in the package's `publishConfig` (`.gitea/workflows/publish-client.yml`, with the registry token in the `NPM_PUBLISH_TOKEN` secret). A version the registry already has is not published again, and a merge that still carries unconsumed changesets fails instead of publishing.

The budget's tests also run against a real Redis when `CONTENT_GATEWAY_TEST_REDIS_URL` names one, for example `docker run --rm -p 127.0.0.1:6390:6379 redis:8.10-alpine` and `CONTENT_GATEWAY_TEST_REDIS_URL=redis://127.0.0.1:6390/0 cargo test`. Each test uses its own key prefix and deletes its keys; CI runs them against a throwaway Redis.

## License

[MIT](LICENSE)
