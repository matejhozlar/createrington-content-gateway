# @createrington/content-gateway-client

## 0.3.0

### Minor Changes

- ba90268: `javaRuntime(platform, { major } | { component })` reads `GET /v1/minecraft/java`: a Java runtime Mojang ships for a platform, with the address, SHA-1 and size of Mojang's manifest of its files, typed as `JavaRuntime`. `minecraftVersion(id)` answers `javaComponent`, the runtime the version runs on. New error code `UNKNOWN_RUNTIME`.

## 0.2.1

### Patch Changes

- 8edffb7: Name the public repository in `package.json`.

## 0.2.0

### Minor Changes

- ab889dd: `budget()` takes `{ days, signal }` and asks `/v1/budget?days=` for up to the gateway's `historyDays` of daily counts, which the snapshot now reports; `INVALID_DAYS` is a new error code. The bare `budget(signal)` form is gone.
- 33e193f: `minecraftVersions({ type, loader })`, `minecraftVersion(id)` and `loaderVersions(loader, { minecraft })` read the gateway's Minecraft and mod loader version lists under `/v1/minecraft/*`, typed as `MinecraftVersions`, `MinecraftVersionDetails` and `LoaderVersions`. New error codes `INVALID_QUERY`, `UNKNOWN_VERSION` and `UNKNOWN_LOADER`.
- 190c230: `modrinth(path, request)` sends any Modrinth call through the gateway like `curseforge()`, with `modrinthVersionFile(sha1)` and `modrinthVersionFiles(hashes)` for the SHA-1 lookups and loose schemas (`modrinthProjectSchema`, `modrinthSearchSchema`, `modrinthVersionSchema`, `modrinthVersionFilesSchema`, `modrinthTagSchema`, `modrinthGameVersionSchema`) plus `modrinthFileUrl` to read the answers; `resolve(fileIds, options)` reads `POST /v1/resolve`, typed as `Resolved`. `budget()` now answers one `UpstreamBudget` for CurseForge and one for Modrinth under `curseforge` and `modrinth`, which replaces the flat shape of 0.1.0. `CurseForgeAnswer` is now an alias of `PassthroughAnswer`. New error codes `INVALID_HASH` and `INVALID_BODY`.

## 0.1.0

### Minor Changes

- 270e5f1: First release: the gateway's routes typed from its OpenAPI document, a fetch client for the CurseForge passthrough with the gateway's refusals handled, and loose zod schemas for the CurseForge answers consumers read.
