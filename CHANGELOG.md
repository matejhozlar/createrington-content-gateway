# Changelog

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
