# content-gateway

A small HTTP service that sits between your own services and the [CurseForge](https://docs.curseforge.com/rest-api/) and [Modrinth](https://docs.modrinth.com/api/) APIs. It holds the CurseForge API key so your services do not have to, shares one cache between all of them, counts every call made on the key, and holds every caller off when CurseForge answers 429.

> **Status:** early. Consumers, tokens and the call budget exist; CurseForge is not called yet. The rest is being built in the open; the plan lives in [Createrington/app#1692](https://gitea.matejhoz.com/Createrington/app/issues/1692).

## What it will do

- **Pass CurseForge through.** `/v1/curseforge/*` mirrors `api.curseforge.com/v1/*`: same paths, same JSON, with the gateway's key.
- **Cache once for everyone.** Answers are cached per route, list requests (`/mods`, `/mods/files`, fingerprints) are cached per id, and identical requests in flight share one upstream call.
- **Count and cap calls.** Every consumer has its own token and named pools; calls are counted per pool and per day, with a ceiling against runaway loops.
- **Back off together.** One 429 from CurseForge holds every consumer off for as long as its `Retry-After` asks, and cached answers keep being served meanwhile.
- **Resolve downloads.** `POST /v1/resolve` finds a download URL for CurseForge files, falling back to Modrinth by SHA-1.

## Running it

Requires the Rust toolchain pinned in `rust-toolchain.toml` (installed automatically by `rustup`).

```bash
CONTENT_GATEWAY_TOKEN="$(openssl rand -base64 32)" cargo run
curl http://127.0.0.1:5005/v1/healthz
```

### Configuration

All configuration comes from environment variables.

| Variable                            | Default     | Meaning                                                                                                                                             |
| ----------------------------------- | ----------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `CONTENT_GATEWAY_HOST`              | `127.0.0.1` | IP address to listen on                                                                                                                             |
| `CONTENT_GATEWAY_PORT`              | `5005`      | Port to listen on                                                                                                                                   |
| `CONTENT_GATEWAY_LOG`               | `info`      | Log filter, in [`tracing-subscriber` `EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) syntax |
| `CONTENT_GATEWAY_TOKEN`             |             | The token of a single consumer named `default` with one pool named `default`                                                                        |
| `CONTENT_GATEWAY_CONSUMERS`         |             | Several consumers as JSON, see below. Set this or `CONTENT_GATEWAY_TOKEN`, not both                                                                 |
| `CONTENT_GATEWAY_BUDGET_PER_MINUTE` | `300`       | Calls to CurseForge per minute over all consumers and pools                                                                                         |

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

### Docker

```bash
docker build -t content-gateway \
  --build-arg CONTENT_GATEWAY_COMMIT="$(git rev-parse HEAD)" \
  --build-arg CONTENT_GATEWAY_BUILT_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)" .
cp .env.example .env   # then set CONTENT_GATEWAY_TOKEN, or CONTENT_GATEWAY_CONSUMERS
docker run --rm --env-file .env -p 127.0.0.1:5005:5005 content-gateway
```

The image is a static binary on a distroless base. Inside the container the gateway listens on `0.0.0.0`; publish the port on `127.0.0.1` as above. The build keeps downloaded crates and compiled dependencies in BuildKit cache mounts, so a rebuild after an update only compiles what changed.

## Endpoints

| Route             | Auth  | What it answers                                                                                                                                        |
| ----------------- | ----- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `GET /v1/healthz` | none  | `{ status, version, commit, builtAt }`                                                                                                                 |
| `GET /v1/budget`  | token | This minute's calls sent and withheld per pool, the 429 hold, and daily counts for the last 14 UTC days. Every count is a count of calls to CurseForge |

Errors answer `{ "code", "message" }`. A call the budget refuses answers `503` with `Retry-After`, plus `pool` and `retryAfterMs` in the body; `code` is `BUDGET_SPENT` when the minute's ceiling is reached and `UPSTREAM_HOLD` while CurseForge's 429 hold lasts. A request that needs more calls than a whole minute allows answers `413 REQUEST_TOO_LARGE` without `Retry-After`, since waiting would not help.

## Keep it private

Run the gateway on loopback or a private network only, with your own CurseForge API key. Anyone who can reach it can spend your key. It is meant to sit next to the services that call it, not on the internet.

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm install        # Prettier, the pre-commit hook and the Infisical CLI
pnpm dev            # cargo run with the env injected from Infisical (maintainers)
pnpm format:check
```

## License

[MIT](LICENSE)
