---
"@createrington/content-gateway-client": patch
---

New error code `INTERNAL`: a request the gateway failed to finish, such as one whose load task died, answers `500 INTERNAL` instead of `502 UPSTREAM_UNREACHABLE`, which blamed the upstream. The client raises it as a `ContentGatewayError`.
