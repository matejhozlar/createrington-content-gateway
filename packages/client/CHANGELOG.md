# @createrington/content-gateway-client

## 0.4.0

### Minor Changes

- f645c6b: Breaking changes:
  
  - `ErrorCode` is open: the codes this version knows, now also exported as `KnownErrorCode`, or any other string, since a newer gateway adds codes inside `/v1`. `ContentGatewayError.code` and `ApiError.code` follow, so an exhaustive `switch` that ends in a `never` check no longer compiles; keep a `default` branch instead.
  - `modrinthVersionFile()` and `modrinthVersionFiles()` take `PassthroughOptions`, which has no `method` or `body`; both were overridden before.
  - `cdn` of `ResolveRequest` is optional, as the gateway's document says (the generator no longer turns a field with a default into a required one).
  - The `zod` peer range is `^4.6.6` instead of `^3.25.0 || ^4.0.0`: zod 3.25 and zod 4 below 4.6.6 can exhaust memory while validating oversized constrained collections ([GHSA-mrhx-c7xc-m3w5](https://github.com/advisories/GHSA-mrhx-c7xc-m3w5)), and zod 3 has no patched release. A consumer on either moves to zod 4.6.6 or newer.
  
  Also:
  
  - `health()` and `minecraftVersion(id)` take `{ signal }` like every other method; passing the signal itself still works and is deprecated.
  - `BudgetOptions`, `Latest`, `RequestOptions`, `BudgetedOptions` and `PassthroughOptions` are exported. The option types of `budget()`, `minecraftVersions()`, `loaderVersions()`, `javaRuntime()` and `resolve()` are derived from the generated ones.
  - The path parameter of `loaderVersions()` is encoded like the others.
  - The package ships its MIT `LICENSE`, and `src` beside `dist` so its declaration and source maps resolve. A build starts from an empty `dist`.

### Patch Changes

- 49d5649: The CurseForge batch routes (`POST mods`, `POST mods/files`, `POST fingerprints/432` and `POST fingerprints`) now take at most 1000 different ids, like `version_files` and `resolve`: a larger batch answers `400 INVALID_BODY`, which the client raises as a `ContentGatewayError`. The generated types document the limit.
- 9b4d0a4: New error code `INTERNAL`: a request the gateway failed to finish, such as one whose load task died, answers `500 INTERNAL` instead of `502 UPSTREAM_UNREACHABLE`, which blamed the upstream. The client raises it as a `ContentGatewayError`.
- a52a758: New error code `UNKNOWN_ROUTE`: the gateway now answers a path none of its routes serves with its JSON error, `404 UNKNOWN_ROUTE`, instead of an empty 404.

## 0.3.1

### Patch Changes

- 9c26df3: The gateway's own routes (`health`, `budget`, `resolve` and the version lists) throw `ContentGatewayError` on any answer that is not the gateway's JSON object instead of returning it typed as their result: a page from something else at the base URL, an empty success, or CurseForge's own failure that `resolve` passes on with its status. A thrown error's `retryAfterMs` falls back to `Retry-After` when the body carries none, and an `X-Cache` value the gateway does not send is named as such instead of reported as a missing header.
- e0ea008: The generated types' docs follow the gateway's document: daily counts speak of the upstream instead of CurseForge, Modrinth paths list the slug characters they accept, and `algorithm` on `version_files` says another algorithm passes through to Modrinth.

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
