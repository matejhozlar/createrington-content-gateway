from __future__ import annotations

import asyncio
import json
import re
import time
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any, Literal, cast
from urllib.parse import quote, urlencode, urlsplit

import httpx

from .generated import (
    ApiError,
    BudgetSnapshot,
    ErrorCode,
    Health,
    Loader,
    LoaderVersions,
    MinecraftVersionDetails,
    MinecraftVersions,
    Resolved,
    VersionFilter,
)

DEFAULT_TIMEOUT = 15.0
"""How long one request to the gateway may take, in seconds, above the 10 s it gives an upstream."""

RESOLVE_TIMEOUT = 60.0
"""How long `resolve()` may take unless told otherwise, above the gateway's 40 s worst case."""

MODRINTH_CDN_HOST = "cdn.modrinth.com"
"""The host every Modrinth download address is on."""

CacheStatus = Literal["hit", "miss", "stale"]
"""How the gateway answered a passthrough call, from `X-Cache`."""

Query = Mapping[str, str | int | float | bool | None]
"""Query parameters; `None` values are left out and booleans become `true` or `false`."""

Method = Literal["GET", "POST"]

_CACHE_HEADER = "x-cache"
_POOL_HEADER = "x-budget-pool"
_CACHE_STATUSES: tuple[CacheStatus, ...] = ("hit", "miss", "stale")
_JSON_TYPE = re.compile(r"\bjson\b", re.IGNORECASE)
_SECONDS = re.compile(r"^\d+$")


@dataclass(frozen=True, slots=True)
class PassthroughAnswer:
    """What an upstream (CurseForge or Modrinth) answered through the gateway."""

    status: int
    ok: bool
    cache: CacheStatus
    body: Any
    headers: httpx.Headers


class ContentGatewayError(Exception):
    """An answer the gateway gave itself instead of passing one on; `code` is `None` when it was not the gateway's JSON error."""

    status: int
    code: ErrorCode | None
    pool: str | None
    retry_after_ms: int | None

    def __init__(self, status: int, error: ApiError | None, fallback: str) -> None:
        super().__init__(error["message"] if error is not None else fallback)
        self.status = status
        self.code = error["code"] if error is not None else None
        self.pool = error.get("pool") if error is not None else None
        self.retry_after_ms = error.get("retryAfterMs") if error is not None else None


@dataclass(frozen=True, slots=True)
class _Request:
    method: Method
    path: str
    query: Query | None = None
    body: Any = None
    authenticated: bool = True
    pool: str | None = None
    fresh: bool = False
    wait_up_to: float | None = None
    timeout: float | None = None


_Outcome = tuple[Literal["done"], Any] | tuple[Literal["wait"], float]


