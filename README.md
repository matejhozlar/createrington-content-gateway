# content-gateway

A small HTTP service that sits between your own services and the [CurseForge](https://docs.curseforge.com/rest-api/) and [Modrinth](https://docs.modrinth.com/api/) APIs. It holds the CurseForge API key so your services do not have to, shares one cache between all of them, counts every call made on the key, and holds every caller off when CurseForge answers 429.

> **Status:** early. Only the health endpoint exists so far. The rest is being built in the open; the plan lives in [Createrington/app#1692](https://gitea.matejhoz.com/Createrington/app/issues/1692).

## What it will do

- **Pass CurseForge through.** `/v1/curseforge/*` mirrors `api.curseforge.com/v1/*`: same paths, same JSON, with the gateway's key.
- **Cache once for everyone.** Answers are cached per route, list requests (`/mods`, `/mods/files`, fingerprints) are cached per id, and identical requests in flight share one upstream call.
- **Count and cap calls.** Every consumer has its own token and named pools; calls are counted per pool and per day, with a ceiling against runaway loops.
- **Back off together.** One 429 from CurseForge holds every consumer off for as long as its `Retry-After` asks, and cached answers keep being served meanwhile.
- **Resolve downloads.** `POST /v1/resolve` finds a download URL for CurseForge files, falling back to Modrinth by SHA-1.

## Running it

Requires the Rust toolchain pinned in `rust-toolchain.toml` (installed automatically by `rustup`).

```bash
cargo run
curl http://127.0.0.1:5005/v1/healthz
```

### Configuration

All configuration comes from environment variables.

| Variable               | Default     | Meaning                                                                                                                                             |
| ---------------------- | ----------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `CONTENT_GATEWAY_HOST` | `127.0.0.1` | IP address to listen on                                                                                                                             |
| `CONTENT_GATEWAY_PORT` | `5005`      | Port to listen on                                                                                                                                   |
| `CONTENT_GATEWAY_LOG`  | `info`      | Log filter, in [`tracing-subscriber` `EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) syntax |

### Docker

```bash
docker build -t content-gateway .
docker run --rm -p 127.0.0.1:5005:5005 content-gateway
```

The image is a static binary on a distroless base. Inside the container the gateway listens on `0.0.0.0`; publish the port on `127.0.0.1` as above.

## Keep it private

Run the gateway on loopback or a private network only, with your own CurseForge API key. Anyone who can reach it can spend your key. It is meant to sit next to the services that call it, not on the internet.

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm install        # Prettier and the pre-commit hook
pnpm format:check
```

## License

[MIT](LICENSE)
