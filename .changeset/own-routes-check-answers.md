---
"@createrington/content-gateway-client": patch
---

The gateway's own routes (`health`, `budget`, `resolve` and the version lists) throw `ContentGatewayError` on any answer that is not the gateway's JSON object instead of returning it typed as their result: a page from something else at the base URL, an empty success, or CurseForge's own failure that `resolve` passes on with its status. A thrown error's `retryAfterMs` falls back to `Retry-After` when the body carries none, and an `X-Cache` value the gateway does not send is named as such instead of reported as a missing header.