class _Gateway:
    def __init__(self, base_url: str, token: str, pool: str | None, timeout: float) -> None:
        self._base_url = base_url.rstrip("/")
        self._token = token
        self._pool = pool
        self._timeout = timeout

    def _build(self, request: _Request) -> tuple[str, dict[str, str], bytes | None]:
        url = self._base_url + request.path + _query_string(request.query)
        headers = {"accept": "application/json"}
        if request.authenticated:
            headers["authorization"] = f"Bearer {self._token}"
            pool = request.pool if request.pool is not None else self._pool
            if pool is not None:
                headers[_POOL_HEADER] = pool
        if request.fresh:
            headers["cache-control"] = "no-cache"
        content: bytes | None = None
        if request.body is not None:
            headers["content-type"] = "application/json"
            content = json.dumps(request.body).encode()
        return url, headers, content

    def _settle(self, request: _Request, response: httpx.Response, deadline: float) -> _Outcome:
        body = _read_body(response)
        if _CACHE_HEADER in response.headers or response.is_success:
            return ("done", body)
        error = _as_api_error(body)
        wait = _retry_after(response, error)
        if response.status_code == 503 and wait is not None and time.monotonic() + wait <= deadline:
            return ("wait", wait)
        fallback = f"the gateway answered {response.status_code} to {request.method} {request.path}"
        if isinstance(body, str) and body != "":
            fallback += f": {body}"
        raise ContentGatewayError(response.status_code, error, fallback)

    @staticmethod
    def _passthrough_answer(
        request: _Request, response: httpx.Response, body: Any
    ) -> PassthroughAnswer:
        cache = response.headers.get(_CACHE_HEADER)
        if cache not in _CACHE_STATUSES:
            raise ContentGatewayError(
                response.status_code,
                None,
                f"the answer to {request.method} {request.path} did not come through the gateway: "
                "no X-Cache header",
            )
        return PassthroughAnswer(
            status=response.status_code,
            ok=response.is_success,
            cache=cast(CacheStatus, cache),
            body=body,
            headers=response.headers,
        )

    @staticmethod
    def _passthrough_request(
        upstream: str,
        path: str,
        *,
        query: Query | None,
        body: Any,
        method: Method | None,
        pool: str | None,
        fresh: bool,
        wait_up_to: float | None,
    ) -> _Request:
        chosen: Method = method if method is not None else ("GET" if body is None else "POST")
        if chosen == "GET" and body is not None:
            raise TypeError(
                f"a GET to {path} cannot carry a body; leave method unset or make it POST"
            )
        return _Request(
            method=chosen,
            path=f"/v1/{upstream}/{path.lstrip('/')}",
            query=query,
            body=body,
            pool=pool,
            fresh=fresh,
            wait_up_to=wait_up_to,
        )

    @staticmethod
    def _budget_request(days: int | None) -> _Request:
        return _Request("GET", "/v1/budget", query=None if days is None else {"days": days})

    @staticmethod
    def _resolve_request(
        file_ids: Sequence[int],
        *,
        cdn: bool | None,
        pool: str | None,
        fresh: bool,
        wait_up_to: float | None,
        timeout: float,
    ) -> _Request:
        body: dict[str, Any] = {"fileIds": list(file_ids)}
        if cdn is not None:
            body["cdn"] = cdn
        return _Request(
            "POST",
            "/v1/resolve",
            body=body,
            pool=pool,
            fresh=fresh,
            wait_up_to=wait_up_to,
            timeout=timeout,
        )

    @staticmethod
    def _minecraft_versions_request(type: VersionFilter | None, loader: Loader | None) -> _Request:
        return _Request("GET", "/v1/minecraft/versions", query={"type": type, "loader": loader})

    @staticmethod
    def _minecraft_version_request(id: str) -> _Request:
        return _Request("GET", f"/v1/minecraft/versions/{quote(id, safe='')}")

    @staticmethod
    def _loader_versions_request(loader: Loader, minecraft: str | None) -> _Request:
        return _Request(
            "GET", f"/v1/minecraft/loaders/{loader}/versions", query={"minecraft": minecraft}
        )


