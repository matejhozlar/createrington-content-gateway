---
"@createrington/content-gateway-client": minor
---

Breaking changes, all in the types:

- `ErrorCode` is open: the codes this version knows, now also exported as `KnownErrorCode`, or any other string, since a newer gateway adds codes inside `/v1`. `ContentGatewayError.code` and `ApiError.code` follow, so an exhaustive `switch` that ends in a `never` check no longer compiles; keep a `default` branch instead.
- `modrinthVersionFile()` and `modrinthVersionFiles()` take `PassthroughOptions`, which has no `method` or `body`; both were overridden before.
- `cdn` of `ResolveRequest` is optional, as the gateway's document says (the generator no longer turns a field with a default into a required one).

Also:

- `health()` and `minecraftVersion(id)` take `{ signal }` like every other method; passing the signal itself still works and is deprecated.
- `BudgetOptions`, `Latest`, `RequestOptions`, `BudgetedOptions` and `PassthroughOptions` are exported. The option types of `budget()`, `minecraftVersions()`, `loaderVersions()`, `javaRuntime()` and `resolve()` are derived from the generated ones.
- The path parameter of `loaderVersions()` is encoded like the others.
- The package ships its MIT `LICENSE`, and `src` beside `dist` so its declaration and source maps resolve. A build starts from an empty `dist`.
- The schemas are tested against zod 3.25 as well as zod 4.
