---
"@createrington/content-gateway-client": minor
---

`modrinthVersionFile(sha1)` and `modrinthVersionFiles(hashes)` send the Modrinth SHA-1 lookups through the gateway, with `modrinthVersionSchema`, `modrinthVersionFilesSchema` and `modrinthFileUrl` to read the answers; `resolve(fileIds, options)` reads `POST /v1/resolve`, typed as `Resolved`. `CurseForgeAnswer` is now an alias of `PassthroughAnswer`. New error codes `INVALID_HASH` and `INVALID_BODY`.