class ContentGateway(_Gateway):
    """A consumer's connection to one gateway."""

    def __init__(
        self,
        base_url: str,
        *,
        token: str,
        pool: str | None = None,
        timeout: float = DEFAULT_TIMEOUT,
        client: httpx.Client | None = None,
    ) -> None:
        super().__init__(base_url, token, pool, timeout)
        self._client = client if client is not None else httpx.Client()
        self._owns_client = client is None

    def close(self) -> None:
        """Closes the connections, unless the `httpx.Client` was passed in."""
        if self._owns_client:
            self._client.close()

    def __enter__(self) -> ContentGateway:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def health(self) -> Health:
        """`GET /v1/healthz`, which needs no token."""
        return cast(Health, self._send(_Request("GET", "/v1/healthz", authenticated=False))[1])

    def budget(self, *, days: int | None = None) -> BudgetSnapshot:
        """`GET /v1/budget`: this minute's calls, the 429 hold and the daily counts of the last `days` UTC days."""
        return cast(BudgetSnapshot, self._send(self._budget_request(days))[1])

    def curseforge(
        self,
        path: str,
        *,
        query: Query | None = None,
        body: Any = None,
        method: Method | None = None,
        pool: str | None = None,
        fresh: bool = False,
        wait_up_to: float | None = None,
    ) -> PassthroughAnswer:
        """Sends `api.curseforge.com/v1/<path>` (such as `mods/search`) through the gateway; CurseForge's answer comes back whatever its status, and only the gateway's own answers raise `ContentGatewayError`."""
        return self._passthrough(
            self._passthrough_request(
                "curseforge",
                path,
                query=query,
                body=body,
                method=method,
                pool=pool,
                fresh=fresh,
                wait_up_to=wait_up_to,
            )
        )

    def modrinth(
        self,
        path: str,
        *,
        query: Query | None = None,
        body: Any = None,
        method: Method | None = None,
        pool: str | None = None,
        fresh: bool = False,
        wait_up_to: float | None = None,
    ) -> PassthroughAnswer:
        """Sends `api.modrinth.com/v2/<path>` (such as `project/sodium`) through the gateway; Modrinth's answer comes back whatever its status, and only the gateway's own answers raise `ContentGatewayError`."""
        return self._passthrough(
            self._passthrough_request(
                "modrinth",
                path,
                query=query,
                body=body,
                method=method,
                pool=pool,
                fresh=fresh,
                wait_up_to=wait_up_to,
            )
        )

    def modrinth_version_file(
        self,
        sha1: str,
        *,
        pool: str | None = None,
        fresh: bool = False,
        wait_up_to: float | None = None,
    ) -> PassthroughAnswer:
        """`GET /v1/modrinth/version_file/<sha1>`: the version Modrinth lists the file under, or a 404."""
        return self.modrinth(
            f"version_file/{quote(sha1, safe='')}", pool=pool, fresh=fresh, wait_up_to=wait_up_to
        )

    def modrinth_version_files(
        self,
        hashes: Sequence[str],
        *,
        pool: str | None = None,
        fresh: bool = False,
        wait_up_to: float | None = None,
    ) -> PassthroughAnswer:
        """`POST /v1/modrinth/version_files`: the versions Modrinth lists these SHA-1s under, keyed by lowercase hash."""
        return self.modrinth(
            "version_files",
            body={"hashes": list(hashes), "algorithm": "sha1"},
            pool=pool,
            fresh=fresh,
            wait_up_to=wait_up_to,
        )

    def resolve(
        self,
        file_ids: Sequence[int],
        *,
        cdn: bool | None = None,
        pool: str | None = None,
        fresh: bool = False,
        wait_up_to: float | None = None,
        timeout: float = RESOLVE_TIMEOUT,
    ) -> Resolved:
        """`POST /v1/resolve`: a download address per CurseForge file id."""
        request = self._resolve_request(
            file_ids, cdn=cdn, pool=pool, fresh=fresh, wait_up_to=wait_up_to, timeout=timeout
        )
        return cast(Resolved, self._send(request)[1])

    def minecraft_versions(
        self, *, type: VersionFilter | None = None, loader: Loader | None = None
    ) -> MinecraftVersions:
        """`GET /v1/minecraft/versions`: the Minecraft versions Mojang lists, newest first."""
        return cast(
            MinecraftVersions, self._send(self._minecraft_versions_request(type, loader))[1]
        )

    def minecraft_version(self, id: str) -> MinecraftVersionDetails:
        """`GET /v1/minecraft/versions/<id>`: one version with the Java major it needs; `UNKNOWN_VERSION` raises."""
        return cast(MinecraftVersionDetails, self._send(self._minecraft_version_request(id))[1])

    def loader_versions(self, loader: Loader, *, minecraft: str | None = None) -> LoaderVersions:
        """`GET /v1/minecraft/loaders/<loader>/versions`: a loader's builds, newest first."""
        return cast(LoaderVersions, self._send(self._loader_versions_request(loader, minecraft))[1])

    def _passthrough(self, request: _Request) -> PassthroughAnswer:
        response, body = self._send(request)
        return self._passthrough_answer(request, response, body)

    def _send(self, request: _Request) -> tuple[httpx.Response, Any]:
        url, headers, content = self._build(request)
        deadline = time.monotonic() + (request.wait_up_to or 0)
        timeout = request.timeout if request.timeout is not None else self._timeout
        while True:
            response = self._client.request(
                request.method, url, headers=headers, content=content, timeout=timeout
            )
            outcome = self._settle(request, response, deadline)
            if outcome[0] == "done":
                return response, outcome[1]
            time.sleep(outcome[1])


