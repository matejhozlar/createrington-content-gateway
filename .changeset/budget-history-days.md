---
"@createrington/content-gateway-client": minor
---

`budget()` takes `{ days, signal }` and asks `/v1/budget?days=` for up to the gateway's `historyDays` of daily counts, which the snapshot now reports; `INVALID_DAYS` is a new error code. The bare `budget(signal)` form is gone.
