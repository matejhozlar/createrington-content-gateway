---
"@createrington/content-gateway-client": minor
---

Breaking changes:

- `ErrorCode` is open: the codes this version knows, now also exported as `KnownErrorCode`, or any other string, since a newer gateway adds codes inside `/v1`. `ContentGatewayError.code` and `ApiError.code` follow, so an exhaustive `switch` that ends in a `never` check no longer compiles; keep a `default` branch instead.
- `modrinthVersionFile()` and `modrinthVersionFiles()` take `PassthroughOptions`, which has no `method` or `body`; both were overridden before.
- `cdn` of `ResolveRequest` is optional, as the gateway's document says (the generator no longer turns a field with a default into a required one).
- The `zod` peer range is `^4.6.6` instead of `^3.25.0 || ^4.0.0`: zod 3.25 and zod 4 below 4.6.6 can exhaust memory while validating oversized constrained collections ([GHSA-mrhx-c7xc-m3w5](https://github.com/advisories/GHSA-mrhx-c7xc-m3w5)), and zod 3 has no patched release. A consumer on either moves to zod 4.6.6 or newer.

Also:

- `health()` and `minecraftVersion(id)` take `{ signal }` like every other method; passing the signal itself still works and is deprecated.
- `BudgetOptions`, `Latest`, `RequestOptions`, `BudgetedOptions` and `PassthroughOptions` are exported. The option types of `budget()`, `minecraftVersions()`, `loaderVersions()`, `javaRuntime()` and `resolve()` are derived from the generated ones.
- The path parameter of `loaderVersions()` is encoded like the others.
- The package ships its MIT `LICENSE`, and `src` beside `dist` so its declaration and source maps resolve. A build starts from an empty `dist`.