class AsyncContentGateway(_Gateway):
    """A consumer's connection to one gateway, for asyncio."""

    def __init__(
        self,
        base_url: str,
        *,
        token: str,
        pool: str | None = None,
        timeout: float = DEFAULT_TIMEOUT,
        client: httpx.AsyncClient | None = None,
    ) -> None:
        super().__init__(base_url, token, pool, timeout)
        self._client = client if client is not None else httpx.AsyncClient()
        self._owns_client = client is None

    async def aclose(self) -> None:
        """Closes the connections, unless the `httpx.AsyncClient` was passed in."""
        if self._owns_client:
            await self._client.aclose()

    async def __aenter__(self) -> AsyncContentGateway:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.aclose()

    async def health(self) -> Health:
        """`GET /v1/healthz`, which needs no token."""
        sent = await self._send(_Request("GET", "/v1/healthz", authenticated=False))
        return cast(Health, sent[1])

    async def budget(self, *, days: int | None = None) -> BudgetSnapshot:
        """`GET /v1/budget`: this minute's calls, the 429 hold and the daily counts of the last `days` UTC days."""
        sent = await self._send(self._budget_request(days))
        return cast(BudgetSnapshot, sent[1])

    async def curseforge(
        self,
        path: str,
        *,
        query: Query | None = None,
        body: Any = None,
        method: Method | None = None,
        pool: str | None = None,
        fresh: bool = False,
        wait_up_to: float | None = None,
    ) -> PassthroughAnswer:
        """Sends `api.curseforge.com/v1/<path>` (such as `mods/search`) through the gateway; CurseForge's answer comes back whatever its status, and only the gateway's own answers raise `ContentGatewayError`."""
        return await self._passthrough(
            self._passthrough_request(
                "curseforge",
                path,
                query=query,
                body=body,
                method=method,
                pool=pool,
                fresh=fresh,
                wait_up_to=wait_up_to,
            )
        )

    async def modrinth(
        self,
        path: str,
        *,
        query: Query | None = None,
        body: Any = None,
        method: Method | None = None,
        pool: str | None = None,
        fresh: bool = False,
        wait_up_to: float | None = None,
    ) -> PassthroughAnswer:
        """Sends `api.modrinth.com/v2/<path>` (such as `project/sodium`) through the gateway; Modrinth's answer comes back whatever its status, and only the gateway's own answers raise `ContentGatewayError`."""
        return await self._passthrough(
            self._passthrough_request(
                "modrinth",
                path,
                query=query,
                body=body,
                method=method,
                pool=pool,
                fresh=fresh,
                wait_up_to=wait_up_to,
            )
        )

    async def modrinth_version_file(
        self,
        sha1: str,
        *,
        pool: str | None = None,
        fresh: bool = False,
        wait_up_to: float | None = None,
    ) -> PassthroughAnswer:
        """`GET /v1/modrinth/version_file/<sha1>`: the version Modrinth lists the file under, or a 404."""
        return await self.modrinth(
            f"version_file/{quote(sha1, safe='')}", pool=pool, fresh=fresh, wait_up_to=wait_up_to
        )

    async def modrinth_version_files(
        self,
        hashes: Sequence[str],
        *,
        pool: str | None = None,
        fresh: bool = False,
        wait_up_to: float | None = None,
    ) -> PassthroughAnswer:
        """`POST /v1/modrinth/version_files`: the versions Modrinth lists these SHA-1s under, keyed by lowercase hash."""
        return await self.modrinth(
            "version_files",
            body={"hashes": list(hashes), "algorithm": "sha1"},
            pool=pool,
            fresh=fresh,
            wait_up_to=wait_up_to,
        )

    async def resolve(
        self,
        file_ids: Sequence[int],
        *,
        cdn: bool | None = None,
        pool: str | None = None,
        fresh: bool = False,
        wait_up_to: float | None = None,
        timeout: float = RESOLVE_TIMEOUT,
    ) -> Resolved:
        """`POST /v1/resolve`: a download address per CurseForge file id."""
        request = self._resolve_request(
            file_ids, cdn=cdn, pool=pool, fresh=fresh, wait_up_to=wait_up_to, timeout=timeout
        )
        sent = await self._send(request)
        return cast(Resolved, sent[1])

    async def minecraft_versions(
        self, *, type: VersionFilter | None = None, loader: Loader | None = None
    ) -> MinecraftVersions:
        """`GET /v1/minecraft/versions`: the Minecraft versions Mojang lists, newest first."""
        sent = await self._send(self._minecraft_versions_request(type, loader))
        return cast(MinecraftVersions, sent[1])

    async def minecraft_version(self, id: str) -> MinecraftVersionDetails:
        """`GET /v1/minecraft/versions/<id>`: one version with the Java major it needs; `UNKNOWN_VERSION` raises."""
        sent = await self._send(self._minecraft_version_request(id))
        return cast(MinecraftVersionDetails, sent[1])

    async def loader_versions(
        self, loader: Loader, *, minecraft: str | None = None
    ) -> LoaderVersions:
        """`GET /v1/minecraft/loaders/<loader>/versions`: a loader's builds, newest first."""
        sent = await self._send(self._loader_versions_request(loader, minecraft))
        return cast(LoaderVersions, sent[1])

    async def _passthrough(self, request: _Request) -> PassthroughAnswer:
        response, body = await self._send(request)
        return self._passthrough_answer(request, response, body)

    async def _send(self, request: _Request) -> tuple[httpx.Response, Any]:
        url, headers, content = self._build(request)
        deadline = time.monotonic() + (request.wait_up_to or 0)
        timeout = request.timeout if request.timeout is not None else self._timeout
        while True:
            response = await self._client.request(
                request.method, url, headers=headers, content=content, timeout=timeout
            )
            outcome = self._settle(request, response, deadline)
            if outcome[0] == "done":
                return response, outcome[1]
            await asyncio.sleep(outcome[1])


