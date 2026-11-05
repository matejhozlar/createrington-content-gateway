---
"@createrington/content-gateway-client": minor
---

`minecraftVersions({ type, loader })`, `minecraftVersion(id)` and `loaderVersions(loader, { minecraft })` read the gateway's Minecraft and mod loader version lists under `/v1/minecraft/*`, typed as `MinecraftVersions`, `MinecraftVersionDetails` and `LoaderVersions`. New error codes `INVALID_QUERY`, `UNKNOWN_VERSION` and `UNKNOWN_LOADER`.
