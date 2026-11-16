# Changelog

## v0.1.8 (2026-11-16)

- [add] Answer a path none of the gateway's routes serves with its JSON error, `404 UNKNOWN_ROUTE`, instead of an empty 404
- [add] Log the events the gateway exists for as warn lines with their values as fields (`upstream`, `pool`, `wait_ms`, `limit`, `bytes`, `status`, `url`, `key`, `error`): a 429 starting a hold, a budget spent for the minute (once per minute), an answer over the size limit, a batch answer that cannot be split per id, a load that ended without an answer and a Redis cache entry that does not decode. Every existing log line follows the same shape, and a pool is written as `consumer/pool`
- [fix] Answer a request whose load task died `500 INTERNAL`, documented on every route that loads, instead of `502 UPSTREAM_UNREACHABLE`, which blamed the upstream; a kept answer still stands in for it and the cause is logged once
- [fix] Keep a Minecraft or loader version list only once it reads: a `200` that did not read was cached and answered `502` for its whole fresh time and then stood in as the stale copy. A kept list that does not read, such as one an older gateway left in Redis, now counts as no copy
- [fix] Read the loader in any case in `loader=` of `GET /v1/minecraft/versions` as in the path, and answer `400 INVALID_QUERY` on `GET /v1/minecraft/loaders/{loader}/versions` for a query it cannot read (such as `minecraft` given twice) instead of ignoring it
- [fix] Count a stale answer as a cache hit only when no call was made for it: a single CurseForge route counted a hit as well after its own failed call, which was already counted as that call
- [fix] Keep a Redis outage inside a minute from admitting up to twice the minute's ceiling: the gateway counts every call it let through or refused, and the Redis budget takes that count, in total and per pool, as the least the minute has used
- [fix] Keep a count that arrives after its minute ended from resetting the budget's current minute
- [fix] Tell the request that met a CurseForge 429 the same wait as the requests after it, and report the budget's `source` as `unavailable` when either upstream's budget could not read Redis
- [fix] Name a non-ASCII `X-Budget-Pool` in the `UNKNOWN_POOL` refusal instead of an empty pool name
- [fix] Keep a CDN check of `POST /v1/resolve` running after the request that started it stops waiting, so its verdict is kept for the next request, and let concurrent requests for the same file share one HEAD instead of each sending its own
- [fix] Trim every environment variable of surrounding whitespace and start every configuration error with the variable it is about: `CONTENT_GATEWAY_TOKEN` with surrounding whitespace no longer passes startup and then fails every request, a short `CONTENT_GATEWAY_TOKEN` gets its own error, and a token in `CONTENT_GATEWAY_CONSUMERS` that starts or ends with whitespace stops the gateway at startup since no request can send it
- [security] Take at most 1000 different ids in the CurseForge batch routes (`POST mods`, `POST mods/files`, `POST fingerprints/432`, `POST fingerprints`) like `version_files` and `resolve`, answering `400 INVALID_BODY` for more
- [refactor] Load single answers, batches, Modrinth hashes and version lists through one loader, split the budget, version lists and resolve into named modules and steps, and make every server module private so the compiler reports dead code
- [chore] Share one CI across Gitea and GitHub through composite actions, run it on pushes to `dev` and `main` and weekly, build the Dockerfile in it, pin every action by commit with Renovate moving the pins, install uv from its checksummed release archive, and check versions, em dashes, documented variables and commit subjects in scripts
- [chore] Run the Redis-backed tests as ignored without `CONTENT_GATEWAY_TEST_REDIS_URL` instead of letting them pass unrun, and cover the Modrinth, resolve and version list paths with tests that no longer race sleeps

## v0.1.7 (2026-11-15)