def modrinth_file_url(version: Mapping[str, Any], sha1: str) -> str | None:
    """The `https://cdn.modrinth.com` address of the file with this SHA-1 in a Modrinth version, or `None`; other files of the version and other hosts never count."""
    wanted = sha1.lower()
    for file in version.get("files") or ():
        hashes = file.get("hashes") or {}
        found = hashes.get("sha1")
        if not isinstance(found, str) or found.lower() != wanted:
            continue
        url = file.get("url")
        if not isinstance(url, str):
            continue
        parts = urlsplit(url)
        if parts.scheme == "https" and parts.hostname == MODRINTH_CDN_HOST:
            return url
    return None


def _query_string(query: Query | None) -> str:
    if query is None:
        return ""
    pairs = [(name, _query_value(value)) for name, value in query.items() if value is not None]
    return f"?{urlencode(pairs)}" if pairs else ""


def _query_value(value: str | int | float | bool) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def _read_body(response: httpx.Response) -> Any:
    text = response.text
    if text == "":
        return None
    if not _JSON_TYPE.search(response.headers.get("content-type", "")):
        return text
    try:
        return json.loads(text)
    except ValueError:
        return text


def _as_api_error(body: Any) -> ApiError | None:
    if not isinstance(body, dict):
        return None
    if not isinstance(body.get("code"), str) or not isinstance(body.get("message"), str):
        return None
    return cast(ApiError, body)


def _retry_after(response: httpx.Response, error: ApiError | None) -> float | None:
    if error is not None:
        ms = error.get("retryAfterMs")
        if isinstance(ms, int):
            return ms / 1000
    header = response.headers.get("retry-after", "").strip()
    if _SECONDS.match(header):
        return float(header)
    return None
