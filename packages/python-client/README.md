# content-gateway-client

The Python client of [content-gateway](../../README.md): the gateway's own routes typed from its OpenAPI document, and the CurseForge and Modrinth passthroughs with the gateway's refusals handled. Sync and async, on [httpx](https://www.python-httpx.org/).

```bash
pip install content-gateway-client
```

Python 3.11 or newer.

## Usage

```python
from content_gateway import ContentGateway, ContentGatewayError

with ContentGateway("http://127.0.0.1:5005", token=TOKEN, pool="app") as gateway:
    answer = gateway.curseforge(
        "mods/search", query={"gameId": 432, "searchFilter": "jei", "classId": 6}
    )
    if not answer.ok:
        raise RuntimeError(f"CurseForge answered {answer.status}")
    for mod in answer.body["data"]:
        print(mod["id"], mod["name"])
```

`curseforge(path, ...)` sends `api.curseforge.com/v1/<path>` through the gateway and answers whatever CurseForge answered as a `PassthroughAnswer`: `status`, `ok`, `body` (parsed JSON, the text when it is not JSON, `None` when empty), `headers`, and `cache`, which is `hit`, `miss` or `stale` from the gateway's `X-Cache`. A `body` makes the request a `POST`; `fresh=True` asks CurseForge again instead of the cache; `pool` draws from another of the token's pools. `query` values of `None` are left out.

Only the gateway's own answers raise, as `ContentGatewayError` with the gateway's `code` (`BUDGET_SPENT`, `UPSTREAM_HOLD`, `UNAUTHORIZED`, ...), `status`, `pool` and `retry_after_ms`. `code` is `None` when the answer was not the gateway's JSON error at all, such as a proxy's HTML page.

`modrinth(path, ...)` sends `api.modrinth.com/v2/<path>` through the gateway the same way; the calls draw from the pool's Modrinth budget. `modrinth_version_file(sha1)` and `modrinth_version_files(hashes)` are the two SHA-1 lookups, and `modrinth_file_url(version, sha1)` picks the address of the file itself on Modrinth's CDN out of a version.

`resolve(file_ids, cdn=True, pool=None, fresh=False, wait_up_to=None, timeout=60)` reads `/v1/resolve`: a download address per CurseForge file, from CurseForge's own link, else Modrinth's copy of the file found by SHA-1, else the file's built address on CurseForge's CDN when it answers a HEAD with the file's size (`cdn=False` skips that step). A file with a `source` of `None` and `retry` true could not be fully checked this time. The consumer downloads from the address itself; no bytes pass through the gateway. The gateway tries the three sources in a row, so a call can take 40 s at worst; `resolve()` therefore waits 60 s rather than the client's timeout.

`minecraft_versions(type=None, loader=None)`, `minecraft_version(id)` and `loader_versions(loader, minecraft=None)` read `/v1/minecraft/*`: Mojang's version list (releases by default, narrowed to what a loader builds for), one version with the Java major it needs, and a loader's builds (NeoForge, Forge, Fabric) with `stable` and `recommended` flags.

`health()` reads `/v1/healthz` and `budget(days=None)` reads `/v1/budget`, one `UpstreamBudget` for CurseForge and one for Modrinth with the last `days` UTC days of counts (14 when left out, more than the gateway's `historyDays` answers the days kept).

The gateway's own answers are plain dicts typed as `TypedDict`s generated from `openapi.json` (`Health`, `BudgetSnapshot`, `Resolved`, `MinecraftVersions`, ...), so a type checker knows their keys. The passthrough bodies are CurseForge's and Modrinth's JSON as it came, typed `Any`.

### Async

`AsyncContentGateway` has the same methods as coroutines, on `httpx.AsyncClient`:

```python
from content_gateway import AsyncContentGateway

async with AsyncContentGateway("http://127.0.0.1:5005", token=TOKEN) as gateway:
    health = await gateway.health()
```

### Waiting out a refusal

A refused call answers `503` with `Retry-After`. A background job can sleep it out:

```python
answer = gateway.curseforge("mods", body={"modIds": mod_ids}, wait_up_to=300)
```

The call is sent again after each refusal that fits in what is left of `wait_up_to` seconds; one that does not fit raises at once. A request answering a user should leave it unset and pass the `retry_after_ms` on.

### Timeouts and connections

`timeout`, 15 s by default, is httpx's limit on each phase of a request (connecting, waiting for a read, a write), not on the whole call; the gateway answers in one body, so a call ends within it in practice. The gateway gives CurseForge 10 s before it answers `504` or a stale answer, so keep the client's timeout above that: a stalled call then comes back as the gateway's answer instead of an `httpx.TimeoutException` here. The client keeps connections to the gateway open between calls; pass your own `httpx.Client` or `httpx.AsyncClient` as `client` to change that or to test (an `httpx.MockTransport` makes a fake gateway).

## Development

```bash
uv sync                     # the virtual environment with the dev tools
uv run pytest
uv run mypy
uv run ruff check . && uv run ruff format --check .
uv run python scripts/generate.py          # rewrite src/content_gateway/generated.py from openapi.json
uv run python scripts/generate.py --check  # fail when it is stale
```

`src/content_gateway/generated.py` is generated from the gateway's `openapi.json` with [datamodel-code-generator](https://koxudaxi.github.io/datamodel-code-generator/) and checked in; `pnpm openapi` at the repository root regenerates it with the TypeScript types, and CI fails when it is stale. Never edit it by hand.

The version lives in `pyproject.toml` and `CHANGELOG.md` holds its entries. A release tag of the gateway publishes the package to PyPI when that version is not there yet, through PyPI's trusted publishing from the release workflow, so the PyPI project trusts `release.yml` of the GitHub repository and no token is stored anywhere.
