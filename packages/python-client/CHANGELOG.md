# content-gateway-client

## 0.2.1

The gateway's own routes (`health`, `budget`, `resolve` and the version lists) raise `ContentGatewayError` on any answer that is not the gateway's JSON object instead of returning it typed as their result: a page from something else at the base URL, an empty success, or CurseForge's own failure that `resolve` passes on with its status. A raised error's `retry_after_ms` falls back to `Retry-After` when the body carries none, and an `X-Cache` value the gateway does not send is named as such instead of reported as a missing header.

## 0.2.0

`java_runtime(platform, major=None, component=None)` reads `GET /v1/minecraft/java`: a Java runtime Mojang ships for a platform, with the address of its manifest of files. `minecraft_version(id)` answers `javaComponent`, the runtime the version runs on. New types `JavaRuntime` and `RuntimeManifest`, new error code `UNKNOWN_RUNTIME`.

## 0.1.0

First release: the gateway's routes typed from its OpenAPI document, a sync and an async client for the CurseForge and Modrinth passthroughs with the gateway's refusals handled, `resolve`, the Minecraft and loader version lists, and `modrinth_file_url`.
