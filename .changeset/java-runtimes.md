---
"@createrington/content-gateway-client": minor
---

`javaRuntime(platform, { major } | { component })` reads `GET /v1/minecraft/java`: a Java runtime Mojang ships for a platform, with the address, SHA-1 and size of Mojang's manifest of its files, typed as `JavaRuntime`. `minecraftVersion(id)` answers `javaComponent`, the runtime the version runs on. New error code `UNKNOWN_RUNTIME`.
