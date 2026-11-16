---
"@createrington/content-gateway-client": patch
---

The CurseForge batch routes (`POST mods`, `POST mods/files`, `POST fingerprints/432` and `POST fingerprints`) now take at most 1000 different ids, like `version_files` and `resolve`: a larger batch answers `400 INVALID_BODY`, which the client raises as a `ContentGatewayError`. The generated types document the limit.
