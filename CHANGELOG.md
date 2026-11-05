# Changelog

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