- [add] Add `CONTENT_GATEWAY_USER_AGENT`, the one User-Agent the gateway sends to every host it calls: CurseForge's API and CDN, Modrinth, Mojang, NeoForge's Maven, Forge and Fabric's meta. Before, CurseForge's API and CDN always saw `content-gateway/<version>` while the rest saw `MODRINTH_USER_AGENT`. `MODRINTH_USER_AGENT` is still read when the new variable is unset, and the default stays `content-gateway/<version>`
- [add] Add a `compose.yaml` that runs the release image next to a Redis of its own (`maxmemory 1gb`, `volatile-lru`, data in a volume, no port published), listening on `0.0.0.0` inside the container and published on `127.0.0.1:5005` only; `docker compose up -d --build` builds the checkout instead
- [fix] Name the right upstream in the gateway's refusals on Modrinth routes: a spent Modrinth budget, a Modrinth 429 hold and a request needing more Modrinth calls than a minute allows now say Modrinth instead of CurseForge, and `INVALID_PATH` on `/v1/modrinth/*` names Modrinth and lists the slug characters (`.+!()~',@$`) its paths accept
- [fix] Correct the OpenAPI document: the Modrinth path parameters list the slug characters the gateway accepts, `algorithm` on `POST /v1/modrinth/version_files` says another algorithm passes through to Modrinth uncached instead of claiming only `sha1` is supported, `POST /v1/resolve` documents that it answers with CurseForge's own status and body (marked `X-Cache`) when the file lookup fails, and the budget's daily counts speak of the upstream rather than CurseForge
- [fix] Leave `CONTENT_GATEWAY_HOST` commented out in `.env.example`, so a container started with `docker run --env-file .env` or compose keeps listening on `0.0.0.0` as the image sets it instead of on an unreachable `127.0.0.1`
- [fix] Make the example deploy workflow put the previous `.env` back when the gateway does not answer after a redeploy of the same commit, where only `.env` was replaced, and report the last health probe's error when it gives up
- [chore] Install the Infisical CLI in the example deploy workflow from the version `pnpm-lock.yaml` pins, keep the Infisical API URL to the step that logs in, write the new `.env` with owner-only permissions, and pull the health probe image once before the deploy
- [chore] Document `cargo deny`, `cargo machete`, `pnpm audit` and the Redis-backed budget tests in the README's development commands, and how to run your own copy of the CI and deploy workflows

## v0.1.6 (2026-11-06)

- [add] Add `GET /v1/minecraft/java?platform=<platform>&major=<n>` (or `&component=<name>`), serving the Java runtimes Mojang ships with its launcher for a platform (`linux`, `linux-i386`, `mac-os`, `mac-os-arm64`, `windows-x64`, `windows-x86`, `windows-arm64`): the runtime's `component`, `major`, `version` and release date, and the `url`, `sha1` and `size` of Mojang's manifest of its files. `major` picks the newest non-snapshot runtime of that Java major (the later released one on equal versions), `component` names one exactly, and `404 UNKNOWN_RUNTIME` answers when Mojang ships no such runtime for the platform. Mojang's list is cached and served stale like the other version lists, draws on no budget, and the runtime's files never pass through the gateway
- [add] Answer `javaComponent` from `GET /v1/minecraft/versions/{id}`, the name of Mojang's Java runtime the version runs on, which `GET /v1/minecraft/java` accepts as `component`

## v0.1.5 (2026-11-05)

- [fix] Make the GitHub release workflow build from the commit the tag push carries and point the GitHub release at that commit, and stop waiting for the new tag to show up in the remote's tags: the pushed tag is counted when deciding which release is the latest, and the workflow retries only when the remote's tags cannot be read at all

## v0.1.4 (2026-11-05)

- [fix] Make the GitHub release workflow build a release tag only once: a tag that already has a GitHub release is skipped when the mirror pushes it again, and running the workflow by hand still rebuilds it
- [chore] Reference the PyPI publish action in the release workflow by its `v1.14.3` release tag

## v0.1.3 (2026-11-05)

- [fix] Make the GitHub release workflow read the release tags from the remote instead of fetching them with the checkout, retrying for up to a minute until the new tag is visible and failing the release when it never is, and ignore pre-release tags when deciding which release is the latest
- [chore] Move the CI, deploy and publish workflows and the shared composite actions to the Node 24 majors of their actions (`actions/checkout@v7`, `actions/setup-node@v7`, `actions/cache@v6`, `pnpm/action-setup@v6`)
- [chore] Describe the Gitea and GitHub CI workflows in the README as equivalent, without naming either as the primary host

## v0.1.2 (2026-11-05)

- [add] Publish every release on GitHub: the `vX.Y.Z` tag builds static amd64 and arm64 binaries, `openapi.json` and a `SHA256SUMS` for the GitHub release, with notes taken from this changelog, and a multi-arch image at `ghcr.io/matejhozlar/createrington-content-gateway:<version>`, after checking that the tag matches `package.json` and `Cargo.toml`
- [add] Add a Python client, `content-gateway-client` on PyPI (Python 3.11+, sync and async on httpx), covering the CurseForge and Modrinth passthroughs with the gateway's refusals raised as `ContentGatewayError`, `resolve`, the Minecraft and loader version lists, the budget and health, with its types generated from `openapi.json` and published by the release workflow when PyPI does not have its version yet
- [chore] Prepare the repository for its public mirror: name the repository in the manifests, document every variable in `.env.example`, describe running with `cargo run`, present the Gitea deploy workflow as an example, and shorten the server's doc comments
- [chore] Run CI on GitHub pull requests as well as Gitea, sharing composite actions under `.ci/actions`, with a read-only token, and check the Python client (generated types, ruff, mypy, pytest) in both

## v0.1.1 (2026-11-05)

- [add] Add the Modrinth passthrough at `/v1/modrinth/*`, mirroring `api.modrinth.com/v2/*` like the CurseForge one (status, content type and body as they came, `X-Cache`, shared in-flight calls, stale answers when Modrinth fails, a 10 s timeout) with its own per-route cache table, per-hash caching for `GET /version_file/{sha1}` and `POST /version_files` (up to 1000 SHA-1 hashes, 100 per call, `400 INVALID_HASH` for a malformed one, other algorithms passed through as they came), and paths that accept every character Modrinth slugs allow
- [add] Add a separate Modrinth budget per pool with its own ceiling (`CONTENT_GATEWAY_MODRINTH_PER_MINUTE`, default 300), its own Redis keys, and a shared hold after Modrinth answers 429 for as long as `X-Ratelimit-Reset` or `Retry-After` asks; `MODRINTH_USER_AGENT` sets what the gateway calls itself to Modrinth (`content-gateway/<version>` when unset)
- [add] Add `POST /v1/resolve`, which answers a download address per CurseForge file id (up to 1000) from CurseForge's own link, else Modrinth's copy matched by SHA-1, else the file's CurseForge CDN address when a HEAD confirms its size (skippable with `"cdn": false`), with `sha1`, `fileName`, `size` and a `retry` flag per file; no bytes pass through the gateway
- [add] Add `GET /v1/minecraft/versions`, `GET /v1/minecraft/versions/{id}` and `GET /v1/minecraft/loaders/{loader}/versions`, serving Mojang's Minecraft versions (filterable by type and by loader support, with the Java major a version runs on) and NeoForge, Forge and Fabric builds with `stable` and `recommended` flags, from one shared cache kept an hour and served stale for a day when a source fails, drawing on no budget
- [add] Add `CONTENT_GATEWAY_BUDGET_HISTORY_DAYS` (default 90, 1 to 3660) to choose how many UTC days of daily counts are kept, deleting older days on the first trim after the value is lowered, and a `days` query on `GET /v1/budget` (default 14, `400 INVALID_DAYS` for a bad value) to choose how many are answered
- [refactor] Change the `GET /v1/budget` answer to hold one budget per upstream under `curseforge` and `modrinth`, with `source` and the new `historyDays` at the top level; readers of the previous flat shape must move to `curseforge`
- [chore] Make the example deploy workflow say so on a first deploy instead of claiming the build already runs

## v0.1.0 (2026-11-04)

- [add] Add the content-gateway service: a loopback HTTP server that holds the CurseForge API key, configured from environment variables (`CURSEFORGE_API_KEY`, `CONTENT_GATEWAY_HOST`, `CONTENT_GATEWAY_PORT`, `CONTENT_GATEWAY_LOG`), refusing an invalid value at startup with a message naming the variable and stopping within five seconds of a shutdown signal
- [add] Add `GET /v1/healthz`, which reports the commit and build time stamped into the binary at build time (`CONTENT_GATEWAY_COMMIT`, `CONTENT_GATEWAY_BUILT_AT`)
- [add] Add consumer authentication by bearer token, either one `default` consumer from `CONTENT_GATEWAY_TOKEN` or several consumers with named pools from `CONTENT_GATEWAY_CONSUMERS`, selected per request with `X-Budget-Pool`
- [add] Add a call budget on the CurseForge key: one ceiling per minute over all pools (`CONTENT_GATEWAY_BUDGET_PER_MINUTE`), counts per pool and per day, a shared hold after CurseForge answers 429, `503 BUDGET_SPENT`/`UPSTREAM_HOLD` refusals with `Retry-After`, and `413 REQUEST_TOO_LARGE` for a request that needs more calls than a minute allows
- [add] Add `GET /v1/budget`, a readout of this minute's sent and withheld calls per pool, the 429 hold, and daily counts of calls, cache hits, withheld calls and 403/429 answers for the last 14 UTC days
- [add] Add the CurseForge passthrough at `/v1/curseforge/*`, mirroring `api.curseforge.com/v1/*` with CurseForge's status, content type and body as they came, an in-memory cache with per-route fresh and stale times, shared in-flight calls, `Cache-Control: no-cache` support, stale answers (`X-Cache: stale`) when CurseForge is held, failing or slow, and a 10 s upstream timeout
- [add] Add per-id caching for `POST /mods` and `POST /mods/files`, shared with the single-item routes, so a batch only asks CurseForge for the ids not already kept, 100 per call, admitted by the budget all at once or not at all
- [add] Add per-fingerprint caching for `POST /fingerprints` and `POST /fingerprints/432`, rebuilding CurseForge's answer shape from the kept matches
- [add] Add optional Redis (`REDIS_URL`) to keep the minute's count, the 429 hold, the daily counts and cached answers across restarts, falling back to memory while Redis cannot be reached and never waiting more than a second on it
- [add] Add `GET /v1/openapi.json`, an OpenAPI document of the gateway's own routes generated from the server's types and checked in as `openapi.json`
- [chore] Add the Cargo and pnpm workspaces, CI (fmt, clippy, tests with and without Redis, cargo-deny, cargo-machete, Prettier), a distroless Docker image, and an example workflow that deploys the binary on a release merge, verifies the running build and rolls back when it does not answer
