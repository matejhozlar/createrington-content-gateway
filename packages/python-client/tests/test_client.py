from __future__ import annotations

import asyncio
import copy
import inspect
import json
import pickle
import time
from collections.abc import Callable, Coroutine, Iterator
from typing import Any, cast, get_overloads

import httpx
import pytest

from content_gateway import (
    RESOLVE_TIMEOUT,
    AsyncContentGateway,
    ContentGateway,
    ContentGatewayError,
    modrinth_file_url,
)

TOKEN = "t" * 40
BASE_URL = "http://gateway.test"


class FakeGateway:
    """A stand-in for the gateway that answers the replies queued for it, in order, and records what it was sent."""

    def __init__(self) -> None:
        self.requests: list[httpx.Request] = []
        self.replies: list[httpx.Response | httpx.TransportError] = []

    def reply(
        self,
        status: int = 200,
        body: Any = None,
        *,
        headers: dict[str, str] | None = None,
        text: str | None = None,
    ) -> None:
        sent = dict(headers or {})
        if text is not None:
            self.replies.append(httpx.Response(status, text=text, headers=sent))
        elif body is None:
            self.replies.append(httpx.Response(status, headers=sent))
        else:
            self.replies.append(httpx.Response(status, json=body, headers=sent))

    def fail(self, error: httpx.TransportError) -> None:
        self.replies.append(error)

    def handle(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        if not self.replies:
            raise AssertionError(f"no reply queued for {request.method} {request.url}")
        reply = self.replies.pop(0)
        if isinstance(reply, httpx.TransportError):
            raise reply
        return reply

    def client(self) -> httpx.Client:
        return httpx.Client(transport=httpx.MockTransport(self.handle))

    def async_client(self) -> httpx.AsyncClient:
        return httpx.AsyncClient(transport=httpx.MockTransport(self.handle))

    @property
    def last(self) -> httpx.Request:
        return self.requests[-1]


class Blocking:
    """Drives an `AsyncContentGateway` through the sync class's interface, awaiting each call on one event loop, so every test runs against both classes."""

    def __init__(self, gateway: AsyncContentGateway, runner: asyncio.Runner) -> None:
        self._gateway = gateway
        self._runner = runner

    def __getattr__(self, name: str) -> Any:
        found = getattr(self._gateway, "aclose" if name == "close" else name)
        if not callable(found):
            return found

        def call(*args: Any, **kwargs: Any) -> Any:
            return self._wait(found(*args, **kwargs))

        return call

    def __enter__(self) -> Blocking:
        self._wait(self._gateway.__aenter__())
        return self

    def __exit__(self, *exc: object) -> None:
        self._wait(self._gateway.__aexit__(*exc))

    def _wait(self, result: Any) -> Any:
        if inspect.iscoroutine(result):
            return self._runner.run(cast(Coroutine[Any, Any, Any], result))
        return result


Connect = Callable[..., ContentGateway]


@pytest.fixture
def fake() -> FakeGateway:
    return FakeGateway()


@pytest.fixture(params=["sync", "async"])
def connect(request: pytest.FixtureRequest, fake: FakeGateway) -> Iterator[Connect]:
    """Makes a `ContentGateway`, or an `AsyncContentGateway` behind `Blocking`, on the fake unless `own_client` is set."""
    with asyncio.Runner() as runner:

        def connect(*, own_client: bool = False, **options: Any) -> ContentGateway:
            if request.param == "sync":
                client = None if own_client else fake.client()
                return ContentGateway(BASE_URL + "/", token=TOKEN, client=client, **options)
            async_client = None if own_client else fake.async_client()
            gateway = AsyncContentGateway(
                BASE_URL + "/", token=TOKEN, client=async_client, **options
            )
            return cast(ContentGateway, Blocking(gateway, runner))

        yield connect


@pytest.fixture
def gateway(connect: Connect) -> ContentGateway:
    return connect(pool="app")


@pytest.fixture
def no_sleep(monkeypatch: pytest.MonkeyPatch) -> list[float]:
    slept: list[float] = []

    async def sleep(seconds: float) -> None:
        slept.append(seconds)

    monkeypatch.setattr(time, "sleep", slept.append)
    monkeypatch.setattr(asyncio, "sleep", sleep)
    return slept


def public(cls: type) -> set[str]:
    return {name for name in dir(cls) if not name.startswith("_")}


def test_the_async_class_mirrors_the_sync_one() -> None:
    assert public(AsyncContentGateway) == public(ContentGateway) - {"close"} | {"aclose"}
    for name in public(ContentGateway):
        sync = getattr(ContentGateway, name)
        twin = getattr(AsyncContentGateway, "aclose" if name == "close" else name)
        assert inspect.iscoroutinefunction(twin), name
        assert inspect.signature(twin) == inspect.signature(sync), name
        overloads = [inspect.signature(o) for o in get_overloads(sync)]
        assert [inspect.signature(o) for o in get_overloads(twin)] == overloads, name
        if name != "close":
            assert twin.__doc__ == sync.__doc__, name


def test_health_needs_no_token(fake: FakeGateway, gateway: ContentGateway) -> None:
    fake.reply(200, {"status": "ok", "version": "0.1.1", "commit": None, "builtAt": None})
    health = gateway.health()
    assert health["version"] == "0.1.1"
    assert str(fake.last.url) == f"{BASE_URL}/v1/healthz"
    assert "authorization" not in fake.last.headers
    assert fake.last.headers["accept"] == "application/json"


def test_budget_sends_the_token_the_pool_and_the_days(
    fake: FakeGateway, gateway: ContentGateway
) -> None:
    fake.reply(200, {"historyDays": 90, "source": "memory"})
    snapshot = gateway.budget(days=30)
    assert snapshot["historyDays"] == 90
    assert str(fake.last.url) == f"{BASE_URL}/v1/budget?days=30"
    assert fake.last.headers["authorization"] == f"Bearer {TOKEN}"
    assert fake.last.headers["x-budget-pool"] == "app"


def test_curseforge_get_passes_the_query_and_reads_the_cache_status(
    fake: FakeGateway, gateway: ContentGateway
) -> None:
    fake.reply(200, {"data": [{"id": 1}]}, headers={"x-cache": "hit"})
    answer = gateway.curseforge(
        "/mods/search", query={"gameId": 432, "index": None, "sortOrder": "desc", "x": True}
    )
    assert answer.ok is True
    assert answer.status == 200
    assert answer.cache == "hit"
    assert answer.body == {"data": [{"id": 1}]}
    assert fake.last.method == "GET"
    assert (
        str(fake.last.url)
        == f"{BASE_URL}/v1/curseforge/mods/search?gameId=432&sortOrder=desc&x=true"
    )
    assert "content-type" not in fake.last.headers


def test_a_body_makes_the_request_a_post(fake: FakeGateway, gateway: ContentGateway) -> None:
    fake.reply(200, {"data": []}, headers={"x-cache": "miss"})
    gateway.curseforge("mods", body={"modIds": [1, 2]})
    assert fake.last.method == "POST"
    assert fake.last.headers["content-type"] == "application/json"
    assert json.loads(fake.last.content) == {"modIds": [1, 2]}


def test_a_get_cannot_carry_a_body(gateway: ContentGateway) -> None:
    with pytest.raises(TypeError, match="cannot carry a body"):
        gateway.curseforge("mods", method="GET", body={"modIds": [1]})


def test_fresh_and_pool_override_the_headers(fake: FakeGateway, gateway: ContentGateway) -> None:
    fake.reply(200, {"data": []}, headers={"x-cache": "miss"})
    gateway.curseforge("categories", fresh=True, pool="jobs")
    assert fake.last.headers["cache-control"] == "no-cache"
    assert fake.last.headers["x-budget-pool"] == "jobs"


def test_an_upstream_error_comes_back_as_an_answer(
    fake: FakeGateway, gateway: ContentGateway
) -> None:
    fake.reply(404, headers={"x-cache": "miss"})
    answer = gateway.curseforge("mods/0")
    assert answer.ok is False
    assert answer.status == 404
    assert answer.body is None


def test_a_non_json_body_is_the_text(fake: FakeGateway, gateway: ContentGateway) -> None:
    fake.reply(
        200, text="<p>changelog</p>", headers={"x-cache": "hit", "content-type": "text/html"}
    )
    answer = gateway.curseforge("mods/1/files/2/changelog")
    assert answer.body == "<p>changelog</p>"
    fake.reply(200, text="not json", headers={"x-cache": "hit", "content-type": "application/json"})
    assert gateway.curseforge("mods/1").body == "not json"


def test_the_gateways_own_refusal_raises(fake: FakeGateway, gateway: ContentGateway) -> None:
    fake.reply(
        503,
        {"code": "BUDGET_SPENT", "message": "spent", "pool": "app", "retryAfterMs": 4000},
        headers={"retry-after": "4"},
    )
    with pytest.raises(ContentGatewayError) as raised:
        gateway.curseforge("mods/1")
    assert raised.value.status == 503
    assert raised.value.code == "BUDGET_SPENT"
    assert raised.value.pool == "app"
    assert raised.value.retry_after_ms == 4000
    assert raised.value.retry_after == 4.0
    assert str(raised.value) == "spent"


def test_the_error_survives_pickling_and_copying() -> None:
    error = ContentGatewayError(
        503, {"code": "BUDGET_SPENT", "message": "spent", "pool": "app", "retryAfterMs": 4000}, ""
    )
    for twin in (pickle.loads(pickle.dumps(error)), copy.copy(error)):
        assert isinstance(twin, ContentGatewayError)
        assert (twin.status, twin.code, twin.pool, twin.retry_after_ms) == (
            503,
            "BUDGET_SPENT",
            "app",
            4000,
        )
        assert str(twin) == "spent"
    assert str(ContentGatewayError(502, None, "fallback")) == "fallback"


def test_an_answer_that_did_not_come_through_the_gateway_raises(
    fake: FakeGateway, gateway: ContentGateway
) -> None:
    fake.reply(200, text="<html>proxy</html>", headers={"content-type": "text/html"})
    with pytest.raises(ContentGatewayError) as raised:
        gateway.curseforge("mods/1")
    assert raised.value.code is None
    assert raised.value.status == 200
    assert "no X-Cache header" in str(raised.value)


def test_an_unknown_cache_status_is_named(fake: FakeGateway, gateway: ContentGateway) -> None:
    fake.reply(200, {"data": []}, headers={"x-cache": "warm"})
    with pytest.raises(ContentGatewayError, match='X-Cache "warm" is not one the gateway sends'):
        gateway.curseforge("mods/1")


def test_an_own_route_raises_on_a_success_that_is_not_its_json(
    fake: FakeGateway, gateway: ContentGateway
) -> None:
    fake.reply(200, text="<html>another service</html>", headers={"content-type": "text/html"})
    fake.reply(200)
    fake.reply(200, [1, 2])
    fake.reply(200, {"loader": "fabric", "versions": []}, headers={"x-cache": "warm"})
    calls: list[Callable[[], object]] = [
        gateway.health,
        gateway.budget,
        gateway.minecraft_versions,
        lambda: gateway.loader_versions("fabric"),
    ]
    for call in calls:
        with pytest.raises(ContentGatewayError) as raised:
            call()
        assert raised.value.status == 200
        assert raised.value.code is None
        assert "is not the gateway's JSON" in str(raised.value)


def test_resolve_raises_the_curseforge_failure_it_passes_on(
    fake: FakeGateway, gateway: ContentGateway
) -> None:
    fake.reply(
        500,
        {"code": "NOT_FOUND", "message": "CurseForge says no"},
        headers={"x-cache": "miss"},
    )
    with pytest.raises(ContentGatewayError) as raised:
        gateway.resolve([1])
    assert raised.value.status == 500
    assert raised.value.code is None
    assert str(raised.value) == (
        "the 500 answer to POST /v1/resolve is an upstream's, passed on by the gateway (X-Cache: miss)"
    )


def test_retry_after_fills_the_error_when_the_body_says_nothing(
    fake: FakeGateway, gateway: ContentGateway
) -> None:
    fake.reply(503, headers={"retry-after": "7"})
    with pytest.raises(ContentGatewayError) as raised:
        gateway.curseforge("mods/1")
    assert raised.value.code is None
    assert raised.value.retry_after_ms == 7000
    assert ContentGatewayError(502, None, "fallback").retry_after is None


def test_a_bad_gateway_page_raises_with_its_text(
    fake: FakeGateway, gateway: ContentGateway
) -> None:
    fake.reply(502, text="Bad Gateway", headers={"content-type": "text/plain"})
    with pytest.raises(
        ContentGatewayError, match="answered 502 to GET /v1/curseforge/mods/1: Bad Gateway"
    ):
        gateway.curseforge("mods/1")


def test_wait_up_to_sleeps_out_a_refusal_that_fits(
    fake: FakeGateway, gateway: ContentGateway, no_sleep: list[float]
) -> None:
    fake.reply(503, {"code": "UPSTREAM_HOLD", "message": "held", "retryAfterMs": 1500})
    fake.reply(503, {"code": "BUDGET_SPENT", "message": "spent"}, headers={"retry-after": "2"})
    fake.reply(200, {"data": []}, headers={"x-cache": "miss"})
    answer = gateway.curseforge("mods", body={"modIds": [1]}, wait_up_to=10)
    assert answer.ok is True
    assert len(fake.requests) == 3
    assert no_sleep == [1.5, 2.0]


def test_wait_up_to_raises_a_refusal_that_does_not_fit(
    fake: FakeGateway, gateway: ContentGateway, no_sleep: list[float]
) -> None:
    fake.reply(503, {"code": "BUDGET_SPENT", "message": "spent", "retryAfterMs": 30_000})
    with pytest.raises(ContentGatewayError) as raised:
        gateway.curseforge("mods/1", wait_up_to=5)
    assert raised.value.code == "BUDGET_SPENT"
    assert no_sleep == []
    assert len(fake.requests) == 1


def test_modrinth_lookups(fake: FakeGateway, gateway: ContentGateway) -> None:
    fake.reply(200, {"id": "v"}, headers={"x-cache": "hit"})
    gateway.modrinth_version_file("ABC/DEF")
    assert str(fake.last.url) == f"{BASE_URL}/v1/modrinth/version_file/ABC%2FDEF"
    assert fake.last.method == "GET"
    fake.reply(200, {}, headers={"x-cache": "miss"})
    gateway.modrinth_version_files(["a", "b"])
    assert str(fake.last.url) == f"{BASE_URL}/v1/modrinth/version_files"
    assert json.loads(fake.last.content) == {"hashes": ["a", "b"], "algorithm": "sha1"}


def test_resolve_posts_the_ids_and_waits_longer(fake: FakeGateway, gateway: ContentGateway) -> None:
    fake.reply(200, {"files": [{"fileId": 1, "retry": False}]})
    resolved = gateway.resolve([1, 2], cdn=False)
    assert resolved["files"][0]["fileId"] == 1
    assert json.loads(fake.last.content) == {"fileIds": [1, 2], "cdn": False}
    assert fake.last.extensions["timeout"]["read"] == RESOLVE_TIMEOUT
    fake.reply(200, {"files": []})
    gateway.resolve([3])
    assert json.loads(fake.last.content) == {"fileIds": [3]}


def test_the_default_timeout_is_sent(fake: FakeGateway, gateway: ContentGateway) -> None:
    fake.reply(200, {"status": "ok", "version": "1"})
    gateway.health()
    assert fake.last.extensions["timeout"]["read"] == 15.0


def test_version_routes(fake: FakeGateway, gateway: ContentGateway) -> None:
    fake.reply(200, {"latest": {"release": "1.21.1", "snapshot": "24w33a"}, "versions": []})
    versions = gateway.minecraft_versions(type="all", loader="neoforge")
    assert versions["latest"]["release"] == "1.21.1"
    assert str(fake.last.url) == f"{BASE_URL}/v1/minecraft/versions?type=all&loader=neoforge"
    fake.reply(200, {"id": "1.21.1", "javaMajor": 21})
    gateway.minecraft_version("1.21 pre/1")
    assert str(fake.last.url) == f"{BASE_URL}/v1/minecraft/versions/1.21%20pre%2F1"
    fake.reply(200, {"loader": "fabric", "versions": []})
    gateway.loader_versions("fabric", minecraft="1.21.1")
    assert str(fake.last.url) == f"{BASE_URL}/v1/minecraft/loaders/fabric/versions?minecraft=1.21.1"
    fake.reply(200, {"loader": "forge", "versions": []})
    gateway.loader_versions("forge")
    assert str(fake.last.url) == f"{BASE_URL}/v1/minecraft/loaders/forge/versions"


def test_java_runtime_by_major_or_component(fake: FakeGateway, gateway: ContentGateway) -> None:
    fake.reply(200, {"platform": "linux", "component": "java-runtime-delta", "major": 21})
    runtime = gateway.java_runtime("linux", major=21)
    assert runtime["component"] == "java-runtime-delta"
    assert str(fake.last.url) == f"{BASE_URL}/v1/minecraft/java?platform=linux&major=21"
    fake.reply(200, {"platform": "mac-os-arm64", "component": "jre-legacy", "major": 8})
    gateway.java_runtime("mac-os-arm64", component="jre-legacy")
    assert (
        str(fake.last.url)
        == f"{BASE_URL}/v1/minecraft/java?platform=mac-os-arm64&component=jre-legacy"
    )
    with pytest.raises(ValueError, match="either major or component"):
        gateway.java_runtime("linux")  # type: ignore[call-overload]
    with pytest.raises(ValueError, match="either major or component"):
        gateway.java_runtime("linux", major=21, component="java-runtime-delta")  # type: ignore[call-overload]


def test_unknown_version_raises(fake: FakeGateway, gateway: ContentGateway) -> None:
    fake.reply(404, {"code": "UNKNOWN_VERSION", "message": "no such version"})
    with pytest.raises(ContentGatewayError) as raised:
        gateway.minecraft_version("nope")
    assert raised.value.code == "UNKNOWN_VERSION"


def test_modrinth_file_url_picks_the_matching_file_on_the_cdn() -> None:
    version = {
        "files": [
            {"hashes": {"sha1": "AAA"}, "url": "https://cdn.modrinth.com/data/x/a.jar"},
            {"hashes": {"sha1": "bbb"}, "url": "https://example.com/b.jar"},
            {"hashes": {"sha1": "ccc"}, "url": "http://cdn.modrinth.com/c.jar"},
            {"hashes": {}, "url": "https://cdn.modrinth.com/d.jar"},
            {"hashes": {"sha1": "eee"}, "url": "https://[broken/e.jar"},
            {"hashes": {"sha1": "fff"}, "url": "https://[broken/f.jar"},
            {"hashes": {"sha1": "fff"}, "url": "https://cdn.modrinth.com/f.jar"},
            "not a file",
            {"hashes": "not hashes", "url": "https://cdn.modrinth.com/g.jar"},
        ]
    }
    assert modrinth_file_url(version, "aaa") == "https://cdn.modrinth.com/data/x/a.jar"
    assert modrinth_file_url(version, "bbb") is None
    assert modrinth_file_url(version, "ccc") is None
    assert modrinth_file_url(version, "ddd") is None
    assert modrinth_file_url(version, "eee") is None
    assert modrinth_file_url(version, "fff") == "https://cdn.modrinth.com/f.jar"
    assert modrinth_file_url(version, "ggg") is None
    assert modrinth_file_url({}, "aaa") is None


def test_a_failure_that_never_reached_the_gateway_raises_httpxs_own(
    fake: FakeGateway, gateway: ContentGateway
) -> None:
    fake.fail(httpx.ConnectError("connection refused"))
    with pytest.raises(httpx.ConnectError, match="connection refused"):
        gateway.curseforge("mods/1")


def test_the_client_closes_only_what_it_made(connect: Connect) -> None:
    with connect() as passed:
        pass
    assert passed._client.is_closed is False
    made = connect(own_client=True)
    made.close()
    assert made._client.is_closed is True
