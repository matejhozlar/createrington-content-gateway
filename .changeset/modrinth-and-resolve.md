---
"@createrington/content-gateway-client": minor
---

`modrinth(path, request)` sends any Modrinth call through the gateway like `curseforge()`, with `modrinthVersionFile(sha1)` and `modrinthVersionFiles(hashes)` for the SHA-1 lookups and loose schemas (`modrinthProjectSchema`, `modrinthSearchSchema`, `modrinthVersionSchema`, `modrinthVersionFilesSchema`, `modrinthTagSchema`, `modrinthGameVersionSchema`) plus `modrinthFileUrl` to read the answers; `resolve(fileIds, options)` reads `POST /v1/resolve`, typed as `Resolved`. `budget()` now answers one `UpstreamBudget` for CurseForge and one for Modrinth under `curseforge` and `modrinth`, which replaces the flat shape of 0.1.0. `CurseForgeAnswer` is now an alias of `PassthroughAnswer`. New error codes `INVALID_HASH` and `INVALID_BODY`.
