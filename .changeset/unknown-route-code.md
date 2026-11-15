---
"@createrington/content-gateway-client": patch
---

New error code `UNKNOWN_ROUTE`: the gateway now answers a path none of its routes serves with its JSON error, `404 UNKNOWN_ROUTE`, instead of an empty 404.
